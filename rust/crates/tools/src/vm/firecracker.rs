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
        let guest_ip = guest_ip_for(inst.slot);
        let tap_ip = host_tap_ip_for(inst.slot);
        ensure_forwarding_enabled().await;
        let proxy = detect_host_proxy().await;
        bridge_host_proxy(&tap_ip, proxy).await;
        // NFSv4 mounts the export's own root, so the guest must be handed the
        // full `<tap_ip>:<dir>` spec — not just the server.
        let nfs_spec = export_workspace(&tap_ip, cwd).await;
        // Guest HTTP clients get the bridge (not the host's loopback port). Absent
        // when no proxy runs, in which case the guest uses direct NAT.
        let proxy_arg = match proxy {
            Some(_) => format!(" nonoclaw.proxy={tap_ip}:{PROXY_BRIDGE_PORT}"),
            None => String::new(),
        };
        // Empty when the export failed, so the guest simply skips /workspace.
        let nfs_arg = if nfs_spec.is_empty() {
            String::new()
        } else {
            format!(" nonoclaw.nfsserver={nfs_spec}")
        };

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
        // The instance disk is a BARE ext4 filesystem (not a partitioned
        // disk), so the whole virtio-blk device IS the root fs: root=/dev/vda.
        // The minimal Firecracker CI kernel does not scan MBR partition tables,
        // so root=/dev/vda1 would never find the partition node.
        let boot_args = format!(
            "console=ttyS0 reboot=k panic=1 random.trust_cpu=on root=/dev/vda rw\
             {nfs_arg}{proxy_arg}"
        );
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

/// Mount point inside the guest for the host project directory.
pub(crate) const WORKSPACE_MOUNT: &str = "/workspace";

/// Export the host project directory over NFS so the guest sees the same files.
///
/// Firecracker has no virtio-fs and no 9p, so the only way to share a directory
/// with the guest is over the network. NFS is the one option the stock kernel
/// already supports (nfs + nfs4 are built in; `mount.nfs` is baked into the
/// image), and it rides the tap device we already set up.
///
/// `/etc/exports` is root-owned and `exportfs` is not in the passwordless sudo
/// allow-list, so the rewrite is done through a privileged container pid-namespaced
/// into the host — the same route the VM itself uses.
async fn export_workspace(tap_ip: &str, cwd: &Path) -> String {
    let Ok(dir) = cwd.canonicalize() else {
        tracing::warn!("cannot resolve project dir {cwd:?}; guest /workspace will be empty");
        return String::new();
    };
    let dir = dir.to_string_lossy().to_string();
    // Host part of the tap /30 (172.29.N.1) -> scope to this VM's subnet.
    let subnet = tap_ip.rsplit_once('.').map(|(s, _)| format!("{s}.0/30")).unwrap_or_default();
    if subnet.is_empty() {
        return String::new();
    }
    // NFSv4 mounts the export's own root, not the server's filesystem root, so
    // the guest spec is `<tap_ip>:<dir>` and the mount lands exactly on /workspace.
    let spec = format!("{tap_ip}:{dir}");
    let line = format!("{dir} {subnet}(rw,sync,no_subtree_check,insecure,all_squash,anonuid=1000,anongid=1000)");
    let script = format!(
        r#"apk add --no-cache nfs-utils >/dev/null 2>&1
if ! grep -qxF '{line}' /hostetc/exports 2>/dev/null; then
  grep -v '^{dir} ' /hostetc/exports > /tmp/e 2>/dev/null || true
  printf '%s\n' "{line}" >> /tmp/e
  cp /tmp/e /hostetc/exports
fi
nsenter -t 1 -m -n exportfs -ra 2>&1 || echo EXPORTFS_FAILED"#
    );
    let out = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--privileged",
            "--pid",
            "host",
            "-v",
            "/etc:/hostetc",
            "alpine",
            "sh",
            "-c",
            &script,
        ])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() && !String::from_utf8_lossy(&o.stdout).contains("EXPORTFS_FAILED") => {
            tracing::debug!("exported {dir} to {subnet} for guest {WORKSPACE_MOUNT}");
            spec
        }
        Ok(o) => {
            tracing::warn!(
                "NFS export of {dir} failed: {}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            String::new()
        }
        Err(e) => {
            tracing::warn!("NFS export of {dir} failed: {e}");
            String::new()
        }
    }
}

/// Fixed port the host-side proxy bridge listens on (tap_ip side).
pub(crate) const PROXY_BRIDGE_PORT: u16 = 20_172;

