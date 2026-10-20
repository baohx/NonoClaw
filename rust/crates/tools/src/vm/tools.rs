//! Agent-facing VM tools: `Vm`, `VmExec`, `VmFs`, `VmBrowser`.
//!
//! These follow the house style of the other builtin tools: struct + const
//! PROMPT + JsonSchema input struct + `call()` dispatch on `action`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};


use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tool::{Tool, ToolCtx, ToolResult};
use nonoclaw_core::{Error, PermissionDecision, Result};

use super::backend::{
    backend_for, free_port, guest_ip_for, instance_dir, load_instance, load_vm_settings, next_slot,
    save_instance, scan_instances, vm_assets_dir, VmBackend, VmInstance, VmState,
};
use super::ssh;

// ---------------------------------------------------------------------------
// Vm — lifecycle
// ---------------------------------------------------------------------------

pub struct VmTool;

#[async_trait]
impl Tool for VmTool {
    fn name(&self) -> &str {
        "Vm"
    }
    fn prompt(&self) -> &str {
        "Manage persistent Linux microVM instances. Actions: list, status, ensure, \
create, start, stop, destroy. Prefer 'ensure' before any VM work — it \
idempotently creates+starts the instance and returns once it's running. \
Input: {\"type\": <action>, \"name\": <instance>, \"image\"?: string}. The \
default instance is named \"default\". The VM's disk survives stop/start and \
across sessions. Linux+KVM uses Firecracker, otherwise QEMU (macOS included)."
    }
    fn description(&self) -> &str {
        "Persistent Linux microVM lifecycle (Firecracker on Linux/KVM, QEMU on macOS)."
    }
    fn snippet(&self) -> String {
        "Persistent Linux microVM lifecycle: create/start/stop/status/list/destroy.".into()
    }
    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "Create the VM once and reuse it across sessions — the disk survives stop/start.",
            "Check status/list before assuming a VM exists; the default instance is \"default\".",
        ]
    }
    fn is_read_only(&self, input: &Value) -> bool {
        matches!(input["type"].as_str(), Some("list") | Some("status"))
    }
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false // lifecycle ops mutate shared state
    }
    fn is_destructive(&self, input: &Value) -> bool {
        matches!(input["type"].as_str(), Some("destroy"))
    }
    async fn check_permissions(&self, _input: &Value, _ctx: &ToolCtx<'_>) -> PermissionDecision {
        PermissionDecision::allow()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "type": {
                    "type": "string",
                    "enum": ["list", "status", "ensure", "create", "start", "stop", "destroy", "viewer"],
                    "description": "Lifecycle action. 'ensure' idempotently creates+starts the VM and returns once it's running (use this before any VM work). 'viewer' returns the VNC address for a human to watch/drive the live browser."
                },
                "name": {
                    "type": "string",
                    "description": "Instance name (default: \"default\")."
                },
                "image": {
                    "type": "string",
                    "description": "Rootfs image path override (rarely needed)."
                }
            },
            "required": ["type"]
        })
    }
    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        _cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let action = input["type"].as_str().unwrap_or("list").to_string();
        let name = input["name"].as_str().unwrap_or("default").to_string();
        let cwd = ctx.cwd;

        match action.as_str() {
            "list" => {
                let insts = scan_instances(cwd);
                if insts.is_empty() {
                    return Ok(ToolResult::ok("No VM instances. Create one with {\"type\":\"create\"}."));
                }
                let mut out = String::new();
                for i in insts {
                    out.push_str(&format!(
                        "{}: state={:?} backend={:?} ip={} ssh_port={} cdp_port={}\n",
                        i.name,
                        i.state,
                        i.backend,
                        guest_ip_for(i.slot),
                        i.ssh_port,
                        i.cdp_port
                    ));
                }
                Ok(ToolResult::ok(out))
            }
            "status" => {
                let Some(inst) = load_instance(cwd, &name) else {
                    return Ok(ToolResult::error(format!("no VM instance named '{name}'")));
                };
                Ok(ToolResult::ok(serde_json::to_string_pretty(&inst).unwrap_or_default()))
            }
            "viewer" => {
                // Human take-over: report the VNC address to watch/drive the
                // live browser (x11vnc mirrors the same Xvfb :99 screen the
                // agent drives via CDP). Firecracker is reached directly at the
                // guest IP; QEMU via the 127.0.0.1 hostfwd port.
                let Some(inst) = load_instance(cwd, &name) else {
                    return Ok(ToolResult::error(format!("no VM instance named '{name}'")));
                };
                if inst.state != VmState::Running {
                    return Ok(ToolResult::error(format!(
                        "'{name}' is not running (state={:?}); start it first",
                        inst.state
                    )));
                }
                let addr = match inst.backend {
                    VmBackend::Firecracker => format!("{}:5900", guest_ip_for(inst.slot)),
                    VmBackend::Qemu => format!("127.0.0.1:{}", inst.vnc_port),
                };
                Ok(ToolResult::ok(format!(
                    "VNC viewer for '{name}': connect any VNC client to {addr}\n\
                     (no password). You share the live browser window + profile \
                     with the agent — log in by hand, then let the agent continue."
                )))
            }
            "create" => {
                // reject if exists (idempotent success if already created)
                if load_instance(cwd, &name).is_some() {
                    return Ok(ToolResult::ok(format!("instance '{name}' already exists")));
                }
                let image = input["image"].as_str().map(str::to_string);
                let (dir, backend) = create_instance(cwd, &name, image)?;
                Ok(ToolResult::ok(format!(
                    "created instance '{name}' (backend={backend:?}) at {}",
                    dir.display()
                )))
            }
            "start" => {
                let inst = load_instance(cwd, &name)
                    .ok_or_else(|| Error::Other(format!("no VM instance named '{name}'")))?;
                if inst.state == VmState::Running {
                    return Ok(ToolResult::ok(format!("'{name}' already running")));
                }
                start_instance(cwd, inst).await
            }
            // Idempotent "make it ready": create the instance if missing, start
            // it if not already running, and return immediately if it is. This
            // is the single entry point used by session prewarm and by any
            // tool that needs the VM up (callers never juggle create/start).
            "ensure" => {
                let image = input["image"].as_str().map(str::to_string);
                let inst = match load_instance(cwd, &name) {
                    Some(i) => i,
                    None => {
                        create_instance(cwd, &name, image)?;
                        load_instance(cwd, &name).ok_or_else(|| {
                            Error::Other(format!("instance '{name}' vanished after create"))
                        })?
                    }
                };
                if inst.state == VmState::Running {
                    return Ok(ToolResult::ok(format!(
                        "'{name}' already running: ip={} ssh={}",
                        guest_ip_for(inst.slot),
                        inst.ssh_port
                    )));
                }
                start_instance(cwd, inst).await
            }
            "stop" => {
                let mut inst = load_instance(cwd, &name)
                    .ok_or_else(|| Error::Other(format!("no VM instance named '{name}'")))?;
                if inst.state == VmState::Running {
                    let sup = backend_for(&inst);
                    sup.stop(cwd, &inst).await.map_err(|e| Error::Other(format!("stop failed: {e}")))?;
                }
                inst.state = VmState::Stopped;
                save_instance(cwd, &inst).map_err(|e| Error::Other(e.to_string()))?;
                Ok(ToolResult::ok(format!("'{name}' stopped")))
            }
            "destroy" => {
                let Some(mut inst) = load_instance(cwd, &name) else {
                    return Ok(ToolResult::error(format!("no VM instance named '{name}'")));
                };
                if inst.state == VmState::Running {
                    let sup = backend_for(&inst);
                    let _ = sup.stop(cwd, &inst).await;
                    inst.state = VmState::Stopped;
                }
                let dir = instance_dir(cwd, &name);
                std::fs::remove_dir_all(&dir).map_err(|e| Error::Other(format!("rm: {e}")))?;
                Ok(ToolResult::ok(format!("destroyed '{name}' ({})", dir.display())))
            }
            other => Err(Error::Other(format!("unknown action '{other}'"))),
        }
    }
}

