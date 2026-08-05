//! Deterministic, bounded executable discovery and runtime probing.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nonoclaw_core::display_path;
use serde::Serialize;
use tokio::io::AsyncReadExt;

use crate::settings::{ExecutableEntrySetting, ExecutableSettings, ResolvedConfig};

const DEFAULT_TIMEOUT_MS: u64 = 3_000;
const DEFAULT_OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeProbeReport {
    pub fingerprint: String,
    pub completed_at_ms: u64,
    pub timeout_ms: u64,
    pub output_limit_bytes: usize,
    pub entries: Vec<ExecutableProbe>,
    pub python_venv: PythonVenvProbe,
    pub markitdown: MarkItDownProbe,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecutableProbe {
    pub name: String,
    pub status: String,
    pub path: Option<String>,
    pub version: Option<String>,
    pub expected_version: Option<String>,
    pub resolution_source: String,
    pub diagnostic: Option<String>,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PythonVenvProbe {
    pub status: String,
    pub python_path: Option<String>,
    pub required: bool,
    pub suggestion: Option<String>,
}

/// Status of the MarkItDown CLI (https://github.com/microsoft/markitdown).
/// The tool converts office documents, HTML, EPUB, and many more formats
/// to structured Markdown for LLM consumption.
#[derive(Debug, Clone, Serialize)]
pub struct MarkItDownProbe {
    pub status: String,
    /// Resolved path to the `markitdown` executable.
    pub path: Option<String>,
    pub version: Option<String>,
    pub suggestion: Option<String>,
}

const EXECUTABLE_NAMES: [&str; 9] = [
    "rust.rustc",
    "rust.cargo",
    "rust.rustup",
    "node.node",
    "node.npm",
    "node.npx",
    "node.corepack",
    "python.python",
    "python.pip",
];

pub async fn probe_runtime(config: &ResolvedConfig) -> RuntimeProbeReport {
    probe_runtime_with_updates(config, None, |_| {}).await
}

pub async fn probe_runtime_with_updates<F>(
    config: &ResolvedConfig,
    previous: Option<&RuntimeProbeReport>,
    mut on_update: F,
) -> RuntimeProbeReport
where
    F: FnMut(RuntimeProbeReport),
{
    let timeout_ms = env_number("NONOCLAW_RUNTIME_PROBE_TIMEOUT_MS", DEFAULT_TIMEOUT_MS);
    let output_limit_bytes = env_number(
        "NONOCLAW_RUNTIME_PROBE_OUTPUT_LIMIT_BYTES",
        DEFAULT_OUTPUT_LIMIT as u64,
    ) as usize;
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let output_limit = output_limit_bytes.max(128);
    let settings = config.executable_settings();
    let required = settings
        .and_then(|settings| settings.python.as_ref())
        .and_then(|python| python.require_virtual_env)
        .unwrap_or(false);
    let mut report = RuntimeProbeReport {
        fingerprint: config.executable_fingerprint(),
        completed_at_ms: previous.map_or(0, |report| report.completed_at_ms),
        timeout_ms,
        output_limit_bytes,
        entries: EXECUTABLE_NAMES
            .iter()
            .map(|name| {
                previous
                    .and_then(|report| report.entries.iter().find(|entry| entry.name == *name))
                    .cloned()
                    .unwrap_or_else(|| {
                        failed(
                            name,
                            "missing",
                            None,
                            configured_entry(settings, name)
                                .and_then(|entry| entry.version.clone()),
                            "not probed".into(),
                            "executable_missing",
                            "Runtime probe has not completed for this entry.",
                        )
                    })
            })
            .collect(),
        python_venv: previous.map_or(
            PythonVenvProbe {
                status: "missing".into(),
                python_path: None,
                required,
                suggestion: required.then(|| "Python venv capability has not been probed.".into()),
            },
            |report| report.python_venv.clone(),
        ),
        markitdown: previous.map_or(
            MarkItDownProbe {
                status: "missing".into(),
                path: None,
                version: None,
                suggestion: None,
            },
            |report| report.markitdown.clone(),
        ),
    };
    report.python_venv.required = required;

    for (index, name) in EXECUTABLE_NAMES.iter().enumerate() {
        let mut checking = report.entries[index].clone();
        checking.status = "checking".into();
        checking.diagnostic = None;
        checking.suggestion = None;
        report.entries[index] = checking;
        on_update(report.clone());
        report.entries[index] = probe_one(config, settings, name, timeout, output_limit).await;
        on_update(report.clone());
    }
    let python = report
        .entries
        .iter()
        .find(|entry| entry.name == "python.python");
    report.python_venv = probe_python_venv(python, required, timeout, output_limit).await;
    // MarkItDown is a Python CLI with a slow cold start (~3–4 s on first
    // invocation while it imports magika/charset_normalizer). The generic
    // probe budget (default 3 s) would kill `--version` before it finishes,
    // misreporting an installed tool as invalid. Give it a dedicated,
    // generous floor instead.
    let markitdown_timeout = timeout.max(Duration::from_secs(20));
    report.markitdown = probe_markitdown(settings, markitdown_timeout, output_limit).await;
    report.completed_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    on_update(report.clone());
    report
}

async fn probe_one(
    config: &ResolvedConfig,
    settings: Option<&ExecutableSettings>,
    name: &'static str,
    timeout: Duration,
    output_limit: usize,
) -> ExecutableProbe {
    let configured = configured_entry(settings, name);
    let expected = configured.and_then(|entry| entry.version.clone());
    let (candidate, source, explicit) = resolve_candidate(config, settings, name, configured);
    let Some(candidate) = candidate else {
        return failed(
            name,
            "missing",
            None,
            expected,
            source,
            "executable_missing",
            "Install the executable or configure an absolute path.",
        );
    };
    let canonical = match tokio::fs::canonicalize(&candidate).await {
        Ok(path) => path,
        Err(_) if explicit => {
            return failed(
                name,
                "invalid",
                Some(candidate),
                expected,
                source,
                "configured_path_unavailable",
                "Fix the configured path; explicit paths do not fall back to PATH.",
            )
        }
        Err(_) => {
            return failed(
                name,
                "missing",
                None,
                expected,
                source,
                "executable_missing",
                "Install the executable or configure an absolute path.",
            )
        }
    };
    if !is_executable_file(&canonical) {
        return failed(
            name,
            "invalid",
            Some(canonical),
            expected,
            source,
            "not_executable_file",
            "Point this entry to an executable regular file.",
        );
    }
    let output = match bounded_command(&canonical, &["--version"], timeout, output_limit).await {
        Ok(output) => output,
        Err(failure) => {
            return failed(
                name,
                "invalid",
                Some(canonical),
                expected,
                source,
                failure.code,
                failure.suggestion,
            )
        }
    };
    let Some(actual) = parse_version(&output) else {
        return failed(
            name,
            "invalid",
            Some(canonical),
            expected,
            source,
            "version_unrecognized",
            "Use an executable whose --version output contains a numeric version.",
        );
    };
    let comparison = expected
        .as_deref()
        .map(|value| (normalize_version(value), normalize_version(&actual)));
    if let Some((Some(expected_normalized), Some(actual_normalized))) = &comparison {
        if expected_normalized != actual_normalized {
            return failed(
                name,
                "version mismatch",
                Some(canonical),
                expected,
                source,
                "version_mismatch",
                "Install the exact configured version or update the expected version.",
            )
            .with_version(actual);
        }
    }
    let inconclusive = expected.is_some()
        && comparison.is_some_and(|(left, right)| left.is_none() || right.is_none());
    ExecutableProbe {
        name: name.into(),
        status: "available".into(),
        path: Some(display_path(&canonical)),
        version: Some(actual),
        expected_version: expected,
        resolution_source: source,
        diagnostic: inconclusive.then(|| "version_comparison_inconclusive".into()),
        suggestion: inconclusive
            .then(|| "Use a numeric exact version for deterministic comparison.".into()),
    }
}

impl ExecutableProbe {
    fn with_version(mut self, version: String) -> Self {
        self.version = Some(version);
        self
    }
}

fn failed(
    name: &str,
    status: &str,
    path: Option<PathBuf>,
    expected_version: Option<String>,
    resolution_source: String,
    diagnostic: &str,
    suggestion: &str,
) -> ExecutableProbe {
    ExecutableProbe {
        name: name.into(),
        status: status.into(),
        path: path.as_ref().map(|p| display_path(p)),
        version: None,
        expected_version,
        resolution_source,
        diagnostic: Some(diagnostic.into()),
        suggestion: Some(suggestion.into()),
    }
}

fn configured_entry<'a>(
    settings: Option<&'a ExecutableSettings>,
    name: &str,
) -> Option<&'a ExecutableEntrySetting> {
    settings?
        .entries()
        .into_iter()
        .find(|(entry, _)| *entry == name)
        .map(|(_, value)| value)
}

