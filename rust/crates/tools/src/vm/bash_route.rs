//! Host-side `Bash` routing into the microVM.
//!
//! When `vm.defaultExecution = "vm"` and a running instance exists, shell
//! commands execute in the guest instead of the host. The guest sees the host
//! project directory at `/workspace` over NFS, so relative paths, `git`, and
//! build commands behave the same way they do locally — the whole point of the
//! shared mount is that the agent does not have to care which side it runs on.

use crate::vm::{
    backend::{load_instance, load_vm_settings, VmState},
    ssh,
};
use crate::tool::ToolResult;
use nonoclaw_core::Error;
use serde_json::Value;
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Guest mount point for the host project directory.
const WORKSPACE: &str = crate::vm::firecracker::WORKSPACE_MOUNT;

/// Commands that must stay on the host: they act on host-only state (the VM
/// itself, the host filesystem outside the project, container runtime) and would
/// either fail or give a misleading answer inside the guest.
const HOST_ONLY: &[&str] = &[
    "docker", "podman", "systemctl", "journalctl", "sudo", "apt", "apt-get", "dpkg",
];

/// Run `command` in the guest when VM-default execution is on.
///
/// Returns `None` when the caller should fall through to host execution (feature
/// off, no instance, guest not reachable, or a host-only command). Errors from a
/// command that *did* run in the guest are returned as a `ToolResult` — a failing
/// command is a normal result, not a tool failure.
pub async fn maybe_run_in_vm(
    cwd: &Path,
    command: &str,
    input: &Value,
    cancel: &CancellationToken,
) -> Option<Result<ToolResult, Error>> {
    let settings = load_vm_settings();
    if settings.default_execution.as_deref() != Some("vm") {
        return None;
    }
    if is_host_only(command) {
        return None;
    }
    let inst = load_instance(cwd, "default")?;
    if inst.state != VmState::Running {
        return None;
    }
    // The mount only exists when the host exported the directory; without it the
    // guest would run the command in an empty /workspace and silently produce
    // wrong results. Fall back to the host instead.
    let (code, has_mount) = match ssh::exec(
        cwd,
        &inst,
        &format!("mountpoint -q {WORKSPACE} && echo yes || echo no"),
        15,
    )
    .await
    {
        Ok((c, out)) => (c, out.trim() == "yes"),
        Err(e) => {
            tracing::warn!(error = %e, "vm bash: guest unreachable, running on host");
            return None;
        }
    };
    let _ = code;
    if !has_mount {
        tracing::warn!("vm bash: {WORKSPACE} not mounted in guest (NFS export missing?), running on host");
        return None;
    }

    let timeout_secs = input["timeout_ms"]
        .as_u64()
        .map(|ms| (ms / 1000).max(1))
        .unwrap_or(120);

    let script = format!("cd {WORKSPACE} && {command}");
    let run = ssh::exec(cwd, &inst, &script, timeout_secs);
    let result = tokio::select! {
        r = run => r,
        _ = cancel.cancelled() => return Some(Err(Error::Cancelled)),
    };
    match result {
        Ok((exit_code, output)) => Some(Ok(ToolResult::ok(format!(
            "[vm] {output}{}",
            if exit_code == 0 {
                String::new()
            } else {
                format!("\n[exit {exit_code}]")
            }
        )))),
        Err(e) => Some(Ok(ToolResult::error(format!("[vm] failed: {e}")))),
    }
}

/// True when the command's first word targets host-only tooling.
///
/// Skips leading `VAR=value` assignments and `env`, so `FOO=1 apt-get update`
/// is still classified as host-only.
fn is_host_only(command: &str) -> bool {
    command
        .split(|c: char| c.is_whitespace() || c == ';' || c == '&' || c == '|')
        .find(|t| !t.is_empty() && *t != "env" && !is_assignment(t))
        .map(|t| HOST_ONLY.contains(&t))
        .unwrap_or(false)
}

/// `NAME=value` with a shell-legal identifier on the left.
fn is_assignment(token: &str) -> bool {
    match token.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_only_detection() {
        assert!(is_host_only("docker ps"));
        assert!(is_host_only("sudo apt-get install -y jq"));
        assert!(is_host_only("FOO=1 apt-get update"));
        assert!(is_host_only("env docker ps"));
        assert!(!is_host_only("git status"));
        assert!(!is_host_only("ls -la && cat x"));
    }
}