/// Materialise a new instance: pick backend, copy+grow the base rootfs, write
/// the instance record and SSH key. Returns (instance dir, chosen backend).
/// Shared by the `create` and `ensure` actions.
fn create_instance(
    cwd: &Path,
    name: &str,
    image: Option<String>,
) -> Result<(PathBuf, VmBackend)> {
    let settings = load_vm_settings();
    let backend = pick_backend(&settings);
    let slot = next_slot(cwd);
    let dir = instance_dir(cwd, name);
    std::fs::create_dir_all(&dir).map_err(|e| Error::Other(format!("mkdir: {e}")))?;
    let inst_image = image.unwrap_or_else(|| "rootfs-bare.ext4".into());
    let assets = settings
        .assets_dir
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(vm_assets_dir);
    let base = assets.join(&inst_image);
    let rootfs = dir.join("rootfs.img");
    std::fs::copy(&base, &rootfs).map_err(|e| {
        Error::Other(format!(
            "copy rootfs: {e} (expected at {}; run scripts/vm/build-image.sh)",
            base.display()
        ))
    })?;
    // Sparse-grow the instance disk; the guest's firstboot-growfs service
    // expands p1 + the ext4 fs to match on first boot.
    let target = (settings.default_disk_mb.unwrap_or(8192) as u64) * 1024 * 1024;
    if rootfs.metadata().map(|m| m.len()).unwrap_or(0) < target {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&rootfs)
            .map_err(|e| Error::Other(format!("open rootfs: {e}")))?;
        f.set_len(target)
            .map_err(|e| Error::Other(format!("truncate rootfs: {e}")))?;
    }
    let now = chrono::Utc::now().timestamp();
    let inst = VmInstance {
        name: name.to_string(),
        backend,
        state: VmState::Stopped,
        slot,
        cpus: settings.default_cpus.unwrap_or(2),
        mem_mb: settings.default_mem_mb.unwrap_or(2048),
        disk_mb: settings.default_disk_mb.unwrap_or(8192),
        image: inst_image,
        created_at: now,
        updated_at: now,
        ssh_port: 0,
        cdp_port: 0,
        vnc_port: 0,
    };
    save_instance(cwd, &inst).map_err(|e| Error::Other(e.to_string()))?;
    ssh::ensure_key(cwd, name).map_err(Error::Other)?;
    Ok((dir, backend))
}

