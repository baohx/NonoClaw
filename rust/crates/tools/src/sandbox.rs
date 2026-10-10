//! OS-level filesystem sandbox for Bash commands.
//!
//! Linux uses Landlock (kernel 5.13+): an unprivileged process restricts its
//! own filesystem access. We use it as an OS-level backstop for the
//! permission layer: when a run is in a sandboxed permission mode, the Bash
//! tool installs a ruleset in the child (via `Command::pre_exec`) that grants
//! read+execute across the whole filesystem but only grants writes under the
//! workspace (workspace-write mode) or nowhere (read-only mode).
//!
//! macOS uses Seatbelt (`sandbox-exec`): Landlock's pre_exec model does not
//! apply because the restriction must be installed by a wrapper process, so
//! the Bash tool instead wraps the child argv in `sandbox-exec -f <profile>`.
//! The profile grants read+execute everywhere and denies writes outside the
//! workspace (workspace-write) or everywhere (read-only).
//!
//! This module is Linux/macOS-only. On unsupported platforms [`probe`]
//! returns `false` and the caller falls back to approval-only gating.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Sandbox write posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// Read+execute the whole filesystem; write only under the workspace and
    /// any extra writable paths.
    WorkspaceWrite,
    /// Read+execute the whole filesystem; deny all writes.
    ReadOnly,
}

// ── Linux: Landlock ─────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use landlock::{
        path_beneath_rules, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr,
        RulesetCreatedAttr, ABI,
    };

    /// Target Landlock ABI. `BestEffort` compatibility downgrades this to
    /// whatever the running kernel actually supports (ABI V1 = Linux 5.13 is
    /// the floor).
    const TARGET_ABI: ABI = ABI::V5;

    static SUPPORTED: OnceLock<bool> = OnceLock::new();

    /// Whether the running kernel supports Landlock. Cached after the first
    /// call. Safe to call from the parent process: it only creates a ruleset
    /// descriptor, it never restricts the current thread.
    pub fn probe() -> bool {
        *SUPPORTED.get_or_init(|| {
            Ruleset::default()
                .handle_access(AccessFs::from_read(TARGET_ABI))
                .and_then(|ruleset| ruleset.create())
                .is_ok()
        })
    }

    /// Install a Landlock ruleset in the *current* thread, then return. Must
    /// be called from `Command::pre_exec` (after fork, before exec) so only
    /// the child is restricted. `extra_writable` augments the writable set
    /// for [`SandboxMode::WorkspaceWrite`] and is ignored for
    /// [`SandboxMode::ReadOnly`].
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
                .add_rules(path_beneath_rules(
                    writable.iter().map(PathBuf::as_path),
                    write_access,
                ))
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
}

