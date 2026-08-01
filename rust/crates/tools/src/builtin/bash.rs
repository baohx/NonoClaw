//! Bash tool. Mirrors `src/tools/BashTool/`. Spawns the command in a shell,
//! captures combined output, and enforces a timeout. Oversized output is
//! normalized by the canonical `ToolExecutor`, which preserves the complete
//! result in a local reference.
//!
//! The ML command classifier remains conservative and local; permission mode
//! and configured rules are composed by the shared permission gate.

use std::time::Duration;

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionDecision, PermissionResult, Result};
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
        // Conservative: treat nothing as read-only unless explicitly classified.
        // A real classifier lives in `src/utils/permissions/bashClassifier.ts`.
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
        let (shell, arg) = ("bash", "-c");

        let mut cmd = Command::new(shell);
        // `bash --login` loads the user's profile before executing -c; cmd /C
        // doesn't need it.
        #[cfg(not(windows))]
        {
            cmd.arg("--login");
        }
        cmd.arg(arg).arg(command);
        // Close stdin so interactive commands (sudo, ssh, passwd, etc.)
        // fail-fast with EOF instead of hanging until the timeout. The agent
        // should use non-interactive flags (-n, --yes, --non-interactive) or
        // inline input via heredoc/piping instead.
        cmd.stdin(std::process::Stdio::null());

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

/// Very small read-only heuristic for common harmless commands. Not a security
/// boundary — the real classifier is deferred. Conservative: any shell
/// metacharacter, chaining operator, or risky subcommand -> treat as mutating.
fn classify_readonly(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return true;
    }
    const RISKY: &[&str] = &[
        "|", ">", ">>", "&&", "||", ";", "$(", "sudo", "rm ", "mv ", "cp ", "mkdir ", "touch ",
        "chmod", "chown", "kill", "shutdown", "reboot", "dd ",
    ];
    if RISKY.iter().any(|t| trimmed.contains(t)) {
        return false;
    }
    let head = trimmed.split_whitespace().next().unwrap_or("");
    const READONLY: &[&str] = &[
        "ls", "cat", "head", "tail", "wc", "pwd", "echo", "grep", "rg", "find", "git",
    ];
    READONLY.contains(&head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_readonly_basic() {
        assert!(classify_readonly("ls -la"));
        assert!(classify_readonly("git status"));
        assert!(!classify_readonly("rm -rf /"));
        assert!(!classify_readonly("echo hi | sudo tee /etc/x"));
        assert!(classify_readonly(""));
    }

    #[test]
    fn sudo_gets_noninteractive_flag() {
        assert_eq!(ensure_sudo_noninteractive("sudo apt install"), "sudo -n apt install");
        assert_eq!(ensure_sudo_noninteractive("sudo systemctl restart nginx"), "sudo -n systemctl restart nginx");
        assert_eq!(ensure_sudo_noninteractive("  sudo make install"), "  sudo -n make install");
        // Plain sudo with no args
        assert_eq!(ensure_sudo_noninteractive("sudo"), "sudo -n");
        // Already has -n — no change
        assert_eq!(ensure_sudo_noninteractive("sudo -n apt install"), "sudo -n apt install");
        assert_eq!(ensure_sudo_noninteractive("sudo -E -n apt install"), "sudo -E -n apt install");
        // Non-sudo commands pass through
        assert_eq!(ensure_sudo_noninteractive("ls -la"), "ls -la");
        assert_eq!(ensure_sudo_noninteractive("apt install"), "apt install");
        // sudo in a pipe is NOT modified (not a leading sudo)
        assert_eq!(ensure_sudo_noninteractive("echo foo | sudo tee /x"), "echo foo | sudo tee /x");
    }
}
