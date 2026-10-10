//! Bash tool. Mirrors `src/tools/BashTool/`. Spawns the command in a shell,
//! captures combined output, and enforces a timeout. Oversized output is
//! normalized by the canonical `ToolExecutor`, which preserves the complete
//! result in a local reference.
//!
//! The ML command classifier remains conservative and local; permission mode
//! and configured rules are composed by the shared permission gate.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionDecision, PermissionMode, PermissionResult, Result};
use serde_json::{json, Value};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::tool::{Tool, ToolCtx, ToolResult};

const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const MAX_TIMEOUT_MS: u64 = 1_200_000;

const PROMPT: &str = "Executes a command inside a persistent shell and returns its combined stdout+stderr.\n\nThe working directory persists between calls. Shell environment (env vars, aliases) does not — each invocation starts from a fresh profile. On Linux/macOS the shell is bash; on Windows it is cmd /C.\n\nIMPORTANT: Always prefer dedicated tools (Read, Write, Edit, Grep, Glob, WebFetch, WebSearch) over raw shell commands. Only use Bash when no dedicated tool exists for the task.\n\n## Available commands\n- Package managers: cargo, npm, pip, apt, brew, etc.\n- Git: `git status`, `git diff`, `git log`, `git add -p`, `git commit -m`, `git stash`, `git branch`. NEVER run `git push --force`, `git reset --hard`, `git branch -D`, or destructive git commands unless the user explicitly requests them. NEVER update git config.\n- Build/test: `cargo build`, `cargo test`, `cargo check`, `npm test`, `make`, etc.\n- File listing: `ls -la`, `find`, `tree`. Prefer the Glob tool for pattern-based file discovery.\n- System info: `uname -a`, `which`, `env`, `cat /proc/cpuinfo` (Linux).\n- NEVER run interactive commands (e.g. commands without `-y` / `--yes`). stdin is closed — commands that prompt for input (sudo, ssh, passwd, etc.) will fail immediately.
- When a command needs stdin input, use heredoc (`<<'EOF'`) or pipe the data in.
- The agent automatically inserts `-n` after `sudo` so it runs in non-interactive mode. To allow passwordless sudo for specific commands, add a NOPASSWD rule via `visudo`, e.g.:
  `username ALL=(ALL) NOPASSWD: /usr/bin/apt, /usr/bin/systemctl`\n- NEVER run destructive system commands (`sudo rm -rf /`, `shutdown`, `reboot`, etc.) unless the user explicitly requests them.\n\n## Parameters\n- `command` (required): the shell command to execute.\n- `timeout_ms` (optional, default 300000 = 5 minutes, max 1200000 = 20 minutes). Increase for long builds.\n- `run_in_background` (optional): start a managed background task and return its task ID immediately.\n\n## Output\n- Combined stdout+stderr. Oversized results are summarized by the shared tool runtime, with the complete output saved to a local reference.\n- The exit code is appended to the output for non-zero exits.\n- If the command succeeds but produces no output, `[ok — no output]` is returned.";

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "Bash"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Executes a bash command on the local machine."
    }
    fn snippet(&self) -> String {
        "Run shell commands (build, test, git, package managers)".to_string()
    }
    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "Use the Grep tool instead of `rg`/`grep` in Bash for file content searches — it's faster and respects .gitignore.",
            "Use the Read tool instead of `cat`/`head`/`tail` in Bash for reading files.",
            "Quote paths with spaces; prefer absolute paths over `cd` when running one-off commands.",
        ]
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type":"string","description":"The bash command to execute"},
                "timeout_ms": {"type":"integer","description":format!("Optional timeout in milliseconds (max {MAX_TIMEOUT_MS})")},
                "run_in_background": {"type":"boolean","description":"Run as a managed background task and return its task ID"}
            },
            "required": ["command"]
        })
    }

    fn is_read_only(&self, input: &Value) -> bool {
        // Fail closed: only a very small set of commands with no shell
        // control syntax can bypass an approval prompt.
        classify_readonly(input["command"].as_str().unwrap_or(""))
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    async fn check_permissions(&self, input: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        if self.is_read_only(input) {
            PermissionResult::allow()
        } else {
            PermissionDecision::ask("run a shell command")
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let raw_command = require_command(&input)?;
        // sudo reads from the terminal by default — but stdin is closed and
        // there is no TTY. Insert `-n` (non-interactive) so that:
        //  - commands the user has allowed via NOPASSWD in sudoers succeed
        //  - everything else fails immediately with "sudo: a password is required"
        let command = ensure_sudo_noninteractive(raw_command);
        let timeout_ms = input["timeout_ms"]
            .as_u64()
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        // Background execution: spawn and return task ID immediately.
        if input["run_in_background"].as_bool().unwrap_or(false) {
            if let Some(ref reg) = ctx.background_registry {
                let task_id = reg.lock().unwrap().spawn_in_with_cancel(
                    &command,
                    ctx.cwd,
                    timeout_ms,
                    cancel.child_token(),
                );
                return Ok(ToolResult::ok(format!(
                    "Background task started.\nTask ID: {task_id}\nUse TaskOutput to read results."
                )));
            }
            return Ok(ToolResult::ok(
                "Background execution requested but no task registry available. Command will run inline."
            ));
        }

        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        #[cfg(windows)]
        let (shell, arg) = ("cmd", "/C");
        #[cfg(not(windows))]
        let (shell, arg, skip_profile) = resolve_unix_shell();

        // macOS Seatbelt wraps the whole child argv in `sandbox-exec -f …`
        // (no pre_exec-installable API exists); Linux Landlock is applied on
        // the Command below. Keep the wrapper argv for post-spawn cleanup.
        #[cfg(target_os = "macos")]
        let seatbelt = seatbelt_argv(ctx.options.permission_mode, ctx.cwd, shell, arg, &command);
        #[cfg(target_os = "macos")]
        let mut cmd = match seatbelt.as_deref() {
            Some(wrapper) => {
                // wrapper[0] is "sandbox-exec" itself; Command::new already
                // sets the program, so only pass the trailing args.
                let mut cmd = Command::new("sandbox-exec");
                cmd.args(&wrapper[1..]);
                cmd
            }
            None => {
                let mut cmd = Command::new(shell);
                if skip_profile {
                    cmd.arg("--noprofile").arg("--norc");
                }
                cmd.arg(arg).arg(command);
                cmd
            }
        };
        #[cfg(not(target_os = "macos"))]
        let mut cmd = {
            let mut cmd = Command::new(shell);
            // Do not load login/profile scripts: a nominally read-only command
            // must not trigger arbitrary profile side effects. PATH and other
            // required environment are inherited from the NonoClaw process.
            #[cfg(not(windows))]
            {
                if skip_profile {
                    cmd.arg("--noprofile").arg("--norc");
                }
            }
            cmd.arg(arg).arg(command);
            cmd
        };
        // Close stdin so interactive commands (sudo, ssh, passwd, etc.)
        // fail-fast with EOF instead of hanging until the timeout. The agent
        // should use non-interactive flags (-n, --yes, --non-interactive) or
        // inline input via heredoc/piping instead.
        cmd.stdin(std::process::Stdio::null());
        // OS-level sandbox backstop for sandboxed permission modes (Linux:
        // Landlock pre_exec ruleset; macOS is handled by the argv wrapper).
        #[cfg(not(target_os = "macos"))]
        apply_sandbox(&mut cmd, ctx.options.permission_mode, ctx.cwd);

        let mut child = cmd
            .current_dir(ctx.cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::Tool {
                tool: "Bash".into(),
                message: format!("failed to spawn shell: {e}"),
            })?;
        // The Seatbelt profile file can be removed once the child is running.
        #[cfg(target_os = "macos")]
        if let Some(wrapper) = seatbelt.as_deref() {
            crate::sandbox::cleanup(wrapper);
        }

        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");

        let timeout = Duration::from_millis(timeout_ms);
        // Read both pipes concurrently to avoid deadlock when the child fills
        // one pipe buffer while we drain the other, then wait for exit.
        let result = tokio::time::timeout(timeout, async move {
            use tokio::io::AsyncReadExt;
            let mut out_buf = Vec::new();
            let mut err_buf = Vec::new();
            let r1 = stdout.read_to_end(&mut out_buf);
            let r2 = stderr.read_to_end(&mut err_buf);
            let _ = tokio::join!(r1, r2);
            let status = child.wait().await;
            (out_buf, err_buf, status)
        })
        .await;

        match result {
            Ok((out_buf, err_buf, Ok(status))) => {
                let mut combined = String::new();
                combined.push_str(&String::from_utf8_lossy(&out_buf));
                if !err_buf.is_empty() {
                    combined.push_str("\n--- stderr ---\n");
                    combined.push_str(&String::from_utf8_lossy(&err_buf));
                }
                let code = status.code().unwrap_or(-1);
                let data = if code == 0 {
                    if combined.is_empty() {
                        "[ok — no output]".into()
                    } else {
                        combined
                    }
                } else {
                    format!("{combined}\n[exit code: {code}]")
                };
                Ok(ToolResult::ok(data))
            }
            Ok((_, _, Err(e))) => Err(Error::Tool {
                tool: "Bash".into(),
                message: format!("command failed: {e}"),
            }),
            Err(_) => Err(Error::Timeout),
        }
    }
}

fn require_command(input: &Value) -> Result<&str> {
    input["command"].as_str().ok_or_else(|| Error::Tool {
        tool: "Bash".into(),
        message: "missing required string field `command`".into(),
    })
}

/// Insert `-n` after `sudo` so it runs in non-interactive mode.
/// `sudo apt install` → `sudo -n apt install`
/// `sudo` already has `-n` → no change.
/// Non-sudo commands pass through unchanged.
fn ensure_sudo_noninteractive(cmd: &str) -> String {
    let trimmed = cmd.trim_start();
    let indent = &cmd[..cmd.len() - trimmed.len()];
    let mut words = trimmed.split_whitespace();
    if words.next() != Some("sudo") {
        return cmd.to_string();
    }
    // Already has -n somewhere in the args — don't double-add.
    if trimmed.split_whitespace().any(|w| w == "-n") {
        return cmd.to_string();
    }
    // Insert -n right after sudo, preserving any whitespace that followed it.
    let after_sudo = trimmed.strip_prefix("sudo").unwrap();
    format!("{indent}sudo -n{after_sudo}")
}

#[cfg(not(windows))]
/// Pick the Unix shell for the Bash tool. Prefers `bash` (POSIX-compatible
/// Bourne shell is the tool contract); falls back to `sh` on minimal distros
/// (Alpine/BusyBox) where bash is not installed. Returns
/// `(program, arg, is_bash)` — profile-skipping flags only apply to bash.
fn resolve_unix_shell() -> (&'static str, &'static str, bool) {
    use std::sync::OnceLock;
    // (bash, sh) probe results, cached for the process lifetime since PATH
    // rarely changes mid-run.
    static FOUND: OnceLock<(bool, bool)> = OnceLock::new();
    let (has_bash, has_sh) = *FOUND.get_or_init(|| (find_on_path("bash"), find_on_path("sh")));
    if has_bash {
        ("bash", "-c", true)
    } else if has_sh {
        ("sh", "-c", false)
    } else {
        // Neither found on PATH — let Command::new surface the spawn error
        // ("program not found") rather than guessing a path here.
        ("bash", "-c", true)
    }
}

#[cfg(not(windows))]
/// Cheap existence probe that avoids the `which` crate: iterate PATH and
/// check for an executable file.
fn find_on_path(program: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(program);
        std::fs::metadata(&candidate)
            .map(|m| m.is_file() && m.mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// Install a Linux Landlock ruleset in the Bash child when the run is in a
/// sandboxed permission mode. No-op when the kernel lacks Landlock (falls
/// back to approval-only gating).
#[cfg(target_os = "linux")]
fn apply_sandbox(cmd: &mut Command, mode: PermissionMode, cwd: &Path) {
    use crate::sandbox::{self, SandboxMode};
    let sandbox_mode = match mode {
        PermissionMode::SandboxWorkspaceWrite => Some(SandboxMode::WorkspaceWrite),
        PermissionMode::SandboxReadOnly => Some(SandboxMode::ReadOnly),
        _ => None,
    };
    let Some(sandbox_mode) = sandbox_mode else {
        return;
    };
    if !sandbox::probe() {
        return;
    }
    let workspace = cwd.to_path_buf();
    unsafe {
        cmd.pre_exec(move || sandbox::apply(sandbox_mode, &workspace, &[]));
    }
}

#[cfg(target_os = "macos")]
/// macOS Seatbelt: there is no pre_exec-installable ruleset API, so instead
/// of configuring `cmd` we rewrite the child argv to
/// `sandbox-exec -f <profile> -- bash -c <command>`. Returns the wrapper
/// argv (for profile cleanup after the child exits) or None when Seatbelt
/// is unavailable (falls back to approval-only gating).
fn seatbelt_argv(
    mode: PermissionMode,
    cwd: &Path,
    shell: &str,
    arg: &str,
    command: &str,
) -> Option<Vec<String>> {
    use crate::sandbox::{self, SandboxMode};
    let sandbox_mode = match mode {
        PermissionMode::SandboxWorkspaceWrite => Some(SandboxMode::WorkspaceWrite),
        PermissionMode::SandboxReadOnly => Some(SandboxMode::ReadOnly),
        _ => None,
    }?;
    if !sandbox::probe() {
        return None;
    }
    sandbox::wrap_argv(
        sandbox_mode,
        cwd,
        &[],
        shell,
        &[arg.to_string(), command.to_string()],
    )
    .ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn apply_sandbox(_cmd: &mut Command, _mode: PermissionMode, _cwd: &Path) {}

/// Fail-closed classifier for the handful of commands that may bypass a
/// permission prompt. This is intentionally not a shell parser: anything
/// requiring shell syntax or command-specific semantic analysis is treated as
/// having side effects. In particular, every `git`, `find`, and `rg` command
/// requires approval because those tools have destructive/execution options.
fn classify_readonly(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return true;
    }

    const SHELL_CONTROL: &[char] = &['|', '&', ';', '>', '<', '`', '$', '\n', '\r'];
    if trimmed
        .chars()
        .any(|character| SHELL_CONTROL.contains(&character))
    {
        return false;
    }

    let head = trimmed.split_whitespace().next().unwrap_or("");
    matches!(
        head,
        "pwd" | "ls" | "cat" | "head" | "tail" | "wc" | "grep" | "echo"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_readonly_basic() {
        assert!(classify_readonly("ls -la"));
        assert!(classify_readonly("cat README.md"));
        assert!(!classify_readonly("git status"));
        assert!(!classify_readonly("git clean -fdx"));
        assert!(!classify_readonly("git reset --hard"));
        assert!(!classify_readonly("git push"));
        assert!(!classify_readonly("find . -delete"));
        assert!(!classify_readonly("rg --pre dangerous pattern"));
        assert!(!classify_readonly("rm -rf /"));
        assert!(!classify_readonly("echo hi | sudo tee /etc/x"));
        assert!(!classify_readonly("cat $HOME/.ssh/id_rsa"));
        assert!(classify_readonly(""));
    }

    #[test]
    fn sudo_gets_noninteractive_flag() {
        assert_eq!(
            ensure_sudo_noninteractive("sudo apt install"),
            "sudo -n apt install"
        );
        assert_eq!(
            ensure_sudo_noninteractive("sudo systemctl restart nginx"),
            "sudo -n systemctl restart nginx"
        );
        assert_eq!(
            ensure_sudo_noninteractive("  sudo make install"),
            "  sudo -n make install"
        );
        // Plain sudo with no args
        assert_eq!(ensure_sudo_noninteractive("sudo"), "sudo -n");
        // Already has -n — no change
        assert_eq!(
            ensure_sudo_noninteractive("sudo -n apt install"),
            "sudo -n apt install"
        );
        assert_eq!(
            ensure_sudo_noninteractive("sudo -E -n apt install"),
            "sudo -E -n apt install"
        );
        // Non-sudo commands pass through
        assert_eq!(ensure_sudo_noninteractive("ls -la"), "ls -la");
        assert_eq!(ensure_sudo_noninteractive("apt install"), "apt install");
        // sudo in a pipe is NOT modified (not a leading sudo)
        assert_eq!(
            ensure_sudo_noninteractive("echo foo | sudo tee /x"),
            "echo foo | sudo tee /x"
        );
    }
}