// ── macOS: Seatbelt (sandbox-exec) ──────────────────────────────────────────

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    static SUPPORTED: OnceLock<bool> = OnceLock::new();

    /// Whether `sandbox-exec` is available on this system. Cached.
    pub fn probe() -> bool {
        *SUPPORTED.get_or_init(|| {
            std::process::Command::new("sandbox-exec")
                .arg("--help")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
        })
    }

    /// Escape a path for a Seatbelt string literal: backslash and quote.
    fn escape_subpath(path: &Path) -> String {
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }

    /// Build the Seatbelt profile body for the mode. Deny-by-default writes;
    /// read+execute everywhere (the `(allow default)` term).
    ///
    /// Paths are canonicalized first: Seatbelt `subpath` filters match the
    /// *resolved* path, and prefixes like `/tmp` (a symlink to
    /// `/private/tmp`) never match un-canonicalized.
    fn profile(mode: SandboxMode, workspace_root: &Path, extra_writable: &[PathBuf]) -> String {
        fn resolved(path: &Path) -> PathBuf {
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        }
        let mut body = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
        if mode == SandboxMode::WorkspaceWrite {
            body.push_str(&format!(
                "(allow file-write* (subpath \"{}\"))\n",
                escape_subpath(&resolved(workspace_root))
            ));
            for extra in extra_writable {
                body.push_str(&format!(
                    "(allow file-write* (subpath \"{}\"))\n",
                    escape_subpath(&resolved(extra))
                ));
            }
        }
        body
    }

    /// Write the profile to a unique 0600 temp file and return its path.
    /// `create_new` gives O_EXCL semantics so a concurrent process cannot
    /// pre-create or swap the file (profile injection).
    fn write_profile(body: &str) -> std::io::Result<PathBuf> {
        let mut attempt = 0u32;
        loop {
            let path = std::env::temp_dir().join(format!(
                "nonoclaw-seatbelt-{}-{}.sb",
                std::process::id(),
                attempt
            ));
            use std::io::Write;
            use std::os::unix::fs::PermissionsExt;
            let mut file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    attempt += 1;
                    continue;
                }
                Err(error) => return Err(error),
            };
            file.write_all(body.as_bytes())?;
            let _ = file.sync_all();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            return Ok(path);
        }
    }

    /// Wrap the user command argv in `sandbox-exec -f <profile> -- <argv>`.
    /// The Bash tool calls this instead of spawning the shell directly; the
    /// returned argv replaces `bash -c <cmd>` so the whole child tree (shell
    /// + descendants) inherits the Seatbelt restrictions.
    pub fn wrap_argv(
        mode: SandboxMode,
        workspace_root: &Path,
        extra_writable: &[PathBuf],
        user_program: &str,
        user_args: &[String],
    ) -> std::io::Result<Vec<String>> {
        let body = profile(mode, workspace_root, extra_writable);
        let profile_path = write_profile(&body)?;
        let mut argv = vec![
            "sandbox-exec".to_string(),
            "-f".to_string(),
            profile_path.to_string_lossy().into_owned(),
            "--".to_string(),
            user_program.to_string(),
        ];
        argv.extend(user_args.iter().cloned());
        Ok(argv)
    }

    /// Best-effort removal of the profile file named by a `wrap_argv`
    /// result (argv[2]). Called after the child exits; failures ignored —
    /// a stale 0600 file in the temp dir is harmless.
    pub fn cleanup(argv: &[String]) {
        if argv.len() > 2 && argv[0] == "sandbox-exec" && argv[1] == "-f" {
            let _ = std::fs::remove_file(&argv[2]);
        }
    }
}

// ── Platform dispatch ───────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub use imp::{apply, probe};

#[cfg(target_os = "macos")]
pub use imp::{cleanup, probe, wrap_argv};

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::*;

    pub fn probe() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_a_bool_without_restricting() {
        // probe() must not panic on any platform and must not restrict the
        // current thread (it only creates a ruleset fd / probes a binary).
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

        let target =
            std::env::temp_dir().join(format!("nonoclaw-landlock-probe-{}", uuid::Uuid::new_v4()));
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

    /// End-to-end Seatbelt enforcement check on macOS: a read-only profile
    /// must deny writes outside the workspace. Skipped when `sandbox-exec`
    /// is unavailable.
    #[cfg(target_os = "macos")]
    #[test]
    fn read_only_seatbelt_denies_writes_outside_workspace() {
        if !probe() {
            eprintln!("skipping: sandbox-exec not available");
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "nonoclaw-seatbelt-probe-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.txt");
        let argv = wrap_argv(
            SandboxMode::ReadOnly,
            &dir,
            &[],
            "touch",
            &[target.to_string_lossy().into_owned()],
        )
        .expect("failed to build seatbelt argv");
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .expect("failed to spawn sandboxed child");
        assert!(
            !status.success(),
            "read-only seatbelt must deny writes outside the workspace"
        );
        cleanup(&argv);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Workspace-write Seatbelt must still allow writes inside the workspace.
    #[cfg(target_os = "macos")]
    #[test]
    fn workspace_write_seatbelt_allows_writes_inside_workspace() {
        if !probe() {
            eprintln!("skipping: sandbox-exec not available");
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "nonoclaw-seatbelt-ws-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("in.txt");
        let argv = wrap_argv(
            SandboxMode::WorkspaceWrite,
            &dir,
            &[],
            "touch",
            &[target.to_string_lossy().into_owned()],
        )
        .expect("failed to build seatbelt argv");
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .expect("failed to spawn sandboxed child");
        assert!(
            status.success() && target.is_file(),
            "workspace-write seatbelt must allow writes inside the workspace"
        );
        cleanup(&argv);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
