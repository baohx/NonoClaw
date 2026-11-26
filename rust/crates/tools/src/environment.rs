//! Shell + filesystem execution backends.
//!
//! [`Environment`] abstracts *where* commands run and *which* filesystem the
//! file tools (Read/Write/Edit/Glob/Grep) touch. It is the seam that unlocks
//! sandboxed benchmark runs (mini-swe-agent style): the default
//! [`LocalEnvironment`] preserves the exact historical behavior, while
//! [`DockerEnvironment`] routes shell execution through `docker exec` and file
//! I/O through `docker cp`, so the agent can only ever mutate the container —
//! never the host.
//!
//! Path semantics: tools resolve relative paths against
//! [`Environment::effective_cwd`]. For `LocalEnvironment` that is the host cwd
//! unchanged. For `DockerEnvironment` it is the container-side `workdir`
//! (e.g. `/testbed`), and absolute paths passed by the model are assumed to
//! already be container-side paths (the SWE-bench convention).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nonoclaw_core::{Error, Result};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Default wall-clock budget for file-bridge operations (read/write/mkdir).
const FILE_OP_TIMEOUT_MS: u64 = 60_000;

/// A single file matched by [`Environment::glob`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobEntry {
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
}

/// The captured result of one command execution.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    /// Process exit code, or `-1` when the process did not report one (e.g.
    /// killed by a signal or timed out).
    pub exit_code: i32,
}

impl CommandOutput {
    /// Combined stdout+stderr, stderr appended only when non-empty. Matches the
    /// legacy Bash tool's presentation.
    pub fn combined(&self) -> String {
        if self.stderr.is_empty() {
            self.stdout.clone()
        } else if self.stdout.is_empty() {
            self.stderr.clone()
        } else {
            format!("{}\n--- stderr ---\n{}", self.stdout, self.stderr)
        }
    }
}

