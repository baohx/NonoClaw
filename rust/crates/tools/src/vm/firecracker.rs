//! Firecracker backend (Linux + KVM).
//!
//! Runs firecracker inside a docker "jailer" container. The host has no
//! passwordless sudo (user namespaces disabled), so we cannot use the
//! official jailer or create taps on the host directly. The container gets
//! CAP_NET_ADMIN + /dev/net/tun + /dev/kvm passthrough and runs in the host
//! network namespace: the tap it creates lands on the host, the guest is
//! directly L3-reachable at 172.29.N.2, and the container's NET_ADMIN lets it
//! install the NAT rules the guest needs for outbound internet. Docker
//! supplies the process/cgroup/namespace isolation the official jailer would.
//!
//! Networking: the guest uses plain DHCP (standard cloud-image behaviour).
//! This container runs a dnsmasq on tap0 handing out exactly one lease
//! (172.29.N.2) plus DNS forwarding, and iptables NAT for egress.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::process::Command;

use super::backend::{
    guest_ip_for, host_tap_ip_for, instance_dir, vm_assets_dir, VmInstance, VmSupervisor,
};

/// Container name for an instance (also the stop handle).
pub fn container_name(name: &str) -> String {
    format!("nonoclaw-vm-{name}")
}

/// Docker image with firecracker + iproute2 + iptables (jailer role).
/// Built by `scripts/vm/build-runtime.sh`.
pub const RUNTIME_IMAGE: &str = "nonoclaw-vm-runtime:latest";

pub struct FirecrackerBackend;

#[async_trait::async_trait]
impl VmSupervisor for FirecrackerBackend {
    async fn start(&self, cwd: &Path, inst: &VmInstance) -> Result<(), String> {
        let name = &inst.name;
        let dir = instance_dir(cwd, name);
        let assets = vm_assets_dir();
        let kernel = assets.join("kernel.img");
        let rootfs = dir.join("rootfs.img");
        if !kernel.is_file() {
            return Err(format!(
                "kernel not found at {} — run scripts/vm/build-image.sh first",
                kernel.display()
            ));
        }
        if !rootfs.is_file() {
            return Err(format!(
                "instance rootfs missing at {} — use Vm action=create first",
                rootfs.display()
            ));
        }
        ensure_runtime_image().await?;
        warn_if_forwarding_disabled().await;

        let cid = container_name(name);
        // Fresh start if a stale container exists.
        let _ = Command::new("docker")
            .args(["rm", "-f", &cid])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;

        // Write the firecracker config file into the instance dir
        // (bind-mounted at /vm inside the container).
        let guest_ip = guest_ip_for(inst.slot);
        let tap_ip = host_tap_ip_for(inst.slot);
        // The instance disk is a BARE ext4 filesystem (not a partitioned
        // disk), so the whole virtio-blk device IS the root fs: root=/dev/vda.
        // The minimal Firecracker CI kernel does not scan MBR partition tables,
        // so root=/dev/vda1 would never find the partition node.
        let boot_args = "console=ttyS0 reboot=k panic=1 random.trust_cpu=on root=/dev/vda rw".to_string();
        let initrd_path = "/host-assets/initrd.img".to_string();
        let cfg = serde_json::json!({
            "boot-source": {
                "kernel_image_path": "/host-assets/kernel.img",
                "initrd_path": initrd_path,
                "boot_args": boot_args,
            },
            "drives": [{
                "drive_id": "rootfs",
                "path_on_host": "/vm/rootfs.img",
                "is_root_device": true,
                "is_read_only": false,
            }],
            "network-interfaces": [{
                "iface_id": "eth0",
                "guest_mac": guest_mac_for(inst.slot),
                "host_dev_name": "tap0",
            }],
            "machine-config": {
                "vcpu_count": inst.cpus,
                "mem_size_mib": inst.mem_mb,
                "smt": false,
            },
        });
        fs::write(
            dir.join("vmconfig.json"),
            serde_json::to_string_pretty(&cfg).unwrap(),
        )
        .map_err(|e| format!("write vmconfig.json: {e}"))?;

        let assets_str = assets.to_string_lossy().to_string();
        let dir_str = dir.to_string_lossy().to_string();
        let status = Command::new("docker")
            .args([
                "run", "-d", "--name", &cid,
                // jailer-equivalent hardening:
                "--cap-drop", "ALL",
                "--cap-add", "NET_ADMIN",
                // dnsmasq needs NET_RAW to open the DHCP packet socket and
                // NET_BIND_SERVICE to bind port 67; without them it dies with
                // "failed to bind DHCP server socket: Permission denied".
                "--cap-add", "NET_RAW",
                "--cap-add", "NET_BIND_SERVICE",
                // The instance dir is owned by the host user (uid 1000, mode
                // 644) but this container runs as root (uid 0 = "other"). With
                // ALL caps dropped, root loses its DAC bypass, so opening
                // rootfs.img O_RDWR / dnsmasq.log O_APPEND fails with EPERM.
                // DAC_OVERRIDE restores the file-permission bypass the real
                // firecracker jailer also holds.
                "--cap-add", "DAC_OVERRIDE",
                "--security-opt", "no-new-privileges",
                "--device", "/dev/kvm",
                "--device", "/dev/net/tun",
                // host netns so tap + NAT rules act on the host itself:
                "--network", "host",
                // Mount host assets at /host-assets, NOT /assets: the runtime
                // image bakes firecracker at /assets/bin/firecracker, so mounting
                // over /assets would shadow it ("firecracker: not found", exit 1).
                "-v", &format!("{assets_str}:/host-assets:ro"),
                "-v", &format!("{dir_str}:/vm"),
                RUNTIME_IMAGE,
                // run the entry script through a shell: without `sh -c` docker
                // treats the script as argv[0] and tries to exec a file by that
                // (multi-line) name → "no such file or directory" (exit 127).
                "sh", "-c",
            ])
            .arg(entry_script(&tap_ip, &guest_ip))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("docker run failed: {e} (docker installed & group member?)"))?;
        if !status.status.success() {
            return Err(format!(
                "docker run exited {:?}: {}",
                status.status.code(),
                String::from_utf8_lossy(&status.stderr)
            ));
        }

