//! Host↔guest SSH channel: exec / put / get, using the system `ssh` binary.
//!
//! A single ed25519 keypair in the global VM assets dir is shared by all
//! instances: its public half is baked into the base rootfs at build time
//! (scripts/vm/build-image.sh), so every instance cloned from it accepts it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use super::backend::{guest_ip_for, instance_dir, vm_assets_dir, VmInstance};

/// Generate the global keypair if missing (idempotent).
pub fn ensure_key(_cwd: &Path, _name: &str) -> Result<(), String> {
    let dir = vm_assets_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let key = dir.join("ssh_key");
    if key.exists() {
        return Ok(());
    }
    let out = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", &format!("nonoclaw-vm-global"), "-f"])
        .arg(&key)
        .output()
        .map_err(|e| format!("ssh-keygen missing: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok(())
}

/// Guest-side user (root on the nocloud image).
pub const VM_USER: &str = "root";

fn base_ssh_args(_cwd: &Path, inst: &VmInstance) -> Vec<String> {
    let key = vm_assets_dir().join("ssh_key");
    let ip = ssh_host_for(inst);
    vec![
        // Strict guest identity: accept our pinned key, no password prompts.
        "-i".into(),
        key.to_string_lossy().into(),
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-p".into(),
        ssh_port_for(inst).to_string(),
        format!("{VM_USER}@{ip}"),
    ]
}

/// scp shares every ssh option EXCEPT the port flag: ssh uses `-p PORT` but
/// scp uses `-P PORT`. Reusing the ssh args verbatim makes scp read `-p` as
/// its "preserve mode" flag and treat the bare port number as a source file
/// (`scp: stat local "22"`). This builder swaps the flag and drops the
/// trailing `user@host` (callers append host-qualified paths themselves).
fn scp_opts(cwd: &Path, inst: &VmInstance) -> Vec<String> {
    base_ssh_args(cwd, inst)
        .into_iter()
        .filter(|a| !a.starts_with(&format!("{VM_USER}@")))
        .map(|a| if a == "-p" { "-P".to_string() } else { a })
        .collect()
}

fn ssh_host_for(inst: &VmInstance) -> String {
    match inst.backend {
        super::backend::VmBackend::Firecracker => guest_ip_for(inst.slot),
        // QEMU hostfwd binds on 127.0.0.1
        super::backend::VmBackend::Qemu => "127.0.0.1".into(),
    }
}

fn ssh_port_for(inst: &VmInstance) -> u16 {
    match inst.backend {
        // direct L3 → guest port 22
        super::backend::VmBackend::Firecracker => 22,
        super::backend::VmBackend::Qemu => inst.ssh_port,
    }
}

/// Run a command inside the guest, returning (exit_code, combined_output).
pub async fn exec(
    cwd: &Path,
    inst: &VmInstance,
    command: &str,
    timeout_secs: u64,
) -> Result<(i32, String), String> {
    let mut cmd = Command::new("ssh");
    cmd.args(base_ssh_args(cwd, inst))
        .arg(command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(
        Duration::from_secs(timeout_secs.max(1)),
        cmd.output(),
    )
    .await
    .map_err(|_| format!("SSH exec timed out after {timeout_secs}s"))?
    .map_err(|e| format!("ssh failed: {e}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    Ok((out.status.code().unwrap_or(-1), text))
}

/// Upload a file/dir via `scp -r` (or `ssh cat >` for single small files).
pub async fn put(
    cwd: &Path,
    inst: &VmInstance,
    local: &Path,
    remote: &str,
) -> Result<(), String> {
    let local_str = local.to_string_lossy().to_string();
    let is_dir = local.is_dir();
    let remote_path = if is_dir && !remote.ends_with('/') {
        format!("{remote}/")
    } else {
        remote.to_string()
    };
    let mut cmd = Command::new("scp");
    cmd.arg("-r")
        .args(scp_opts(cwd, inst))
        .arg(&local_str)
        // Remote destination needs the `user@host:` prefix, otherwise scp
        // treats it as a second local path (bogus "Permission denied" on /root)
        // and defaults to the local login user instead of the guest's root.
        .arg(format!("{VM_USER}@{}:{remote_path}", ssh_host_for(inst)))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(180), cmd.output())
        .await
        .map_err(|_| "scp put timed out (180s)".to_string())?
        .map_err(|e| format!("scp failed: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok(())
}

/// Download a file from the guest. Images come back as bytes for the tool
/// layer to decide text-vs-image presentation.
pub async fn get(
    cwd: &Path,
    inst: &VmInstance,
    remote: &str,
    local: &Path,
) -> Result<(), String> {
    let mut cmd = Command::new("scp");
    cmd.args(scp_opts(cwd, inst))
        .arg(format!("{VM_USER}@{}:{remote}", ssh_host_for(inst)))
        .arg(local)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(180), cmd.output())
        .await
        .map_err(|_| "scp get timed out (180s)".to_string())?
        .map_err(|e| format!("scp failed: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    Ok(())
}

/// Where downloaded files land on the host (per-instance spool).
pub fn spool_dir(cwd: &Path, name: &str) -> PathBuf {
    instance_dir(cwd, name).join("spool")
}
