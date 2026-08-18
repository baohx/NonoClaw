//! Linux Landlock filesystem sandbox for Bash commands.
//!
//! Landlock (Linux 5.13+) lets an unprivileged process restrict its own
//! filesystem access. We use it as an OS-level backstop for the permission
//! layer: when a run is in a sandboxed permission mode, the Bash tool installs
//! a ruleset in the child (via `Command::pre_exec`) that grants
//! read+execute across the whole filesystem but only grants writes under the
//! workspace (workspace-write mode) or nowhere (read-only mode).
//!
//! This module is Linux-only. On other platforms (or kernels without Landlock)
//! [`probe`] returns `false` and the caller falls back to approval-only gating.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use landlock::{
    path_beneath_rules, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr,
    RulesetCreatedAttr, ABI,
};

/// Sandbox write posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// Read+execute the whole filesystem; write only under the workspace and
    /// any extra writable paths.
    WorkspaceWrite,
    /// Read+execute the whole filesystem; deny all writes.
    ReadOnly,
}

/// Target Landlock ABI. `BestEffort` compatibility downgrades this to whatever
/// the running kernel actually supports (ABI V1 = Linux 5.13 is the floor).
const TARGET_ABI: ABI = ABI::V5;

static SUPPORTED: OnceLock<bool> = OnceLock::new();

/// Whether the running kernel supports Landlock. Cached after the first call.
/// Safe to call from the parent process: it only creates a ruleset descriptor,
/// it never restricts the current thread.
pub fn probe() -> bool {
    *SUPPORTED.get_or_init(|| {
        Ruleset::default()
            .handle_access(AccessFs::from_read(TARGET_ABI))
            .and_then(|ruleset| ruleset.create())
            .is_ok()
    })
}

/// Install a Landlock ruleset in the *current* thread, then return. Must be
/// called from `Command::pre_exec` (after fork, before exec) so only the child
/// is restricted. `extra_writable` augments the writable set for
/// [`SandboxMode::WorkspaceWrite`] and is ignored for [`SandboxMode::ReadOnly`].
pub fn apply(
    mode: SandboxMode,
    workspace_root: &Path,
    extra_writable: &[PathBuf],
) -> std::io::Result<()> {
    let read_access = AccessFs::from_read(TARGET_ABI);
    let write_access = AccessFs::from_write(TARGET_ABI);

    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(TARGET_ABI))
        .map_err(ruleset_io)?
        .create()
        .map_err(ruleset_io)?;

    // Read + execute the whole filesystem (compilers and language runtimes
    // read system libraries; the shell executes subcommands).
    ruleset = ruleset
        .add_rules(path_beneath_rules([Path::new("/")], read_access))
        .map_err(ruleset_io)?;

    if mode == SandboxMode::WorkspaceWrite {
        let mut writable: Vec<PathBuf> = Vec::with_capacity(1 + extra_writable.len());
        writable.push(workspace_root.to_path_buf());
        writable.extend(extra_writable.iter().cloned());
        ruleset = ruleset
            .add_rules(path_beneath_rules(writable.iter().map(PathBuf::as_path), write_access))
            .map_err(ruleset_io)?;
    }

    ruleset
        .set_compatibility(CompatLevel::BestEffort)
        .restrict_self()
        .map_err(ruleset_io)?;
    Ok(())
}

fn ruleset_io(error: landlock::RulesetError) -> std::io::Error {
    std::io::Error::other(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_a_bool_without_restricting() {
        // probe() must not panic on any platform and must not restrict the
        // current thread (it only creates a ruleset fd).
        let _ = probe();
    }

    #[test]
    fn sandbox_modes_are_distinct() {
        assert_ne!(SandboxMode::WorkspaceWrite, SandboxMode::ReadOnly);
    }

    /// End-to-end Landlock enforcement check: a read-only sandbox must deny a
    /// write outside the workspace. Skipped (not failed) when the kernel lacks
    /// Landlock so the suite remains green on unsupported hosts.
    #[cfg(target_os = "linux")]
    #[test]
    fn read_only_sandbox_denies_writes_outside_workspace() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;

        if !probe() {
            eprintln!("skipping: Landlock not supported on this kernel");
            return;
        }

        let target = std::env::temp_dir().join(format!(
            "nonoclaw-landlock-probe-{}",
            uuid::Uuid::new_v4()
        ));
        let workspace = std::env::temp_dir();
        // `touch <target>` requires MakeReg + WriteFile; a read-only sandbox
        // grants neither outside the workspace, so the child must fail.
        let status = unsafe {
            Command::new("touch")
                .arg(&target)
                .pre_exec(move || apply(SandboxMode::ReadOnly, &workspace, &[]))
                .status()
        }
        .expect("failed to spawn sandboxed child");

        assert!(
            !status.success(),
            "read-only sandbox must deny writes outside the workspace"
        );
        let _ = std::fs::remove_file(&target);
    }
}