/// Where commands run and which filesystem the file tools touch.
/// Implementations must be `Send + Sync + Debug` so they can live inside
/// `Arc<dyn Environment>` shared across tools.
#[async_trait]
pub trait Environment: Send + Sync + std::fmt::Debug {
    /// Execute `command` through a shell with working directory `cwd` and a
    /// wall-clock `timeout_ms`. `cancel` aborts the process group promptly.
    async fn execute(
        &self,
        command: &str,
        cwd: &Path,
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> Result<CommandOutput>;

    /// The working directory that file tools should resolve relative paths
    /// against. Local keeps the host cwd; Docker returns its container workdir.
    fn effective_cwd(&self, host_cwd: &Path) -> PathBuf {
        host_cwd.to_path_buf()
    }

    /// Whether commands run on the local host (as opposed to a container or
    /// remote backend). The Bash tool uses this to keep its host-side
    /// execution discipline — shell selection, sandbox, sudo hardening — for
    /// local runs and delegate everything else to the environment.
    fn is_local(&self) -> bool {
        true
    }

    /// Read a file as raw bytes from the environment's filesystem.
    async fn read_bytes(&self, path: &Path) -> Result<Vec<u8>>;

    /// Write `content` to a file, creating parent directories as needed.
    async fn write_bytes(&self, path: &Path, content: &[u8]) -> Result<()>;

    /// Create a directory (and parents) inside the environment.
    async fn create_dir_all(&self, path: &Path) -> Result<()>;

    /// Run an argv vector directly (no shell) with working directory `cwd`.
    /// Used by Grep (`rg`) and Glob (`find`) so their argument lists can be
    /// passed through untouched into a container.
    async fn run_argv(
        &self,
        argv: &[String],
        cwd: &Path,
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> Result<CommandOutput>;

    /// Resolve a user-supplied path (absolute, `~`-prefixed, or relative to
    /// `cwd`) against this environment. Local keeps the host semantics;
    /// Docker maps `~` to `/root` and relative paths to the container workdir.
    fn resolve_user_path(&self, cwd: &Path, user_path: &str) -> PathBuf {
        crate::builtin::resolve_path(cwd, user_path)
    }

    /// Match files under `base` against a glob `pattern` and return their
    /// paths with modification times (unsorted; the caller sorts). Local uses
    /// the `glob` crate; Docker runs `find` inside the container and matches
    /// the pattern locally.
    async fn glob(&self, base: &Path, pattern: &str) -> Result<Vec<GlobEntry>>;
}

/// Execute on the local host. Historical default; behavior-identical to the
/// pre-abstraction tools.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalEnvironment;

#[async_trait]
impl Environment for LocalEnvironment {
    async fn execute(
        &self,
        command: &str,
        cwd: &Path,
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> Result<CommandOutput> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        #[cfg(windows)]
        let (shell, arg) = ("cmd", "/C");
        #[cfg(not(windows))]
        let (shell, arg) = ("bash", "-c");

        let mut argv = vec![shell.to_string(), arg.to_string(), command.to_string()];
        // `bash --login` loads the user's profile before executing -c; cmd /C
        // doesn't need it.
        #[cfg(not(windows))]
        {
            argv.insert(1, "--login".to_string());
        }
        spawn_argv(argv, Some(cwd), timeout_ms, cancel).await
    }

    async fn read_bytes(&self, path: &Path) -> Result<Vec<u8>> {
        tokio::fs::read(path).await.map_err(|e| Error::Tool {
            tool: "Read".into(),
            message: format!("{}: {e}", path.display()),
        })
    }

    async fn write_bytes(&self, path: &Path, content: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| Error::Tool {
                        tool: "Write".into(),
                        message: format!("create_dir_all {}: {e}", parent.display()),
                    })?;
            }
        }
        tokio::fs::write(path, content)
            .await
            .map_err(|e| Error::Tool {
                tool: "Write".into(),
                message: format!("{}: {e}", path.display()),
            })
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        tokio::fs::create_dir_all(path)
            .await
            .map_err(|e| Error::Tool {
                tool: "Write".into(),
                message: format!("create_dir_all {}: {e}", path.display()),
            })
    }

    async fn run_argv(
        &self,
        argv: &[String],
        cwd: &Path,
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> Result<CommandOutput> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        spawn_argv(argv.to_vec(), Some(cwd), timeout_ms, cancel).await
    }

    async fn glob(&self, base: &Path, pattern: &str) -> Result<Vec<GlobEntry>> {
        let full = join_glob_pattern(base, pattern);
        let iter = glob::glob(&full).map_err(|e| Error::Tool {
            tool: "Glob".into(),
            message: format!("invalid pattern `{pattern}`: {e}"),
        })?;
        let mut out = Vec::new();
        for entry in iter.flatten() {
            let modified = std::fs::metadata(&entry)
                .ok()
                .and_then(|m| m.modified().ok());
            out.push(GlobEntry {
                path: entry,
                modified,
            });
        }
        Ok(out)
    }
}

/// Execute inside an already-running container via `docker exec` and bridge
/// files through `docker cp`.
///
/// `container` is the container name or id; `workdir` overrides the working
/// directory inside the container (defaults to the caller-provided `cwd`
/// verbatim). For SWE-bench images `workdir` is typically `/testbed`, already
/// present in the image.
///
/// Container lifecycle (create/start/stop) is intentionally out of scope: the
/// caller (e.g. a SWE-bench harness) owns it. This type assumes a running
/// container.
#[derive(Debug, Clone)]
pub struct DockerEnvironment {
    pub container: String,
    pub workdir: Option<PathBuf>,
}

impl DockerEnvironment {
    pub fn new(container: impl Into<String>) -> Self {
        Self {
            container: container.into(),
            workdir: None,
        }
    }

    pub fn with_workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.workdir = Some(workdir.into());
        self
    }

    fn workdir_or<'a>(&'a self, cwd: &'a Path) -> &'a Path {
        self.workdir.as_deref().unwrap_or(cwd)
    }
}