/// Allocate ports, boot the backend, and flip the instance to Running (or back
/// to Stopped on failure). Shared by the `start` and `ensure` actions.
async fn start_instance(cwd: &Path, mut inst: VmInstance) -> Result<ToolResult> {
    let name = inst.name.clone();
    inst.state = VmState::Starting;
    inst.ssh_port = free_port().await.map_err(|e| Error::Other(e.to_string()))?;
    inst.cdp_port = free_port().await.map_err(|e| Error::Other(e.to_string()))?;
    // VNC human-viewer port. Firecracker reaches the guest directly at its IP,
    // so the port is the in-guest 5900. QEMU needs a free host port to hostfwd
    // to guest 5900 (set before start()).
    inst.vnc_port = match inst.backend {
        VmBackend::Firecracker => 5900,
        VmBackend::Qemu => free_port().await.map_err(|e| Error::Other(e.to_string()))?,
    };
    save_instance(cwd, &inst).map_err(|e| Error::Other(e.to_string()))?;
    let sup = backend_for(&inst);
    match sup.start(cwd, &inst).await {
        Ok(()) => {
            inst.state = VmState::Running;
            save_instance(cwd, &inst).map_err(|e| Error::Other(e.to_string()))?;
            Ok(ToolResult::ok(format!(
                "'{name}' started: ip={} ssh={}",
                guest_ip_for(inst.slot),
                inst.ssh_port
            )))
        }
        Err(e) => {
            inst.state = VmState::Stopped;
            save_instance(cwd, &inst).map_err(|e| Error::Other(e.to_string()))?;
            Err(Error::Other(format!("start failed: {e}")))
        }
    }
}

/// Background session prewarm: if `vm.defaultExecution` is "vm" (the default),
/// bring the default instance up ahead of first use so the first VM-bound tool
/// call doesn't pay the cold-boot latency. No-op when the setting is "host".
/// Idempotent and safe to call unconditionally at serve startup — a VM that is
/// already running short-circuits immediately. Errors are logged, not fatal:
/// a failed prewarm just means the first real call pays the cold boot.
pub async fn prewarm_default(cwd: &Path) {
    let settings = load_vm_settings();
    let mode = settings.default_execution.as_deref().unwrap_or("vm");
    if mode != "vm" {
        return;
    }
    let name = "default";
    let inst = match load_instance(cwd, name) {
        Some(i) => i,
        None => match create_instance(cwd, name, None) {
            Ok(_) => match load_instance(cwd, name) {
                Some(i) => i,
                None => {
                    tracing::warn!("vm prewarm: instance vanished after create");
                    return;
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "vm prewarm: create failed");
                return;
            }
        },
    };
    if inst.state == VmState::Running {
        return;
    }
    match start_instance(cwd, inst).await {
        Ok(r) => tracing::info!(result = %r.data, "vm prewarm: default instance ready"),
        Err(e) => tracing::warn!(error = %e, "vm prewarm: start failed"),
    }
}