fn resolve_candidate(
    config: &ResolvedConfig,
    settings: Option<&ExecutableSettings>,
    name: &str,
    entry: Option<&ExecutableEntrySetting>,
) -> (Option<PathBuf>, String, bool) {
    if let Some(raw) = entry.and_then(|entry| entry.path.as_deref()) {
        let field = format!("executables.{name}.path");
        return (
            expand_configured_path(raw, config.cwd()),
            format!("configured · {}", config.executable_source_label(&field)),
            true,
        );
    }
    if let Some(primary) = primary_name(name) {
        if primary != name {
            if let Some(primary_path) =
                configured_entry(settings, primary).and_then(|entry| entry.path.as_deref())
            {
                let source = format!("{primary} runtime directory");
                if let Some(parent) = expand_configured_path(primary_path, config.cwd())
                    .and_then(|path| path.parent().map(Path::to_path_buf))
                {
                    for candidate in candidate_names(name) {
                        let path = parent.join(candidate);
                        if path.exists() {
                            return (Some(path), source, false);
                        }
                    }
                }
                return (None, source, false);
            }
        }
    }
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        for candidate in candidate_names(name) {
            let path = directory.join(candidate);
            if path.exists() {
                return (Some(path), "PATH discovery".into(), false);
            }
        }
    }
    (None, "PATH discovery".into(), false)
}

