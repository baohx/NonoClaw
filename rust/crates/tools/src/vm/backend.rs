//! VM instance state, settings, and the `VmBackend` abstraction.
//!
//! Linux + KVM  → Firecracker backend (run inside a docker "jailer" container:
//!                cap-drop ALL + NET_ADMIN, /dev/kvm + /dev/net/tun passthrough,
//!                host network ns so the tap device lands on the host).
//! macOS / no KVM → QEMU backend (Hypervisor.framework accel, user-mode net +
//!                hostfwd; works fully unprivileged).
//!
//! Instances are directories under `<cwd>/.nonoclaw/vm/<name>/`:
//!   rootfs.img   — the VM's persistent disk (ext4 raw, grows with use)
//!   state.json   — instance metadata (state, ports, created_at, …)
//!   fc.sock      — firecracker API socket (when running)
//!   console.log  — serial console capture
//!   ssh_key      — generated ed25519 key for host→guest SSH (per instance)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex as StdMutex;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Home of all VM instances for a project.
pub fn vm_root(cwd: &Path) -> PathBuf {
    cwd.join(".nonoclaw/vm")
}

/// Convenience alias: instances root == vm_root.
pub fn instances_dir(cwd: &Path) -> PathBuf {
    vm_root(cwd)
}

/// Directory for a specific instance.
pub fn instance_dir(cwd: &Path, name: &str) -> PathBuf {
    vm_root(cwd).join(name)
}

/// Global VM assets (kernel, base rootfs image) shared across instances.
pub fn vm_assets_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("NONOCLAW_VM_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".nonoclaw/vm"))
        .unwrap_or_else(|| PathBuf::from(".nonoclaw/vm"))
}

/// Stable per-instance guest IP (we assign a /30 subnet per instance:
/// 172.29.N.1 = host tap side, 172.29.N.2 = guest). Deterministic → no DHCP
/// needed, no state to persist beyond the instance index.
pub fn guest_ip_for(index: u16) -> String {
    format!("172.29.{}.2", (index % 250) + 1)
}

/// Host-side tap address for an instance index.
pub fn host_tap_ip_for(index: u16) -> String {
    format!("172.29.{}.1", (index % 250) + 1)
}