fn pick_backend(settings: &super::backend::VmSettings) -> VmBackend {
    match settings.backend.as_deref() {
        Some("qemu") => VmBackend::Qemu,
        Some("firecracker") => VmBackend::Firecracker,
        _ => {
            #[cfg(target_os = "linux")]
            { VmBackend::Firecracker }
            #[cfg(not(target_os = "linux"))]
            { VmBackend::Qemu }
        }
    }
}

// ---------------------------------------------------------------------------
// VmExec — terminal
// ---------------------------------------------------------------------------

pub struct VmExecTool;

#[async_trait]
impl Tool for VmExecTool {
    fn name(&self) -> &str {
        "VmExec"
    }
    fn prompt(&self) -> &str {
        "Run a shell command inside a persistent Linux microVM over SSH. Input: \"command\", \"vm\"? (default \"default\"), \"timeoutMs\"?. Like the Bash tool but inside the VM: full root, persistent filesystem, keeps installed packages across calls. The VM keeps state across calls and sessions."
    }
    fn description(&self) -> &str {
        "Execute a shell command inside a VM over SSH (persistent terminal environment)."
    }
    fn snippet(&self) -> String {
        "Run shell commands inside the microVM (SSH, like Bash but in the VM).".into()
    }
    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "Prefer running tasks in the VM by default: use VmExec/VmFs/VmBrowser for building, running untrusted or network-facing code, browsing, and scratch work. Reach for the host Bash/Write/Edit tools only when the task is specifically about the host machine (host paths, host services, this repo's own files) or the user asks for the host.",
            "Call `Vm ensure` once before VM work — it creates+starts the instance if needed and returns immediately if it is already running.",
            "The VM is disposable Linux — install packages freely with apt; they persist in the instance disk. The host project directory is NFS-mounted at /workspace (same files as the host), and VmExec starts there; VmFs put/get is still the way to move files outside the project.",
        ]
    }
    async fn check_permissions(&self, _input: &Value, _ctx: &ToolCtx<'_>) -> PermissionDecision {
        PermissionDecision::allow()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "vm": {"type": "string", "description": "Instance name (default: \"default\")"},
                "command": {"type": "string", "description": "bash -lc command line to run"},
                "timeoutMs": {"type": "integer", "description": "default 60000"}
            },
            "required": ["command"]
        })
    }
    fn is_read_only(&self, input: &Value) -> bool {
        matches!(input["type"].as_str(), None) // exec always mutates? keep simple: false
    }
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }
    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        _cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let name = input["vm"].as_str().unwrap_or("default");
        let command = input["command"].as_str().unwrap_or_default().to_string();
        let timeout_ms = input["timeoutMs"].as_u64().unwrap_or(60_000);
        let Some(inst) = load_instance(ctx.cwd, name) else {
            return Ok(ToolResult::error(format!("no VM instance {name:?} — create+start first")));
        };
        if inst.state != VmState::Running {
            return Ok(ToolResult::error(format!("VM {name:?} not running (state={:?})", inst.state)));
        }
        // The project is NFS-mounted at /workspace when the host exported it, so
        // land there by default; fall back to root's home if the mount failed.
        let cmdline = format!(
            "bash -lc {}",
            shell_quote(&format!(
                "cd {} 2>/dev/null || true\n{command}",
                super::firecracker::WORKSPACE_MOUNT
            ))
        );
        match ssh::exec(ctx.cwd, &inst, &cmdline, timeout_ms / 1000).await {
            Ok((code, out)) => Ok(ToolResult::ok(format!(
                "[exit {code}]\n{out}"
            ))),
            Err(e) => Ok(ToolResult::error(e)),
        }
    }
}

/// POSIX single-quote for wrapping the command in bash -lc '…'.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// VmFs — file bridge
// ---------------------------------------------------------------------------

pub struct VmFsTool;