fn expand_configured_path(raw: &str, workspace: &Path) -> Option<PathBuf> {
    let expanded = if let Some(rest) = raw.strip_prefix("${HOME}") {
        nonoclaw_core::home_dir()?.join(rest.trim_start_matches(['/', '\\']))
    } else if let Some(rest) = raw.strip_prefix("${WORKSPACE}") {
        workspace.join(rest.trim_start_matches(['/', '\\']))
    } else {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else {
            // Relative paths resolve against the working directory, same as
            // `${WORKSPACE}`. This matches the mental model of portable
            // deployments where executables live next to nonoclaw.exe.
            workspace.join(p)
        }
    };
    Some(expanded)
}

fn primary_name(name: &str) -> Option<&'static str> {
    match name.split('.').next()? {
        "rust" => Some("rust.rustc"),
        "node" => Some("node.node"),
        "python" => Some("python.python"),
        _ => None,
    }
}

fn candidate_names(name: &str) -> &'static [&'static str] {
    match name {
        "rust.rustc" => &["rustc", "rustc.exe"],
        "rust.cargo" => &["cargo", "cargo.exe"],
        "rust.rustup" => &["rustup", "rustup.exe"],
        "node.node" => &["node", "node.exe"],
        "node.npm" => &["npm", "npm.cmd", "npm.exe"],
        "node.npx" => &["npx", "npx.cmd", "npx.exe"],
        "node.corepack" => &["corepack", "corepack.cmd", "corepack.exe"],
        "python.python" => &["python3", "python", "python.exe"],
        "python.pip" => &["pip3", "pip", "pip.exe"],
        _ => &[],
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[derive(Debug)]
struct ProbeFailure {
    code: &'static str,
    suggestion: &'static str,
}

async fn bounded_command(
    path: &Path,
    arguments: &[&str],
    timeout: Duration,
    limit: usize,
) -> Result<String, ProbeFailure> {
    let mut child = tokio::process::Command::new(path)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ProbeFailure {
            code: "probe_execution",
            suggestion: "Verify that the executable can start without shell setup.",
        })?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let read_limit = limit.saturating_add(1) as u64;
    let output_overflow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stdout_overflow = std::sync::Arc::clone(&output_overflow);
    let stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let result = stdout.take(read_limit).read_to_end(&mut bytes).await;
        if bytes.len() > limit {
            stdout_overflow.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        result.map(|_| bytes)
    });
    let stderr_overflow = std::sync::Arc::clone(&output_overflow);
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let result = stderr.take(read_limit).read_to_end(&mut bytes).await;
        if bytes.len() > limit {
            stderr_overflow.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        result.map(|_| bytes)
    });
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            return Err(ProbeFailure {
                code: "probe_execution",
                suggestion: "Verify that the executable can run and exit normally.",
            })
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            if output_overflow.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(ProbeFailure {
                    code: "probe_output_limit",
                    suggestion:
                        "Fix noisy startup output or increase the runtime probe output limit.",
                });
            }
            return Err(ProbeFailure {
                code: "probe_timeout",
                suggestion: "Fix the executable startup or increase the runtime probe timeout.",
            });
        }
    };
    let stdout = stdout_task
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let stderr = stderr_task
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    if stdout.len().saturating_add(stderr.len()) > limit {
        return Err(ProbeFailure {
            code: "probe_output_limit",
            suggestion: "Fix noisy startup output or increase the runtime probe output limit.",
        });
    }
    if !status.success() {
        return Err(ProbeFailure {
            code: "probe_execution",
            suggestion: "Verify that --version succeeds for this executable.",
        });
    }
    Ok(String::from_utf8_lossy(&stdout).to_string() + &String::from_utf8_lossy(&stderr))
}