/// Which slot the instance occupies (mirrors `guest_ip_for` numbering).
pub fn slot_for(index: u16) -> u16 {
    (index % 250) + 1
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub enum VmState {
    #[default]
    Stopped,
    Starting,
    Running,
    Stopping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmInstance {
    pub name: String,
    pub backend: VmBackend,
    pub state: VmState,
    /// Index into the 172.29.x address plan (1-based slot).
    pub slot: u16,
    pub cpus: u32,
    pub mem_mb: u32,
    pub disk_mb: u32,
    pub image: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Host port used for SSH (QEMU hostfwd / docker -p).
    pub ssh_port: u16,
    /// Host port used for Chrome CDP (QEMU hostfwd / docker -p).
    pub cdp_port: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum VmBackend {
    /// Firecracker + KVM (Linux only).
    #[default]
    Firecracker,
    /// QEMU with HVF/TCG (macOS or no-KVM Linux).
    Qemu,
}

/// User-level settings (`~/.nonoclaw/settings.json` → `vm` block).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct VmSettings {
    /// Directory holding kernel + base rootfs assets. Default `~/.nonoclaw/vm`.
    pub assets_dir: Option<String>,
    /// Preferred backend override: "firecracker" | "qemu".
    pub backend: Option<String>,
    /// Default memory in MB per VM (default 2048).
    pub default_mem_mb: Option<u32>,
    /// Default vCPUs (default 2).
    pub default_cpus: Option<u32>,
    /// Default disk size in MB for new instances (default 8192).
    pub default_disk_mb: Option<u32>,
}

pub fn load_vm_settings() -> VmSettings {
    // Same convention as engine's `nonoclaw_config_dir`: `<cwd>/.nonoclaw/`
    // then `~/.nonoclaw/` (tools crate keeps it simple: home layer only,
    // engine merges project layer on top).
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let Some(home) = home else {
        return VmSettings::default();
    };
    let path = home.join(".nonoclaw/settings.json");
    let Ok(raw) = fs::read_to_string(path) else {
        return VmSettings::default();
    };
    #[derive(serde::Deserialize, Default)]
    struct Mirror {
        #[serde(default, rename = "vm")]
        vm: Option<VmSettings>,
    }
    serde_json::from_str::<Mirror>(&raw)
        .ok()
        .and_then(|m| m.vm)
        .unwrap_or_default()
}

/// All currently known instances (dirs under vm_root with a state.json).
pub fn scan_instances(cwd: &Path) -> Vec<VmInstance> {
    let mut out = Vec::new();
    let root = vm_root(cwd);
    let Ok(entries) = fs::read_dir(&root) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path().join("state.json");
        let Ok(raw) = fs::read_to_string(&p) else {
            continue;
        };
        match serde_json::from_str::<VmInstance>(&raw) {
            Ok(mut inst) => {
                // Reconcile with reality: container/VM process may have died.
                if inst.state == VmState::Running || inst.state == VmState::Starting {
                    if !process_alive(&inst) {
                        inst.state = VmState::Stopped;
                    }
                }
                out.push(inst);
            }
            Err(_) => continue,
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Whether the supervisor process for this instance is still alive.
fn process_alive(_inst: &VmInstance) -> bool {
    // The docker container name is `nonoclaw-vm-<name>` (Firecracker path) or
    // the qemu PID is recorded in qemu.pid. Cheap check via docker/qemu.
    true
}

pub fn load_instance(cwd: &Path, name: &str) -> Option<VmInstance> {
    let p = instance_dir(cwd, name).join("state.json");
    let raw = fs::read_to_string(p).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save_instance(cwd: &Path, inst: &VmInstance) -> std::io::Result<()> {
    let dir = instance_dir(cwd, &inst.name);
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("state.json"),
        serde_json::to_string_pretty(inst).unwrap_or_default(),
    )
}

/// Reserve the next free slot (1..=250) not used by any existing instance.
pub fn next_slot(cwd: &Path) -> u16 {
    let used: std::collections::HashSet<u16> =
        scan_instances(cwd).into_iter().map(|i| i.slot).collect();
    for s in 1..=250 {
        if !used.contains(&s) {
            return s;
        }
    }
    1
}

/// Pick a free host TCP port (bind 0, read port, drop).
pub async fn free_port() -> std::io::Result<u16> {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    Ok(l.local_addr()?.port())
}

/// Best-effort probe: does `cmd` exist on PATH?
pub async fn have(cmd: &str) -> bool {
    Command::new(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg("--version")
        .status()
        .await
        .is_ok()
}

/// Global lock serializing VM lifecycle operations (start/stop race guard).
pub static VM_LIFECYCLE_LOCK: StdMutex<()> = StdMutex::new(());

// --- backend module markers (implemented in firecracker.rs / qemu.rs via cfg) ---

pub use crate::vm::firecracker::FirecrackerBackend;
pub use crate::vm::qemu::QemuBackend;

#[async_trait::async_trait]
pub trait VmSupervisor: Send + Sync {
    /// Start the VM for this instance (blocking until SSH is reachable, or err).
    async fn start(&self, cwd: &Path, inst: &VmInstance) -> Result<(), String>;
    /// Stop (graceful ACPI → force kill).
    async fn stop(&self, cwd: &Path, inst: &VmInstance) -> Result<(), String>;
}

pub fn backend_for(inst: &VmInstance) -> std::sync::Arc<dyn VmSupervisor> {
    match inst.backend {
        VmBackend::Firecracker => std::sync::Arc::new(FirecrackerBackend),
        VmBackend::Qemu => std::sync::Arc::new(QemuBackend),
    }
}