#[async_trait]
impl Tool for VmFsTool {
    fn name(&self) -> &str {
        "VmFs"
    }
    fn prompt(&self) -> &str {
        "Move files between the host project and a microVM. Actions: put, get, list. put: \"localPath\", \"remotePath\"; get: \"remotePath\", \"localPath\"? (defaults to spool dir); list: \"path\"?. Retrieved files land under .nonoclaw/vm/<name>/spool/ where the Read tool can access them."
    }
    fn description(&self) -> &str {
        "File bridge host↔VM: put/get/list."
    }
    fn snippet(&self) -> String {
        "Copy files host→VM (put) and VM→host (get); list VM directories.".into()
    }
    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "get files into the host spool dir then Read them — do not cat large files over SSH.",
            "put uses scp -r: both files and directories work.",
        ]
    }
    async fn check_permissions(&self, _input: &Value, _ctx: &ToolCtx<'_>) -> PermissionDecision {
        PermissionDecision::allow()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "type": {"type": "string", "enum": ["put", "get", "list"],
                          "description": "put: host→VM; get: VM→host; list: VM dir listing"},
                "vm": {"type": "string"},
                "localPath": {"type": "string", "description": "host path (put/get)"},
                "vmPath": {"type": "string", "description": "VM path (put/get/list)"}
            },
            "required": ["type"]
        })
    }
    fn is_read_only(&self, input: &Value) -> bool {
        matches!(input["type"].as_str(), Some("list"))
    }
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false // scp copies race on same paths
    }
    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        _cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let name = input["vm"].as_str().unwrap_or("default");
        let action = input["type"].as_str().unwrap_or_default();
        let Some(inst) = load_instance(ctx.cwd, name) else {
            return Ok(ToolResult::error(format!("no VM instance {name:?}")));
        };
        if inst.state != VmState::Running {
            return Ok(ToolResult::error(format!("VM {name:?} not running")));
        }
        match action {
            "put" => {
                let local = input["localPath"].as_str().unwrap_or_default();
                let vm_path = input["vmPath"].as_str().unwrap_or_default();
                if local.is_empty() || vm_path.is_empty() {
                    return Ok(ToolResult::error("localPath and vmPath required"));
                }
                let host = resolve_host_path(ctx.cwd, local);
                if !host.exists() {
                    return Ok(ToolResult::error(format!("{} not found", host.display())));
                }
                // ensure remote dir exists
                let parent = parent_of(vm_path);
                let _ = ssh::exec(ctx.cwd, &inst, &format!("mkdir -p {}", shell_quote(&parent)), 15).await;
                match ssh::put(ctx.cwd, &inst, &host, vm_path).await {
                    Ok(()) => Ok(ToolResult::ok(format!("put {} → vm:{vm_path}", host.display()))),
                    Err(e) => Ok(ToolResult::error(e)),
                }
            }
            "get" => {
                let vm_path = input["vmPath"].as_str().unwrap_or_default();
                if vm_path.is_empty() {
                    return Ok(ToolResult::error("vmPath required"));
                }
                let spool = ssh::spool_dir(ctx.cwd, name);
                tokio::fs::create_dir_all(&spool).await.map_err(|e| Error::Other(e.to_string()))?;
                let file_name = Path::new(vm_path)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "file".into());
                let local = spool.join(&file_name);
                match ssh::get(ctx.cwd, &inst, vm_path, &local).await {
                    Ok(()) => {
                        let meta = tokio::fs::metadata(&local).await.ok();
                        Ok(ToolResult::ok(format!(
                            "got vm:{vm_path} → {} ({})",
                            local.display(),
                            meta.map(|m| format!("{} bytes", m.len())).unwrap_or_default()
                        )))
                    }
                    Err(e) => Ok(ToolResult::error(e)),
                }
            }
            "list" => {
                let vm_path = input["vmPath"].as_str().unwrap_or("/");
                match ssh::exec(
                    ctx.cwd,
                    &inst,
                    &format!("ls -la --time-style=+ {}", shell_quote(vm_path)),
                    20,
                )
                .await
                {
                    Ok((code, out)) => Ok(ToolResult::ok(format!("[exit {code}]\n{out}"))),
                    Err(e) => Ok(ToolResult::error(e)),
                }
            }
            other => Ok(ToolResult::error(format!("unknown action {other:?}"))),
        }
    }
}

fn resolve_host_path(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn parent_of(p: &str) -> String {
    match p.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => p[..i].into(),
        None => ".".into(),
    }
}

// ---------------------------------------------------------------------------
// VmBrowser — Computer Use over CDP
// ---------------------------------------------------------------------------

pub struct VmBrowserTool;

static SHOT_SEQ: AtomicU32 = AtomicU32::new(1);