fn parse_version(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        line.split_whitespace()
            .find(|token| normalize_version(token).is_some())
            .map(|token| {
                token
                    .trim_matches(|character: char| {
                        !character.is_ascii_alphanumeric()
                            && character != '.'
                            && character != '-'
                            && character != '+'
                    })
                    .to_string()
            })
    })
}

fn normalize_version(value: &str) -> Option<String> {
    let value = value.trim().trim_start_matches(['v', 'V']);
    let core = value.split(['-', '+']).next()?;
    let parts: Vec<_> = core.split('.').collect();
    (parts.len() >= 2
        && parts.iter().all(|part| {
            !part.is_empty() && part.chars().all(|character| character.is_ascii_digit())
        }))
    .then(|| core.to_string())
}

async fn probe_python_venv(
    python: Option<&ExecutableProbe>,
    required: bool,
    timeout: Duration,
    limit: usize,
) -> PythonVenvProbe {
    let path = python
        .filter(|entry| entry.status == "available")
        .and_then(|entry| entry.path.as_deref());
    let Some(path) = path else {
        return PythonVenvProbe {
            status: "missing".into(),
            python_path: python.and_then(|entry| entry.path.clone()),
            required,
            suggestion: required
                .then(|| "Configure an available Python executable with the venv module.".into()),
        };
    };
    match bounded_command(Path::new(path), &["-m", "venv", "--help"], timeout, limit).await {
        Ok(_) => PythonVenvProbe {
            status: "available".into(),
            python_path: Some(path.into()),
            required,
            suggestion: None,
        },
        Err(_) => PythonVenvProbe {
            status: if required { "invalid" } else { "missing" }.into(),
            python_path: Some(path.into()),
            required,
            suggestion: Some("Install the Python venv module for this interpreter.".into()),
        },
    }
}