#[async_trait]
impl Environment for DockerEnvironment {
    async fn execute(
        &self,
        command: &str,
        cwd: &Path,
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> Result<CommandOutput> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let workdir = self.workdir_or(cwd);
        let mut argv = vec!["docker".to_string(), "exec".to_string()];
        if !workdir.as_os_str().is_empty() {
            argv.push("-w".to_string());
            argv.push(workdir.display().to_string());
        }
        argv.push(self.container.clone());
        argv.push("bash".to_string());
        argv.push("-c".to_string());
        argv.push(command.to_string());
        spawn_argv(argv, None, timeout_ms, cancel).await
    }

    fn effective_cwd(&self, host_cwd: &Path) -> PathBuf {
        self.workdir
            .clone()
            .unwrap_or_else(|| host_cwd.to_path_buf())
    }

    fn is_local(&self) -> bool {
        false
    }

    async fn read_bytes(&self, path: &Path) -> Result<Vec<u8>> {
        let tmp =
            std::env::temp_dir().join(format!("nonoclaw-docker-read-{}", uuid::Uuid::new_v4()));
        let src = format!("{}:{}", self.container, path.display());
        let out = spawn_argv(
            vec![
                "docker".to_string(),
                "cp".to_string(),
                src,
                tmp.display().to_string(),
            ],
            None,
            FILE_OP_TIMEOUT_MS,
            CancellationToken::new(),
        )
        .await?;
        if out.exit_code != 0 {
            let detail = if out.stderr.trim().is_empty() {
                out.stdout.trim().to_string()
            } else {
                out.stderr.trim().to_string()
            };
            return Err(Error::Tool {
                tool: "Read".into(),
                message: format!("{}: {detail}", path.display()),
            });
        }
        let bytes = std::fs::read(&tmp).map_err(|e| Error::Tool {
            tool: "Read".into(),
            message: format!("{}: {e}", path.display()),
        });
        let _ = std::fs::remove_file(&tmp);
        bytes
    }

