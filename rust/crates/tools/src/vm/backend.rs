//! VM instance state, settings, and the `VmBackend` abstraction.
//!
//! Linux + KVM  → Firecracker backend (run inside a docker "jailer" container:
//!                cap-drop ALL + NET_ADMIN, /dev/kvm + /dev/net/tun passthrough,
//!                host network ns so the tap device lands on the host).
//! macOS / no KVM → QEMU backend (Hypervisor.framework accel, user-mode net +
//!                hostfwd; works fully unprivileged).
//!
//! Instance directories under the per-project NonoClaw data root:
//!   rootfs.img   — the VM's persistent disk (ext4 raw, grows with use)
//!   state.json   — instance metadata (state, ports, created_at, …)
//!   fc.sock      — firecracker API socket (when running)
//!   console.log  — serial console capture
//!
//! Firecracker exports the project tree over NFS, so private state must not
//! live there: every guest could otherwise modify every other instance. Only
//! the screenshot spool remains in `<cwd>/.nonoclaw/vm`. SSH keys are global
//! assets, outside both locations.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex as StdMutex;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Home of all VM instances for a project.
///
/// Lives under the per-project NonoClaw data root, not `<cwd>/.nonoclaw/vm`:
/// Firecracker NFS-exports the project tree at `/workspace`, so private VM
/// state inside it would be readable/writable by every guest (conflict B).
/// The key mirrors session storage so distinct cwd paths do not collide.
pub fn vm_root(cwd: &Path) -> PathBuf {
    project_key(cwd).join("vm")
}

/// Guest-visible files only (e.g. screenshots downloaded for the user).
/// Unlike private VM state this stays inside the project tree, where Read
/// resolves it without relaxing the project path boundary.
pub fn vm_shared_root(cwd: &Path) -> PathBuf {
    cwd.join(".nonoclaw/vm")
}

fn project_key(cwd: &Path) -> PathBuf {
    project_key_with_roots(
        cwd,
        nonoclaw_core::nonoclaw_data_dir(),
        nonoclaw_core::home_dir(),
    )
}

fn project_key_with_roots(
    cwd: &Path,
    configured: Option<PathBuf>,
    home: Option<PathBuf>,
) -> PathBuf {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let configured = configured.map(|dir| {
        if dir.is_relative() {
            canonical.join(dir)
        } else {
            dir
        }
    });
    let home = home.filter(|home| home.is_absolute());
    let key = canonical
        .to_string_lossy()
        .trim_start_matches(['/', '\\'])
        .replace(['/', '\\', ':'], "-");
    // Portable mode may point NONOCLAW_HOME at <cwd>/.nonoclaw. That is valid
    // for sessions, but cannot hold VM secrets because the cwd is NFS-exported.
    let root = match configured {
        Some(dir) if dir == canonical || !dir.starts_with(&canonical) => dir,
        _ => home
            .filter(|home| !home.starts_with(&canonical))
            .map(|home| home.join(".nonoclaw"))
            .unwrap_or_else(|| std::env::temp_dir().join("nonoclaw")),
    };
    root.join("projects").join(key)
}

/// Convenience alias: instances root == vm_root.
pub fn instances_dir(cwd: &Path) -> PathBuf {
    vm_root(cwd)
}

/// Directory for a specific instance. A legacy instance stays discoverable
/// until it is stopped and migrated by `start_instance`.
pub fn instance_dir(cwd: &Path, name: &str) -> PathBuf {
    let private = vm_root(cwd).join(name);
    let legacy = legacy_instance_dir(cwd, name);
    if !private.join("state.json").is_file() && legacy.join("state.json").is_file() {
        legacy
    } else {
        private
    }
}