async fn probe_markitdown(
    _settings: Option<&ExecutableSettings>,
    timeout: Duration,
    limit: usize,
) -> MarkItDownProbe {
    // Prefer the dedicated NonoClaw venv, then common system paths, then PATH.
    // The venv lives under the NonoClaw data dir so portable deployments can
    // point `$NONOCLAW_HOME` at a bundled `.nonoclaw/` folder next to the exe.
    let mut candidates = Vec::new();
    if let Some(data) = nonoclaw_core::nonoclaw_data_dir() {
        let venv_script = if cfg!(windows) {
            "Scripts/markitdown.exe"
        } else {
            "bin/markitdown"
        };
        candidates.push(data.join("venvs/markitdown").join(venv_script));
    }
    #[cfg(not(windows))]
    {
        candidates.push(PathBuf::from("/usr/local/bin/markitdown"));
        candidates.push(PathBuf::from("/usr/bin/markitdown"));
    }
    for candidate in candidates {
        if candidate.is_file() {
            return probe_markitdown_exec(&candidate.to_string_lossy(), timeout, limit).await;
        }
    }
    // PATH discovery: `which` on Unix, `where` on Windows.
    let finder = if cfg!(windows) { "where" } else { "which" };
    if let Ok(which) = std::process::Command::new(finder)
        .arg("markitdown")
        .output()
    {
        if which.status.success() {
            let path = String::from_utf8_lossy(&which.stdout).trim().to_string();
            if !path.is_empty() && Path::new(&path).exists() {
                return probe_markitdown_exec(&path, timeout, limit).await;
            }
        }
    }

    MarkItDownProbe {
        status: "missing".into(),
        path: None,
        version: None,
        suggestion: Some(
            "Install MarkItDown CLI: create a venv at ~/.nonoclaw/venvs/markitdown then \
             `pip install 'markitdown[pdf,docx,pptx,xlsx]'`.\
             Without it, document attachments fall back to the legacy \
             docModel OCR pipeline."
                .into(),
        ),
    }
}

async fn probe_markitdown_exec(
    path: &str,
    timeout: Duration,
    limit: usize,
) -> MarkItDownProbe {
    match bounded_command(Path::new(path), &["--version"], timeout, limit).await {
        Ok(version) => MarkItDownProbe {
            status: "available".into(),
            path: Some(path.into()),
            version: Some(version.trim().to_string()),
            suggestion: None,
        },
        Err(failure) => MarkItDownProbe {
            status: "invalid".into(),
            path: Some(path.into()),
            version: None,
            suggestion: Some(failure.suggestion.to_string()),
        },
    }
}