/// Republish a loopback-only proxy on the tap address so the guest can use it.
///
/// Transparent-proxy firewalls (v2rayA) bind their inbound on 127.0.0.1 and
/// redirect only loopback traffic, so they never see tap0 packets — the guest is
/// left with no egress at all, and sites that need the proxy stay unreachable.
/// `socat` re-publishes the proxy at `tap_ip:20_172`; the guest points its HTTP
/// client there. When no proxy is running the bridge is torn down and the guest
/// uses plain NAT, so behaviour follows whatever the host is configured for.
async fn bridge_host_proxy(tap_ip: &str, proxy: Option<u16>) {
    // Kill any bridge left over from a previous boot; its target port may differ.
    let _ = Command::new("pkill")
        .args(["-f", &format!("TCP-LISTEN:{PROXY_BRIDGE_PORT},bind={tap_ip}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;

    let Some(port) = proxy else {
        tracing::debug!("no host proxy detected; guest uses direct NAT");
        return;
    };
    if !crate::vm::backend::have("socat").await {
        tracing::warn!("host proxy on 127.0.0.1:{port} found but socat is missing; guest stays direct");
        return;
    }
    // Detached: the bridge must outlive this call, and the guest container is
    // stopped separately.
    let spawned = Command::new("socat")
        .args([
            &format!("TCP-LISTEN:{PROXY_BRIDGE_PORT},bind={tap_ip},fork,reuseaddr"),
            &format!("TCP:127.0.0.1:{port}"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(_) => tracing::info!("bridged host proxy 127.0.0.1:{port} -> {tap_ip}:{PROXY_BRIDGE_PORT}"),
        Err(e) => tracing::warn!("failed to bridge host proxy: {e}"),
    }
}

/// The runtime container shares the host netns, so `ip_forward` must be on for
/// guest egress — but /proc/sys is read-only inside the container and the
/// container's stderr only reaches `docker logs`. Ensure it from the host side
/// and repair it when we can: the setting is not persisted across reboots by
/// default (`/etc/sysctl.conf` ships it commented out), so a guest that worked
/// yesterday is silently offline today.
async fn ensure_forwarding_enabled() {
    const PATH: &str = "/proc/sys/net/ipv4/ip_forward";
    if matches!(tokio::fs::read_to_string(PATH).await, Ok(v) if v.trim() == "1") {
        return;
    }
    match Command::new("sudo")
        .args(["-n", "sysctl", "-qw", "net.ipv4.ip_forward=1"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => tracing::info!("enabled net.ipv4.ip_forward for microVM egress"),
        _ => tracing::warn!(
            "{PATH} is not 1 — microVM guests will have no egress. \
             Run: sudo sysctl -w net.ipv4.ip_forward=1"
        ),
    }
}

/// Port a local HTTP/SOCKS proxy listens on, if the host runs one.
///
/// Guests cannot reach a loopback-bound proxy directly, and the usual
/// transparent-proxy setups (v2rayA/tproxy) redirect *loopback* traffic only, so
/// they never see packets arriving from tap0 — without a bridge a guest has no
/// egress at all while such a firewall is active. Bridging is also what makes
/// blocked sites (pypi, raw.githubusercontent) reachable, so the guest follows
/// whatever the host is configured to use: proxy when one is up, plain NAT when
/// it is not.
async fn detect_host_proxy() -> Option<u16> {
    // Loopback HTTP proxies, most-common-conventional first.
    for port in [20171, 20170, 7890, 10809, 8889] {
        let ok = Command::new("curl")
            .args([
                "-sS",
                "-o",
                "/dev/null",
                "--max-time",
                "2",
                "-x",
                &format!("http://127.0.0.1:{port}"),
                "http://www.gstatic.com/generate_204",
            ])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some(port);
        }
    }
    None
}

/// Script run inside the runtime container: tap up + NAT + exec firecracker.
/// NAT rules are idempotent (guarded by -C checks).
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
# to a loopback-only stub (observed: 127.0.0.1:52353) and redirect other TCP to a
# loopback port. Packets arriving on tap0 match them, get rewritten to 127.0.0.1,
# and die there -- the guest loses ALL DNS and direct TCP. Those rules honour mark
# 0x80 as "bypass", so tag guest traffic (and dnsmasq's own upstream queries,
# which leave with saddr=tap_ip) at mangle priority, which runs before dstnat-5.
# Guest traffic therefore goes direct by default; when the host runs a proxy the
# guest is pointed at the bridge instead (see HOST_PROXY in vmconfig/guest env).
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
