//! QEMU backend (macOS + no-KVM Linux fallback).
//!
//! Fully unprivileged: user-mode networking (slirp) + hostfwd for SSH(22) and
//! CDP(9222), no root, no vmnet entitlement needed. Acceleration: `-accel hvf`
//! on macOS aarch64/x86_64, `-accel kvm` if /dev/kvm exists, else TCG.
//! Persistent rootfs is the same ext4 image as the Firecracker backend.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::process::Command;

use super::backend::{instance_dir, vm_assets_dir, VmInstance, VmSupervisor};

pub struct QemuBackend;

#[async_trait::async_trait]
impl VmSupervisor for QemuBackend {
    async fn start(&self, cwd: &Path, inst: &VmInstance) -> Result<(), String> {
        let name = &inst.name;
        let dir = instance_dir(cwd, name);
        let assets = vm_assets_dir();
        // Prefer the Debian distro bzImage (kernel-qemu.img) — it has virtio-pci
        // + PCI + ACPI, which QEMU q35 needs. The FC vmlinux (kernel.img) has
        // none of those and never finds the disk under QEMU. Fall back to
        // kernel.img for older asset sets that predate the split.
        let kernel_qemu = assets.join("kernel-qemu.img");
        let kernel = if kernel_qemu.is_file() {
            kernel_qemu
        } else {
            assets.join("kernel.img")
        };
        let rootfs = dir.join("rootfs.img");
        if !rootfs.is_file() {
            return Err(format!(
                "instance disk missing at {} — use Vm action=create first",
                rootfs.display()
            ));
        }
        let initrd = assets.join("initrd.img");
        if !kernel.is_file() || !initrd.is_file() {
            return Err(format!(
                "kernel/initrd not found under {} — run scripts/vm/build-image.sh (on Linux) first",
                assets.display()
            ));
        }
        // Same assets as the Firecracker backend: direct kernel boot with the
        // bare ext4 rootfs (no partition table — the whole virtio-blk device
        // IS the root fs, so root=/dev/vda; root=/dev/vda1 would never find
        // the partition node). DHCP: slirp serves 10.0.2.15 natively.
        let kernel_args = "root=/dev/vda rw console=ttyS0".to_string();

        // Stop any previous qemu for this instance.
        stop_qemu(&dir).await;

        // User-mode net: guest 10.0.2.15, hostfwd to chosen free ports.
        let ssh_port = inst.ssh_port;
        let cdp_port = inst.cdp_port;
        // VNC human-viewer: hostfwd a free host port to guest 5900 (x11vnc).
        let vnc_port = if inst.vnc_port != 0 { inst.vnc_port } else { 5900 };
        let accel = accel_flag(); // ("-accel", "hvf")
        let mut accel_args: Vec<String> =
            accel.iter().map(|s| s.to_string()).collect();
        accel_args.dedup();
        let bin = qemu_binary()?;
        let mut cmd = Command::new(bin);
        cmd.args([
            "-machine", "q35",
            "-m", &inst.mem_mb.to_string(),
            "-smp", &inst.cpus.to_string(),
        ])
        .args(&accel_args)
        .args([
            "-daemonize",
            "-pidfile", dir.join("qemu.pid").to_string_lossy().as_ref(),
            "-serial", format!("file:{}", dir.join("console.log").display()).as_str(),
            "-drive", &format!("file={},format=raw,if=virtio", rootfs.display()),
            "-netdev", &format!(
                "user,id=n0,hostfwd=tcp:127.0.0.1:{ssh_port}-:22,hostfwd=tcp:127.0.0.1:{cdp_port}-:9222,hostfwd=tcp:127.0.0.1:{vnc_port}-:5900"
            ),
            "-device", "virtio-net-pci,netdev=n0",
        ]);
        cmd.args([
            "-kernel", kernel.to_string_lossy().as_ref(),
            "-initrd", initrd.to_string_lossy().as_ref(),
            "-append", &kernel_args,
        ]);
        let out = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("qemu spawn failed: {e} — brew install qemu / apt install qemu-system-x86"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).to_string());
        }

        // Wait for SSH.
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            if tokio::net::TcpStream::connect(("127.0.0.1", ssh_port)).await.is_ok() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Err("timeout waiting for guest SSH (120s)".into())
    }

    async fn stop(&self, cwd: &Path, inst: &VmInstance) -> Result<(), String> {
        stop_qemu(&instance_dir(cwd, &inst.name)).await;
        Ok(())
    }
}

async fn stop_qemu(dir: &Path) {
    if let Ok(pid) = fs::read_to_string(dir.join("qemu.pid")) {
        let pid = pid.trim();
        if !pid.is_empty() {
            let _ = Command::new("kill")
                .arg(pid)
                .output()
                .await;
            // Wait for exit so a subsequent start doesn't race the old process.
            // kill -0: portable liveness probe (macOS has no /proc).
            for _ in 0..20 {
                if !Command::new("kill")
                    .args(["-0", pid])
                    .output()
                    .await
                    .map(|o| o.status.success())
                    .unwrap_or(false)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    }
    let _ = fs::remove_file(dir.join("qemu.pid"));
}

/// hvf on macOS, kvm if available, else tcg.
fn accel_flag() -> Vec<&'static str> {
    #[cfg(target_os = "macos")]
    {
        vec!["-accel", "hvf"]
    }
    #[cfg(not(target_os = "macos"))]
    {
        if Path::new("/dev/kvm").exists() {
            vec!["-accel", "kvm"]
        } else {
            vec!["-accel", "tcg"]
        }
    }
}

fn qemu_binary() -> Result<PathBuf, String> {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["/opt/homebrew/bin/qemu-system-x86_64", "qemu-system-x86_64"]
    } else {
        &["qemu-system-x86_64"]
    };
    for c in candidates {
        if which_exists(c).unwrap_or(false) {
            return Ok(PathBuf::from(c));
        }
    }
    Err("qemu-system-x86_64 not found (brew install qemu / sudo apt install qemu-system-x86)".into())
}

fn which_exists(cmd: &str) -> Result<bool, String> {
    Ok(std::process::Command::new(cmd)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok())
}