fn env_number(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_is_explicit_and_inconclusive_is_distinct() {
        assert_eq!(normalize_version("v20.11.1"), Some("20.11.1".into()));
        assert_eq!(normalize_version("nightly"), None);
        assert_eq!(parse_version("rustc 1.90.0 (abc)"), Some("1.90.0".into()));
    }

    #[test]
    fn configured_primary_runtime_prevents_sibling_path_fallback() {
        let layer = crate::settings::ConfigLayer::from_json(
            crate::settings::ConfigSource::BuiltIn,
            serde_json::json!({"executables":{"node":{"node":{"path":"/definitely/missing/node"}}}}),
        )
        .unwrap();
        let config = crate::settings::resolve_layers(
            &[layer],
            &crate::settings::ConfigEnvironment::default(),
            Path::new("/workspace"),
        );
        let (candidate, source, _) =
            resolve_candidate(&config, config.executable_settings(), "node.npm", None);
        assert!(candidate.is_none());
        assert_eq!(source, "node.node runtime directory");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probes_classify_output_limit_and_only_explicit_mismatch_blocks() {
        use std::os::unix::fs::PermissionsExt;

        let temp =
            std::env::temp_dir().join(format!("nonoclaw-toolchain-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp).unwrap();
        let noisy = temp.join("noisy");
        std::fs::write(
            &noisy,
            "#!/bin/sh\nwhile true; do printf '0123456789'; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&noisy, std::fs::Permissions::from_mode(0o700)).unwrap();
        let failure = bounded_command(&noisy, &["--version"], Duration::from_millis(100), 128)
            .await
            .unwrap_err();
        assert_eq!(failure.code, "probe_output_limit");

        let versioned = temp.join("node");
        std::fs::write(&versioned, "#!/bin/sh\nprintf 'node 2.0.0\\n'\n").unwrap();
        std::fs::set_permissions(&versioned, std::fs::Permissions::from_mode(0o700)).unwrap();
        let layer = crate::settings::ConfigLayer::from_json(
            crate::settings::ConfigSource::BuiltIn,
            serde_json::json!({"executables":{"node":{"node":{"path":versioned,"version":"1.0.0"}}}}),
        )
        .unwrap();
        let config = crate::settings::resolve_layers(
            &[layer],
            &crate::settings::ConfigEnvironment::default(),
            &temp,
        );
        let mismatch = probe_one(
            &config,
            config.executable_settings(),
            "node.node",
            Duration::from_secs(1),
            1024,
        )
        .await;
        assert_eq!(mismatch.status, "version mismatch");

        let layer = crate::settings::ConfigLayer::from_json(
            crate::settings::ConfigSource::BuiltIn,
            serde_json::json!({"executables":{"node":{"node":{"path":versioned,"version":"nightly"}}}}),
        )
        .unwrap();
        let config = crate::settings::resolve_layers(
            &[layer],
            &crate::settings::ConfigEnvironment::default(),
            &temp,
        );
        let inconclusive = probe_one(
            &config,
            config.executable_settings(),
            "node.node",
            Duration::from_secs(1),
            1024,
        )
        .await;
        assert_eq!(inconclusive.status, "available");
        assert_eq!(
            inconclusive.diagnostic.as_deref(),
            Some("version_comparison_inconclusive")
        );
        let _ = std::fs::remove_dir_all(temp);
    }

    #[tokio::test]
    async fn incremental_updates_mark_only_the_current_entry_checking() {
        let layer = crate::settings::ConfigLayer::from_json(
            crate::settings::ConfigSource::BuiltIn,
            serde_json::json!({"executables": {
                "rust": {
                    "rustc": {"path": "/missing/rustc"},
                    "cargo": {"path": "/missing/cargo"},
                    "rustup": {"path": "/missing/rustup"}
                },
                "node": {
                    "node": {"path": "/missing/node"},
                    "npm": {"path": "/missing/npm"},
                    "npx": {"path": "/missing/npx"},
                    "corepack": {"path": "/missing/corepack"}
                },
                "python": {
                    "python": {"path": "/missing/python"},
                    "pip": {"path": "/missing/pip"}
                }
            }}),
        )
        .unwrap();
        let config = crate::settings::resolve_layers(
            &[layer],
            &crate::settings::ConfigEnvironment::default(),
            Path::new("/workspace"),
        );
        let mut updates = Vec::new();
        let final_report =
            probe_runtime_with_updates(&config, None, |report| updates.push(report)).await;
        assert_eq!(final_report.entries.len(), EXECUTABLE_NAMES.len());
        assert!(updates.iter().any(|report| report
            .entries
            .iter()
            .any(|entry| entry.status == "checking")));
        assert!(updates.iter().all(|report| {
            report
                .entries
                .iter()
                .filter(|entry| entry.status == "checking")
                .count()
                <= 1
        }));
    }

    #[test]
    fn configured_path_expansion_never_uses_settings_directory() {
        let workspace = Path::new("/workspace");
        assert_eq!(
            expand_configured_path("${WORKSPACE}/bin/node", workspace),
            Some(PathBuf::from("/workspace/bin/node"))
        );
        // Relative paths resolve against the workspace (not settings dir), and
        // return the resolved absolute path so callers can probe/canonicalize it.
        assert_eq!(
            expand_configured_path("relative/node", workspace),
            Some(PathBuf::from("/workspace/relative/node"))
        );
        // Absolute paths pass through unchanged.
        assert_eq!(
            expand_configured_path("/usr/bin/node", workspace),
            Some(PathBuf::from("/usr/bin/node"))
        );
    }
}