/// Legacy instance directory, used only to find and migrate pre-relocation VMs.
pub fn legacy_instance_dir(cwd: &Path, name: &str) -> PathBuf {
    vm_shared_root(cwd).join(name)
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
    /// Host port for the x11vnc human-viewer. Firecracker: always 5900 (reached
    /// directly at the guest IP over the tap net). QEMU: a free host port
    /// hostfwd-ed to guest 5900. 0 until the VM is started.
    #[serde(default)]
    pub vnc_port: u16,
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
    /// Where tasks execute by default: "vm" (run in the microVM, the default)
    /// or "host" (run directly on the host machine). Routing is advisory — it
    /// is injected into the system prompt as policy, not hard-enforced; host
    /// tools stay available and host write operations still require approval.
    pub default_execution: Option<String>,
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

/// All currently known instances (dirs under either VM root with a state.json).
pub fn scan_instances(cwd: &Path) -> Vec<VmInstance> {
    let mut out = Vec::new();
    let roots = [vm_root(cwd), vm_shared_root(cwd)];
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !seen.insert(name) {
                continue;
            }
            let p = entry.path().join("state.json");
            let Ok(raw) = fs::read_to_string(&p) else {
                continue;
            };
            match serde_json::from_str::<VmInstance>(&raw) {
                Ok(mut inst) => {
                    // Reconcile with reality: container/VM process may have died.
                    if inst.state == VmState::Running || inst.state == VmState::Starting {
                        if !process_alive(cwd, &inst) {
                            inst.state = VmState::Stopped;
                        }
                    }
                    out.push(inst);
                }
                Err(_) => continue,
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Whether the supervisor process for this instance is still alive.
/// Firecracker: the `nonoclaw-vm-<name>` docker container must be running.
/// QEMU: the PID recorded in `<instance dir>/qemu.pid` must be alive.
/// `kill -0` checks liveness portably (macOS has no /proc).
/// State on disk can be stale (e.g. host reboot SIGKILLs the VM), so
/// callers use this to reconcile `Running`/`Starting` → `Stopped`.
fn process_alive(cwd: &Path, inst: &VmInstance) -> bool {
    match inst.backend {
        VmBackend::Firecracker => {
            let cid = crate::vm::firecracker::container_name(&inst.name);
            std::process::Command::new("docker")
                .args(["inspect", "-f", "{{.State.Running}}", &cid])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
                .unwrap_or(false)
        }
        VmBackend::Qemu => {
            let Ok(pid) = fs::read_to_string(instance_dir(cwd, &inst.name).join("qemu.pid")) else {
                return false;
            };
            let pid = pid.trim();
            !pid.is_empty()
                && std::process::Command::new("kill")
                    .args(["-0", pid])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
        }
    }
}

pub fn load_instance(cwd: &Path, name: &str) -> Option<VmInstance> {
    let p = instance_dir(cwd, name).join("state.json");
    let raw = fs::read_to_string(p).ok()?;
    let mut inst: VmInstance = serde_json::from_str(&raw).ok()?;
    // Reconcile with reality: a `Running`/`Starting` record survives host
    // reboots and SIGKILLed containers. Without this, `ensure`/prewarm
    // short-circuit on stale state and the VM never comes back up.
    if inst.state == VmState::Running || inst.state == VmState::Starting {
        if !process_alive(cwd, &inst) {
            inst.state = VmState::Stopped;
        }
    }
    Some(inst)
}

pub fn save_instance(cwd: &Path, inst: &VmInstance) -> std::io::Result<()> {
    let dir = instance_dir(cwd, &inst.name);
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("state.json"),
        serde_json::to_string_pretty(inst).unwrap_or_default(),
    )
}

/// Move a stopped legacy instance out of the NFS-exported project tree.
/// Running instances stay at their stable path so stop/SSH remain usable.
pub fn migrate_legacy_instance(cwd: &Path, inst: &VmInstance) -> std::io::Result<()> {
    let legacy = legacy_instance_dir(cwd, &inst.name);
    let private = vm_root(cwd).join(&inst.name);
    if !legacy.join("state.json").is_file() {
        return Ok(());
    }
    if inst.state != VmState::Stopped || process_alive(cwd, inst) {
        return Err(std::io::Error::other(
            "stop the VM before migrating its private state",
        ));
    }
    if private.exists() {
        return Err(std::io::Error::other(format!(
            "both legacy and private state exist for '{}'; refusing to choose a disk: {} and {}",
            inst.name,
            legacy.display(),
            private.display()
        )));
    }
    migrate_instance_files(inst, &legacy, &private)
}

fn migrate_instance_files(inst: &VmInstance, legacy: &Path, private: &Path) -> std::io::Result<()> {
    if !legacy.join("state.json").is_file() {
        return Ok(());
    }
    if inst.state != VmState::Stopped {
        return Err(std::io::Error::other(
            "stop the VM before migrating its private state",
        ));
    }
    if private.exists() {
        return Err(std::io::Error::other(format!(
            "both legacy and private state exist for '{}'; refusing to choose a disk: {} and {}",
            inst.name,
            legacy.display(),
            private.display()
        )));
    }
    let parent = private
        .parent()
        .ok_or_else(|| std::io::Error::other("private VM instance path has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".{}.migrating", inst.name));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir(&staging)?;
    if let Err(error) = copy_instance_files(legacy, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    fs::rename(&staging, private)?;
    // Activation is complete: never roll back the new copy during cleanup.
    // Remove the old state marker first so a partial cleanup cannot make the
    // legacy instance authoritative again. The shared spool stays in place.
    fs::remove_file(legacy.join("state.json"))?;
    for entry in fs::read_dir(legacy)? {
        let entry = entry?;
        if entry.file_name() != "spool" {
            if entry.file_type()?.is_dir() {
                let _ = fs::remove_dir_all(entry.path());
            } else {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

/// Copy private state without following links placed inside the exported tree.
fn copy_instance_files(source: &Path, target: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "spool" {
            continue;
        }
        let kind = entry.file_type()?;
        let destination = target.join(&name);
        if kind.is_dir() {
            fs::create_dir(&destination)?;
            copy_instance_files(&entry.path(), &destination)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), destination)?;
        } else {
            return Err(std::io::Error::other(format!(
                "refusing to migrate non-regular file: {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_root_escapes_an_exported_portable_data_dir() {
        let project = Path::new("/workspace/project");
        let home = PathBuf::from("/home/tester");
        let key =
            project_key_with_roots(project, Some(project.join(".nonoclaw")), Some(home.clone()));
        assert_eq!(key, home.join(".nonoclaw/projects/workspace-project"));
        assert!(!key.starts_with(project));
        assert_eq!(
            project_key_with_roots(
                project,
                Some(PathBuf::from(".nonoclaw")),
                Some(home.clone()),
            ),
            key
        );

        let external = PathBuf::from("/var/lib/nonoclaw");
        assert_eq!(
            project_key_with_roots(project, Some(external.clone()), Some(home)),
            external.join("projects/workspace-project")
        );
    }

    fn test_instance(state: VmState) -> VmInstance {
        VmInstance {
            name: "legacy".into(),
            backend: VmBackend::Qemu,
            state,
            slot: 1,
            cpus: 2,
            mem_mb: 512,
            disk_mb: 1024,
            image: "base".into(),
            created_at: 0,
            updated_at: 0,
            ssh_port: 2222,
            cdp_port: 0,
            vnc_port: 0,
        }
    }

    #[test]
    fn stopped_legacy_instance_moves_private_files_but_keeps_spool() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("project/.nonoclaw/vm/legacy");
        let private = temp.path().join("data/vm/legacy");
        fs::create_dir_all(legacy.join("spool")).unwrap();
        fs::write(legacy.join("state.json"), "{}").unwrap();
        fs::write(legacy.join("rootfs.img"), "disk").unwrap();
        fs::write(legacy.join("known_hosts"), "host-key").unwrap();
        fs::write(legacy.join("spool/screenshot.png"), "image").unwrap();

        migrate_instance_files(&test_instance(VmState::Stopped), &legacy, &private).unwrap();

        assert_eq!(fs::read(private.join("rootfs.img")).unwrap(), b"disk");
        assert_eq!(fs::read(private.join("known_hosts")).unwrap(), b"host-key");
        assert!(private.join("state.json").is_file());
        assert!(!legacy.join("state.json").exists());
        assert_eq!(
            fs::read(legacy.join("spool/screenshot.png")).unwrap(),
            b"image"
        );
        assert!(!private.join("spool").exists());
    }

    #[test]
    fn running_legacy_instance_is_never_moved() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("legacy");
        let private = temp.path().join("private");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("state.json"), "{}").unwrap();

        let error = migrate_instance_files(&test_instance(VmState::Running), &legacy, &private)
            .unwrap_err();
        assert!(error.to_string().contains("stop the VM"));
        assert!(legacy.join("state.json").is_file());
        assert!(!private.exists());
    }

    #[test]
    fn migration_never_overwrites_an_existing_private_instance() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("legacy");
        let private = temp.path().join("private");
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&private).unwrap();
        fs::write(legacy.join("state.json"), "old").unwrap();
        fs::write(private.join("state.json"), "current").unwrap();

        let error = migrate_instance_files(&test_instance(VmState::Stopped), &legacy, &private)
            .unwrap_err();
        assert!(error.to_string().contains("refusing to choose a disk"));
        assert_eq!(
            fs::read_to_string(private.join("state.json")).unwrap(),
            "current"
        );
        assert_eq!(
            fs::read_to_string(legacy.join("state.json")).unwrap(),
            "old"
        );
    }
}