#[async_trait]
impl Tool for VmBrowserTool {
    fn name(&self) -> &str {
        "VmBrowser"
    }
    fn prompt(&self) -> &str {
        "Computer Use: drive the real Chrome inside a microVM via CDP. Actions: navigate, snapshot, screenshot, click, type, key, scroll, eval. click uses \"x\",\"y\" coordinates from snapshot; type inserts text into the focused element; eval runs JS. Screenshots are PNG saved under .nonoclaw/vm/<name>/spool/ for the user; the tool returns a DOM text snapshot with numbered clickable elements and coordinates."
    }
    fn description(&self) -> &str {
        "Computer Use: drive the real Chrome inside the VM (navigate/snapshot/screenshot/click/type/scroll/eval)."
    }
    fn snippet(&self) -> String {
        "Browser automation in the VM: navigate, element map, screenshot, click, type, eval.".into()
    }
    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "Workflow: navigate → snapshot → click by coordinates / type / scroll → snapshot again.",
            "Snapshots carry precise coordinates; screenshots are for the user, not the model.",
        ]
    }
    async fn check_permissions(&self, _input: &Value, _ctx: &ToolCtx<'_>) -> PermissionDecision {
        PermissionDecision::allow()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "type": {"type": "string",
                    "enum": ["navigate", "snapshot", "screenshot", "click", "type",
                             "scroll", "eval", "list-tabs", "new-tab"],
                    "description": "browser action"},
                "vm": {"type": "string"},
                "url": {"type": "string", "description": "navigate/new-tab"},
                "x": {"type": "integer", "description": "click/scroll viewport x"},
                "y": {"type": "integer", "description": "click/scroll viewport y"},
                "dx": {"type": "integer", "description": "scroll delta x"},
                "dy": {"type": "integer", "description": "scroll delta y"},
                "text": {"type": "string", "description": "type: text to enter"},
                "submit": {"type": "boolean", "description": "type: press Enter after"},
                "expression": {"type": "string", "description": "eval: JS expression"},
                "fullPage": {"type": "boolean", "description": "screenshot: capture full page"}
            },
            "required": ["type"]
        })
    }
    fn is_read_only(&self, input: &Value) -> bool {
        matches!(input["type"].as_str(), Some("snapshot") | Some("screenshot"))
    }
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }
    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        _cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let name = input["vm"].as_str().unwrap_or("default");
        let action = input["type"].as_str().unwrap_or_default();
        let Some(inst) = load_instance(ctx.cwd, name) else {
            return Ok(ToolResult::error(format!("no VM instance {name:?}")));
        };
        if inst.state != VmState::Running {
            return Ok(ToolResult::error(format!("VM {name:?} not running")));
        }
        match action {
            "navigate" => {
                let url = input["url"].as_str().unwrap_or("about:blank");
                match super::cdp::navigate(&inst, url).await {
                    Ok(_) => Ok(ToolResult::ok(format!(
                        "navigated to {url} — follow with snapshot"
                    ))),
                    Err(e) => Ok(ToolResult::error(e)),
                }
            }
            "snapshot" => match super::cdp::eval(&inst, SNAPSHOT_JS).await {
                Ok(v) => Ok(ToolResult::ok(v.as_str().unwrap_or("(empty snapshot)").to_string())),
                Err(e) => Ok(ToolResult::error(e)),
            },
            "screenshot" => {
                let full = input["fullPage"].as_bool().unwrap_or(false);
                match super::cdp::screenshot(&inst, full).await {
                    Ok(b64) => {
                        let spool = ssh::spool_dir(ctx.cwd, name);
                        let _ = tokio::fs::create_dir_all(&spool).await;
                        let n = SHOT_SEQ.fetch_add(1, Ordering::SeqCst);
                        let path = spool.join(format!("shot-{n}.png"));
                        let bytes = base64_decode(&b64).map_err(|e| Error::Other(e))?;
                        tokio::fs::write(&path, &bytes)
                            .await
                            .map_err(|e| Error::Other(e.to_string()))?;
                        Ok(ToolResult::ok(format!(
                            "screenshot saved: {} ({} bytes)",
                            path.display(),
                            bytes.len()
                        )))
                    }
                    Err(e) => Ok(ToolResult::error(e)),
                }
            }
            "click" => {
                let x = input["x"].as_i64().unwrap_or(0);
                let y = input["y"].as_i64().unwrap_or(0);
                match super::cdp::click(&inst, x, y).await {
                    Ok(()) => Ok(ToolResult::ok(format!("clicked ({x},{y})"))),
                    Err(e) => Ok(ToolResult::error(e)),
                }
            }
            "type" => {
                let text = input["text"].as_str().unwrap_or_default();
                let submit = input["submit"].as_bool().unwrap_or(false);
                match super::cdp::type_text(&inst, text, submit).await {
                    Ok(()) => Ok(ToolResult::ok(format!("typed {} chars{}", text.len(), if submit { " + Enter" } else { "" }))),
                    Err(e) => Ok(ToolResult::error(e)),
                }
            }
            "scroll" => {
                let x = input["x"].as_i64().unwrap_or(640);
                let y = input["y"].as_i64().unwrap_or(400);
                let dx = input["dx"].as_i64().unwrap_or(0);
                let dy = input["dy"].as_i64().unwrap_or(600);
                match super::cdp::scroll(&inst, x, y, dx, dy).await {
                    Ok(()) => Ok(ToolResult::ok(format!("scrolled by ({dx},{dy})"))),
                    Err(e) => Ok(ToolResult::error(e)),
                }
            }
            "eval" => {
                let expr = input["expression"].as_str().unwrap_or_default();
                match super::cdp::eval(&inst, expr).await {
                    Ok(v) => Ok(ToolResult::ok(serde_json::to_string_pretty(&v).unwrap_or_default())),
                    Err(e) => Ok(ToolResult::error(e)),
                }
            }
            "list-tabs" => match super::cdp::list_tabs(&inst).await {
                Ok(v) => {
                    let mut out = String::new();
                    if let Some(arr) = v.as_array() {
                        for t in arr {
                            out.push_str(&format!(
                                "- [{}] {} {}\n",
                                t.get("type").and_then(Value::as_str).unwrap_or("?"),
                                t.get("title").and_then(Value::as_str).unwrap_or(""),
                                t.get("url").and_then(Value::as_str).unwrap_or(""),
                            ));
                        }
                    }
                    Ok(ToolResult::ok(out))
                }
                Err(e) => Ok(ToolResult::error(e)),
            },
            "new-tab" => {
                let url = input["url"].as_str().unwrap_or("about:blank");
                match super::cdp::new_tab(&inst, url).await {
                    Ok(_) => Ok(ToolResult::ok(format!("opened {url}"))),
                    Err(e) => Ok(ToolResult::error(e)),
                }
            }
            other => Ok(ToolResult::error(format!("unknown action {other:?}"))),
        }
    }
}