    async fn write_bytes(&self, path: &Path, content: &[u8]) -> Result<()> {
        // docker cp does not create missing parent directories.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                self.create_dir_all(parent).await?;
            }
        }
        let tmp =
            std::env::temp_dir().join(format!("nonoclaw-docker-write-{}", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, content).map_err(|e| Error::Tool {
            tool: "Write".into(),
            message: format!("{}: {e}", path.display()),
        })?;
        let dest = format!("{}:{}", self.container, path.display());
        let out = spawn_argv(
            vec![
                "docker".to_string(),
                "cp".to_string(),
                tmp.display().to_string(),
                dest,
            ],
            None,
            FILE_OP_TIMEOUT_MS,
            CancellationToken::new(),
        )
        .await;
        let _ = std::fs::remove_file(&tmp);
        let out = out?;
        if out.exit_code != 0 {
            let detail = if out.stderr.trim().is_empty() {
                out.stdout.trim().to_string()
            } else {
                out.stderr.trim().to_string()
            };
            return Err(Error::Tool {
                tool: "Write".into(),
                message: format!("{}: {detail}", path.display()),
            });
        }
        Ok(())
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        let argv = vec![
            "docker".to_string(),
            "exec".to_string(),
            self.container.clone(),
            "mkdir".to_string(),
            "-p".to_string(),
            path.display().to_string(),
        ];
        let out = spawn_argv(argv, None, FILE_OP_TIMEOUT_MS, CancellationToken::new()).await?;
        if out.exit_code != 0 {
            let detail = if out.stderr.trim().is_empty() {
                out.stdout.trim().to_string()
            } else {
                out.stderr.trim().to_string()
            };
            return Err(Error::Tool {
                tool: "Write".into(),
                message: format!("create_dir_all {}: {detail}", path.display()),
            });
        }
        Ok(())
    }

    async fn run_argv(
        &self,
        argv: &[String],
        cwd: &Path,
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> Result<CommandOutput> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let workdir = self.workdir_or(cwd);
        let mut full = vec!["docker".to_string(), "exec".to_string()];
        if !workdir.as_os_str().is_empty() {
            full.push("-w".to_string());
            full.push(workdir.display().to_string());
        }
        full.push(self.container.clone());
        full.extend(argv.iter().cloned());
        spawn_argv(full, None, timeout_ms, cancel).await
    }

    fn resolve_user_path(&self, cwd: &Path, user_path: &str) -> PathBuf {
        let trimmed = user_path.trim();
        // `~` is the container root user's home directory.
        if let Some(rest) = trimmed.strip_prefix("~/") {
            return PathBuf::from("/root").join(rest);
        }
        if trimmed == "~" {
            return PathBuf::from("/root");
        }
        let p = PathBuf::from(trimmed);
        if p.is_absolute() {
            p
        } else {
            self.effective_cwd(cwd).join(p)
        }
    }

    async fn glob(&self, base: &Path, pattern: &str) -> Result<Vec<GlobEntry>> {
        // List every regular file under `base` with its epoch mtime, then match
        // the glob pattern locally (the `glob` crate runs on the host).
        let out = self
            .run_argv(
                &[
                    "find".to_string(),
                    base.display().to_string(),
                    "-type".to_string(),
                    "f".to_string(),
                    "-printf".to_string(),
                    "%T@\t%p\n".to_string(),
                ],
                base,
                FILE_OP_TIMEOUT_MS,
                CancellationToken::new(),
            )
            .await?;
        if out.exit_code != 0 {
            let detail = if out.stderr.trim().is_empty() {
                out.stdout.trim().to_string()
            } else {
                out.stderr.trim().to_string()
            };
            return Err(Error::Tool {
                tool: "Glob".into(),
                message: format!("find {} failed: {detail}", base.display()),
            });
        }

        let full = join_glob_pattern(base, pattern);
        let matcher = glob::Pattern::new(&full).map_err(|e| Error::Tool {
            tool: "Glob".into(),
            message: format!("invalid pattern `{pattern}`: {e}"),
        })?;

        let mut entries = Vec::new();
        for line in out.stdout.lines() {
            let Some((epoch, path)) = line.split_once('\t') else {
                continue;
            };
            let path = PathBuf::from(path);
            if matcher.matches_path(&path) {
                let modified = epoch
                    .parse::<f64>()
                    .ok()
                    .map(|secs| UNIX_EPOCH + Duration::from_secs_f64(secs));
                entries.push(GlobEntry { path, modified });
            }
        }
        Ok(entries)
    }
}

/// Join a base directory and a glob pattern into a single pattern string.
/// Mirrors the Glob tool's historical behavior: an absolute or `~`-prefixed
/// pattern stands alone; otherwise the base directory is prepended.
pub(crate) fn join_glob_pattern(base: &Path, pattern: &str) -> String {
    if pattern.starts_with('/') || pattern.starts_with('~') {
        return pattern.to_string();
    }
    let base = base.display().to_string();
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        pattern.to_string()
    } else {
        format!("{base}/{pattern}")
    }
}