        wait_guest_ready(&cid, &guest_ip).await
    }

    async fn stop(&self, _cwd: &Path, inst: &VmInstance) -> Result<(), String> {
        let cid = container_name(&inst.name);
        // `docker rm -f` SIGKILLs firecracker → vmm exits, tap auto-closes.
        // The guest disk is virtio-block: SIGKILL risks no more than a power
        // loss (ext4 journal replays on next boot).
        // `docker rm -f` force-removes (SIGKILL) the container; it has no `-t`
        // flag (that belongs to `docker stop`). SIGKILL is safe here: the guest
        // disk is virtio-block, so this is at worst a power loss (ext4 journal
        // replays on next boot).
        let out = Command::new("docker")
            .args(["rm", "-f", &cid])
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).to_string());
        }
        Ok(())
    }
}

/// Script run inside the runtime container: tap up + NAT + exec firecracker.
/// NAT rules are idempotent (guarded by -C checks).
/// The runtime container shares the host netns, so `ip_forward` must be on for
/// guest egress — but /proc/sys is read-only inside the container and the
/// container's stderr only reaches `docker logs`. Check from the host side so a
/// misconfiguration surfaces where the user can see it.
async fn warn_if_forwarding_disabled() {
    let path = "/proc/sys/net/ipv4/ip_forward";
    match tokio::fs::read_to_string(path).await {
        Ok(v) if v.trim() != "1" => tracing::warn!(
            "{path} = {} — microVM guests will have no egress. \
             Run: sudo sysctl -w net.ipv4.ip_forward=1",
            v.trim()
        ),
        _ => {}
    }
}