/// DOM snapshot: visible text outline + numbered interactive elements with
/// viewport coordinates (the model's "eyes" — cheaper and more precise than
/// screenshots for most tasks).
const SNAPSHOT_JS: &str = r#"(() => {
  const out = [];
  const els = [...document.querySelectorAll('a,button,input,select,textarea,[role=button],[onclick],h1,h2,h3')];
  let i = 0;
  for (const el of els) {
    const r = el.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) continue;
    const cx = Math.round(r.x + r.width/2), cy = Math.round(r.y + r.height/2);
    if (cy < -100 || cy > innerHeight + 100) continue;
    const label = (el.innerText || el.value || el.placeholder || el.getAttribute('aria-label') || '').trim().slice(0,80).replace(/\n/g,' ');
    out.push(`[${i}] (${cx},${cy}) <${el.tagName.toLowerCase()}> ${label}`);
    i++;
    if (i >= 120) break;
  }
  const text = document.body.innerText.replace(/\n{3,}/g,'\n\n').slice(0, 3000);
  return `URL: ${location.href}\nTITLE: ${document.title}\nscrollY: ${scrollY} viewport: ${innerWidth}x${innerHeight}\n\n== text ==\n${text}\n\n== interactive elements ==\n${out.join('\n')}`;
})()"#;

/// std-only base64 decode (RFC 4648) — avoids a new dependency.
fn base64_decode(s: &str) -> std::result::Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &b in s.as_bytes() {
        let v = match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return Err(format!("bad base64 byte {b:#x}")),
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        let data = b"hello png \x00\x01\x02".to_vec();
        // encode
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut enc = String::new();
        for chunk in data.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            enc.push(T[(n >> 18) as usize & 63] as char);
            enc.push(T[(n >> 12) as usize & 63] as char);
            enc.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
            enc.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
        }
        assert_eq!(base64_decode(&enc).unwrap(), data);
    }

    #[test]
    fn shell_quote_escapes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}