/// Spawn `argv` directly (no shell), drain stdout/stderr concurrently, and
/// race the `cancel` token against `timeout_ms`. Used by both Local and Docker
/// backends so their timeout/cancel/pipe-drain semantics are identical.
async fn spawn_argv(
    argv: Vec<String>,
    cwd: Option<&Path>,
    timeout_ms: u64,
    cancel: CancellationToken,
) -> Result<CommandOutput> {
    let mut parts = argv.iter();
    let program = parts
        .next()
        .ok_or_else(|| Error::Other("empty argv".into()))?;
    let mut cmd = Command::new(program);
    cmd.args(parts);
    // Close stdin so interactive commands fail-fast with EOF instead of
    // hanging until the timeout.
    cmd.stdin(std::process::Stdio::null());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }

    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::Tool {
            tool: "Bash".into(),
            message: format!("failed to spawn `{program}`: {e}"),
        })?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timeout);

    // Drain both pipes concurrently (avoids pipe-buffer deadlock); the child
    // handle stays in scope so cancel/timeout can reap it explicitly.
    let read_fut = async move {
        use tokio::io::AsyncReadExt;
        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();
        let r1 = stdout.read_to_end(&mut out_buf);
        let r2 = stderr.read_to_end(&mut err_buf);
        let _ = tokio::join!(r1, r2);
        (out_buf, err_buf)
    };
    tokio::pin!(read_fut);

    tokio::select! {
        result = &mut read_fut => match child.wait().await {
            Ok(status) => Ok(CommandOutput {
                stdout: String::from_utf8_lossy(&result.0).into_owned(),
                stderr: String::from_utf8_lossy(&result.1).into_owned(),
                exit_code: status.code().unwrap_or(-1),
            }),
            Err(e) => Err(Error::Tool {
                tool: "Bash".into(),
                message: format!("command failed: {e}"),
            }),
        },
        _ = cancel.cancelled() => {
            // kill_on_drop would fire on future drop, but reaping now avoids a
            // zombie until then.
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(Error::Cancelled)
        },
        _ = &mut timeout => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(Error::Timeout)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared lock for tests that mutate `PATH` to inject a fake `docker`.
    /// Each test holding its own static lock would NOT serialize them — this
    /// one is shared by all PATH-mutating tests in this module.
    static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn combined_omits_empty_stderr() {
        let out = CommandOutput {
            stdout: "abc".into(),
            stderr: String::new(),
            exit_code: 0,
        };
        assert_eq!(out.combined(), "abc");
    }

    #[test]
    fn combined_appends_nonempty_stderr() {
        let out = CommandOutput {
            stdout: "abc".into(),
            stderr: "err".into(),
            exit_code: 1,
        };
        assert_eq!(out.combined(), "abc\n--- stderr ---\nerr");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_executes_and_captures_output() {
        let env = LocalEnvironment;
        let out = env
            .execute(
                "printf local-env-ok",
                Path::new("/tmp"),
                5_000,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out.stdout, "local-env-ok");
        assert_eq!(out.exit_code, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_reports_exit_code_and_stderr() {
        let env = LocalEnvironment;
        let out = env
            .execute(
                "echo boom >&2; exit 3",
                Path::new("/tmp"),
                5_000,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 3);
        assert!(out.stderr.contains("boom"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_file_bridge_roundtrips() {
        let env = LocalEnvironment;
        let dir = std::env::temp_dir().join(format!("nonoclaw-env-{}", uuid::Uuid::new_v4()));
        let file = dir.join("a/b.txt");
        env.write_bytes(&file, b"hello\nworld").await.unwrap();
        assert_eq!(env.read_bytes(&file).await.unwrap(), b"hello\nworld");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_run_argv_executes_program() {
        let env = LocalEnvironment;
        let out = env
            .run_argv(
                &["printf".to_string(), "argv-ok".to_string()],
                Path::new("/tmp"),
                5_000,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out.stdout, "argv-ok");
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn docker_builder_sets_container_and_workdir() {
        let env = DockerEnvironment::new("sweb.eval.x86_64.abcd");
        assert_eq!(env.container, "sweb.eval.x86_64.abcd");
        assert!(env.workdir.is_none());

        let env = env.with_workdir("/testbed");
        assert_eq!(env.container, "sweb.eval.x86_64.abcd");
        assert_eq!(env.workdir.as_deref(), Some(Path::new("/testbed")));
    }

    #[test]
    fn docker_effective_cwd_uses_workdir() {
        let env = DockerEnvironment::new("c").with_workdir("/testbed");
        assert_eq!(
            env.effective_cwd(Path::new("/host/home/user")),
            PathBuf::from("/testbed")
        );
        let env = DockerEnvironment::new("c");
        assert_eq!(
            env.effective_cwd(Path::new("/host/home/user")),
            PathBuf::from("/host/home/user")
        );
    }

    #[test]
    fn docker_resolve_user_path_maps_tilde_and_relative() {
        let env = DockerEnvironment::new("c").with_workdir("/testbed");
        // Absolute container paths pass through untouched.
        assert_eq!(
            env.resolve_user_path(Path::new("/host/cwd"), "/testbed/src/a.py"),
            PathBuf::from("/testbed/src/a.py")
        );
        // `~` maps to the container root home.
        assert_eq!(
            env.resolve_user_path(Path::new("/host/cwd"), "~/x.py"),
            PathBuf::from("/root/x.py")
        );
        assert_eq!(
            env.resolve_user_path(Path::new("/host/cwd"), "~"),
            PathBuf::from("/root")
        );
        // Relative paths resolve against the container workdir, not host cwd.
        assert_eq!(
            env.resolve_user_path(Path::new("/host/cwd"), "src/a.py"),
            PathBuf::from("/testbed/src/a.py")
        );
        // Without a workdir, relative paths fall back to the caller cwd.
        let env = DockerEnvironment::new("c");
        assert_eq!(
            env.resolve_user_path(Path::new("/host/cwd"), "src/a.py"),
            PathBuf::from("/host/cwd/src/a.py")
        );
    }

    // Verifies the `docker exec` command construction and output capture using
    // a fake `docker` executable in PATH — no real container or daemon needed.
    #[cfg(unix)]
    #[tokio::test]
    async fn docker_environment_constructs_exec_command() {
        let _guard = PATH_LOCK.lock().unwrap();

        let dir =
            std::env::temp_dir().join(format!("nonoclaw-fake-docker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let args_file = dir.join("args.txt");
        let script = dir.join("docker");
        let args_file_str = args_file.to_string_lossy().into_owned();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{args}'\nprintf 'fake-stdout'\nprintf 'fake-stderr' >&2\nexit 0\n",
                args = args_file_str.replace('\'', "'\\''"),
            ),
        )
        .unwrap();
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(&script)
            .status()
            .unwrap();

        let old_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", dir.display(), old_path);
        std::env::set_var("PATH", &new_path);

        let env = DockerEnvironment::new("test-container").with_workdir("/testbed");
        let out = env
            .execute(
                "pytest -q",
                Path::new("/ignored"),
                5_000,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        // Restore PATH before asserting so a failure doesn't poison other tests.
        std::env::set_var("PATH", old_path);

        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout, "fake-stdout");
        assert_eq!(out.stderr, "fake-stderr");

        let args = std::fs::read_to_string(&args_file).unwrap();
        let argv: Vec<&str> = args.lines().collect();
        assert_eq!(
            argv,
            vec![
                "exec",
                "-w",
                "/testbed",
                "test-container",
                "bash",
                "-c",
                "pytest -q"
            ]
        );
    }

    // Verifies `run_argv` (used by Grep/Glob) passes the argv vector straight
    // into `docker exec -w <workdir> <container> ...` with no shell mangling.
    #[cfg(unix)]
    #[tokio::test]
    async fn docker_run_argv_constructs_exec_command() {
        let _guard = PATH_LOCK.lock().unwrap();

        let dir =
            std::env::temp_dir().join(format!("nonoclaw-fake-docker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let args_file = dir.join("args.txt");
        let script = dir.join("docker");
        let args_file_str = args_file.to_string_lossy().into_owned();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{args}'\nprintf 'fake-stdout'\nexit 0\n",
                args = args_file_str.replace('\'', "'\\''"),
            ),
        )
        .unwrap();
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(&script)
            .status()
            .unwrap();

        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", &format!("{}:{}", dir.display(), old_path));

        let env = DockerEnvironment::new("test-container").with_workdir("/testbed");
        let out = env
            .run_argv(
                &[
                    "rg".to_string(),
                    "--files-with-matches".to_string(),
                    "fn main".to_string(),
                    "/testbed/src".to_string(),
                ],
                Path::new("/ignored"),
                5_000,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        std::env::set_var("PATH", old_path);

        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout, "fake-stdout");

        let args = std::fs::read_to_string(&args_file).unwrap();
        let argv: Vec<&str> = args.lines().collect();
        assert_eq!(
            argv,
            vec![
                "exec",
                "-w",
                "/testbed",
                "test-container",
                "rg",
                "--files-with-matches",
                "fn main",
                "/testbed/src"
            ]
        );
    }
}