fn entry_script(tap_ip: &str, guest_ip: &str) -> String {
    format!(
        r#"set -eu
# --network host means tap0 lives in the host netns and survives a crashed
# boot. Delete any stale one first so `tuntap add` can't fail with EBUSY
# (ioctl(TUNSETIFF): Resource busy) on a retry.
ip link delete tap0 2>/dev/null || true
ip tuntap add mode tap dev tap0
ip link set tap0 up
ip addr add {tap_ip}/30 dev tap0
# ip_forward lives in the HOST netns (--network host) and /proc/sys is
# bind-mounted read-only here, so `sysctl -w` failed with EROFS and the old
# `|| true` hid it -- the guest only had egress if the host already had it on.
# Try the write, then fail loudly if it is still off.
sysctl -qw net.ipv4.ip_forward=1 2>/dev/null || true
if [ "$(cat /proc/sys/net/ipv4/ip_forward)" != "1" ]; then
  echo "nonoclaw-vm: WARNING net.ipv4.ip_forward=0 in host netns; guest egress will fail." >&2
  echo "nonoclaw-vm: run 'sysctl -w net.ipv4.ip_forward=1' on the host." >&2
fi
# NAT for guest egress (idempotent; 172.29.0.0/16 is the VM subnet pool)
iptables -t nat -C POSTROUTING -s 172.29.0.0/16 ! -o tap0 -j MASQUERADE 2>/dev/null || \
  iptables -t nat -A POSTROUTING -s 172.29.0.0/16 ! -o tap0 -j MASQUERADE
iptables -C FORWARD -i tap0 -j ACCEPT 2>/dev/null || iptables -A FORWARD -i tap0 -j ACCEPT
iptables -C FORWARD -o tap0 -j ACCEPT 2>/dev/null || iptables -A FORWARD -o tap0 -j ACCEPT
# Transparent-proxy bypass for the guest. Host firewall setups (v2rayA/tproxy)
# install nat PREROUTING rules at `dstnat-5` that REDIRECT every dport-53 packet
# to a loopback-only stub (observed: 127.0.0.1:52353) and tproxy other TCP to a
# loopback proxy port. Packets arriving on tap0 match them, get rewritten to
# 127.0.0.1, and die there -- the guest loses ALL DNS and direct TCP. Those rules
# honour mark 0x80 as "bypass", so tag guest traffic (and our own upstream DNS
# queries, which leave with saddr=tap_ip) at mangle priority, which runs before
# dstnat-5.
iptables -t mangle -C PREROUTING -i tap0 -j MARK --set-mark 0x80 2>/dev/null || \
  iptables -t mangle -A PREROUTING -i tap0 -j MARK --set-mark 0x80
iptables -t mangle -C OUTPUT -s {tap_ip} -j MARK --set-mark 0x80 2>/dev/null || \
  iptables -t mangle -A OUTPUT -s {tap_ip} -j MARK --set-mark 0x80
# DHCP + DNS for the single guest address on this /30
dnsmasq --no-daemon --interface=tap0 --bind-interfaces \
  --except-interface=lo \
  --dhcp-range={guest_ip},{guest_ip},255.255.255.252,12h \
  --dhcp-option=option:router,{tap_ip} \
  --log-facility=/vm/dnsmasq.log &
# API socket on /tmp (container tmpfs), NOT /vm: binding an AF_UNIX socket on
# the bind-mounted host dir fails with EPERM ("Failed to bind and run the HTTP
# server: Permission denied"). The socket is ephemeral per boot, so tmpfs is fine.
exec /assets/bin/firecracker --api-sock /tmp/fc.sock --config-file /vm/vmconfig.json
"#
    )
}

/// Stable guest MAC per slot (locally-administered prefix 52:54:98).
fn guest_mac_for(slot: u16) -> String {
    format!("52:54:98:00:{:02x}:02", slot as u8)
}

/// Build the runtime image if missing.
async fn ensure_runtime_image() -> Result<(), String> {
    let have = Command::new("docker")
        .args(["image", "inspect", RUNTIME_IMAGE])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if have {
        return Ok(());
    }
    let repo = find_repo_root()?;
    let script = repo.join("scripts/vm/build-runtime.sh");
    if !script.is_file() {
        return Err(format!("runtime build script missing: {}", script.display()));
    }
    let status = Command::new("bash")
        .arg(&script)
        .status()
        .await
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("build-runtime.sh exited {status:?}"));
    }
    Ok(())
}

/// Walk up from cwd until we find rust/Cargo.toml (repo root).
fn find_repo_root() -> Result<PathBuf, String> {
    let mut dir = std::env::current_dir().map_err(|e| e.to_string())?;
    loop {
        if dir.join("rust/Cargo.toml").is_file() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err("cannot locate NonoClaw repo root (rust/Cargo.toml)".into());
        }
    }
}

/// Wait until the guest SSH port accepts TCP (boot done).
async fn wait_guest_ready(cid: &str, guest_ip: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect((guest_ip, 22)).await.is_ok() {
            return Ok(());
        }
        let alive = Command::new("docker")
            .args(["inspect", "-f", "{{.State.Running}}", cid])
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
            .unwrap_or(false);
        if !alive {
            let logs = Command::new("docker")
                .args(["logs", "--tail", "40", cid])
                .output()
                .await
                .map(|o| format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                ))
                .unwrap_or_default();
            return Err(format!("VM container exited during boot:\n{logs}"));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err("timeout waiting for guest SSH (120s)".into())
}
