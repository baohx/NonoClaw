//! `GET /api/logs/raw` — serve the unredacted raw API request/response
//! payloads written by the `--log-raw-api` flag.
//!
//! The `api` crate writes per-turn files under `<cwd>/.nonoclaw/logs/api/`:
//!   `<ts>-<trace>.request.json`  — full request body (prompt, image data, …)
//!   `<ts>-<trace>.resp.sse`      — raw SSE response frames verbatim
//!   `<ts>-<trace>.summary.json`  — per-turn usage accounting
//!
//! This endpoint lists those files (newest first) and returns a single file's
//! content on demand. API keys are NEVER present in these files (headers only),
//! but payloads DO contain complete prompts, so this endpoint is deliberately
//! **opt-in**: no directory access occurs unless raw logging is enabled.
//!
//! Reads are rooted at the active project. Linux uses descriptor-relative,
//! no-follow opens for every directory component and file. Other targets fail
//! closed until they have an equivalent handle-relative backend. Listing and
//! content reads are both bounded.

use std::cmp::Ordering;
use std::io;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use super::connection::AppState;

const DEFAULT_MAX_CONTENT_BYTES: u64 = 8 * 1024 * 1024;
const HARD_MAX_CONTENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LOG_FILE_NAME_BYTES: usize = 160;
const MAX_SCANNED_ENTRIES: usize = 4096;
const LOG_DIR_COMPONENTS: [&str; 3] = [".nonoclaw", "logs", "api"];

/// List response (newest first).
#[derive(Serialize)]
pub struct RawLogList {
    /// Whether the server was started with `--log-raw-api`.
    pub enabled: bool,
    /// Directory the endpoint scans (for operator visibility).
    pub dir: String,
    /// True when the response or scan budget omitted otherwise valid entries.
    pub truncated: bool,
    pub entries: Vec<RawLogEntry>,
}

#[derive(Serialize)]
pub struct RawLogEntry {
    /// Filename, e.g. `1699999999999-run.request.json`.
    pub file: String,
    /// Extension-derived kind: request | resp | summary.
    pub kind: String,
    /// Unix ms from the filename stem.
    pub ts_ms: u64,
    /// Trace label from the filename stem (`<ts>-<trace>.kind.json`).
    pub trace: String,
    /// Human-readable size, e.g. `1.2 KB`.
    pub size: String,
}

#[derive(Serialize)]
pub struct RawLogContent {
    pub file: String,
    pub content: String,
}

#[derive(Deserialize)]
pub struct ListQuery {
    /// Optional limit on the number of entries returned.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Auth token for remote/mobile access (ignored on loopback).
    #[serde(default)]
    pub token: Option<String>,
}

/// Query struct for the single-file content endpoint (token only).
#[derive(Deserialize)]
pub struct ContentQuery {
    /// Auth token for remote/mobile access (ignored on loopback).
    #[serde(default)]
    pub token: Option<String>,
}

struct OpenLogDirectory {
    display_path: PathBuf,
    access_path: PathBuf,
    #[cfg(target_os = "linux")]
    handle: std::fs::File,
}

/// Parse the exact filename contract emitted by `RawApiLogger`.
fn parse_log_file_name(file: &str) -> Option<(u64, String, &'static str)> {
    if file.is_empty() || file.len() > MAX_LOG_FILE_NAME_BYTES || file.contains(['/', '\\']) {
        return None;
    }
    let (stem, kind) = if let Some(stem) = file.strip_suffix(".request.json") {
        (stem, "request")
    } else if let Some(stem) = file.strip_suffix(".summary.json") {
        (stem, "summary")
    } else if let Some(stem) = file.strip_suffix(".resp.sse") {
        (stem, "resp")
    } else {
        return None;
    };
    let (timestamp, trace) = stem.split_once('-')?;
    if timestamp.is_empty()
        || trace.is_empty()
        || trace.len() > 96
        || !trace
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    Some((timestamp.parse().ok()?, trace.to_string(), kind))
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Ordering used by the response: newest timestamp first, then filename.
fn response_order(left: &RawLogEntry, right: &RawLogEntry) -> Ordering {
    right
        .ts_ms
        .cmp(&left.ts_ms)
        .then(left.file.cmp(&right.file))
}

async fn open_log_directory(project_root: PathBuf) -> io::Result<OpenLogDirectory> {
    tokio::task::spawn_blocking(move || open_log_directory_sync(&project_root))
        .await
        .map_err(|error| io::Error::other(format!("raw-log directory task failed: {error}")))?
}

#[cfg(target_os = "linux")]
fn open_log_directory_sync(project_root: &FsPath) -> io::Result<OpenLogDirectory> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    fn open_directory_path(path: &FsPath) -> io::Result<std::fs::File> {
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        // SAFETY: `path` is a valid NUL-terminated string. The returned owned
        // descriptor is checked before conversion and then managed by `File`.
        let descriptor = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `descriptor` is newly owned after the successful `open`.
        Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
    }

    fn open_directory_at(parent: &std::fs::File, component: &str) -> io::Result<std::fs::File> {
        let component = CString::new(component)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        // SAFETY: both the parent descriptor and component pointer are valid;
        // O_NOFOLLOW rejects a symlink at every traversed component.
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `descriptor` is newly owned after the successful `openat`.
        Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
    }

    // Bind the ambient open to the exact project-root inode selected by the
    // caller. This catches ancestor swaps between path resolution and open;
    // all fixed descendants are then traversed relative to the retained fd.
    let expected_root = std::fs::symlink_metadata(project_root)?;
    if !expected_root.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "raw-log project root is not a direct directory",
        ));
    }
    let canonical_root = std::fs::canonicalize(project_root)?;
    let mut handle = open_directory_path(&canonical_root)?;
    let opened_root = handle.metadata()?;
    if expected_root.dev() != opened_root.dev() || expected_root.ino() != opened_root.ino() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "raw-log project root changed while opening",
        ));
    }
    for component in LOG_DIR_COMPONENTS {
        handle = open_directory_at(&handle, component)?;
    }
    let access_path = PathBuf::from(format!("/proc/self/fd/{}", handle.as_raw_fd()));
    // Fail closed on Linux environments without procfs rather than falling
    // back to a replaceable pathname after opening the trusted descriptor.
    std::fs::read_dir(&access_path)?;
    Ok(OpenLogDirectory {
        display_path: canonical_root.join(".nonoclaw/logs/api"),
        access_path,
        handle,
    })
}

#[cfg(not(target_os = "linux"))]
fn open_log_directory_sync(_project_root: &FsPath) -> io::Result<OpenLogDirectory> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "raw API log viewing requires a handle-relative filesystem backend",
    ))
}

async fn open_log_file(
    directory: &OpenLogDirectory,
    file_name: &str,
) -> io::Result<tokio::fs::File> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};

        let directory = directory.handle.try_clone()?;
        let file_name = file_name.to_string();
        let file = tokio::task::spawn_blocking(move || {
            let file_name = CString::new(file_name).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "filename contains NUL")
            })?;
            // O_NONBLOCK prevents a raced FIFO/device entry from blocking
            // before fstat can reject it as non-regular.
            // SAFETY: the directory descriptor and C string remain valid for
            // the call; a successful descriptor is transferred to `File`.
            let descriptor = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    file_name.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                )
            };
            if descriptor < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `descriptor` is newly owned after successful `openat`.
            let file = unsafe { std::fs::File::from_raw_fd(descriptor) };
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "raw-log entry is not a regular file",
                ));
            }
            Ok(file)
        })
        .await
        .map_err(|error| io::Error::other(format!("raw-log open task failed: {error}")))??;
        return Ok(tokio::fs::File::from_std(file));
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (directory, file_name);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "raw API log viewing requires a handle-relative filesystem backend",
        ))
    }
}

fn empty_list(enabled: bool, dir: &FsPath) -> Response {
    sensitive_json(RawLogList {
        enabled,
        dir: dir.to_string_lossy().to_string(),
        truncated: false,
        entries: Vec::new(),
    })
}

/// `GET /api/logs/raw` — list raw API log entries (newest first).
pub async fn list_raw_logs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> Response {
    if !state.authorized(query.token.as_deref()) {
        return unauthorized();
    }

    let project_root = state.cwd();
    let display_dir = project_root.join(".nonoclaw/logs/api");
    if !raw_api_log_enabled() {
        return empty_list(false, &display_dir);
    }

    let directory = match open_log_directory(project_root).await {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return empty_list(true, &display_dir)
        }
        Err(error) => {
            tracing::warn!(kind = ?error.kind(), "raw API log directory failed confinement");
            return err_response(
                StatusCode::FORBIDDEN,
                "raw API log directory is not safely accessible",
            );
        }
    };
    let limit = query.limit.unwrap_or(50).min(200);
    let mut entries = Vec::with_capacity(limit);
    let mut truncated = false;
    let mut scanned = 0usize;
    let mut reader = match tokio::fs::read_dir(&directory.access_path).await {
        Ok(reader) => reader,
        Err(error) => {
            tracing::warn!(kind = ?error.kind(), "raw API log directory cannot be read");
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "raw API log directory is unavailable",
            );
        }
    };

    loop {
        let entry = match reader.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(kind = ?error.kind(), "raw API log directory iteration failed");
                return err_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "raw API log directory is unavailable",
                );
            }
        };
        scanned += 1;
        if scanned > MAX_SCANNED_ENTRIES {
            truncated = true;
            break;
        }

        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        let Some((ts_ms, trace, kind)) = parse_log_file_name(&file_name) else {
            continue;
        };
        let mut candidate = RawLogEntry {
            file: file_name,
            kind: kind.to_string(),
            ts_ms,
            trace,
            size: String::new(),
        };

        let replace_index = if entries.len() < limit {
            None
        } else if limit == 0 {
            truncated = true;
            continue;
        } else {
            let (worst_index, worst) = entries
                .iter()
                .enumerate()
                .max_by(|(_, left), (_, right)| response_order(left, right))
                .expect("non-empty bounded raw-log list");
            truncated = true;
            if response_order(&candidate, worst).is_lt() {
                Some(worst_index)
            } else {
                continue;
            }
        };

        let file = match open_log_file(&directory, &candidate.file).await {
            Ok(file) => file,
            Err(_) => continue,
        };
        let metadata = match file.metadata().await {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        candidate.size = human_size(metadata.len());
        if let Some(index) = replace_index {
            entries[index] = candidate;
        } else {
            entries.push(candidate);
        }
    }

    entries.sort_by(response_order);
    sensitive_json(RawLogList {
        enabled: true,
        dir: directory.display_path.to_string_lossy().to_string(),
        truncated,
        entries,
    })
}

/// `GET /api/logs/raw/:file` — return a single log file's content.
pub async fn get_raw_log(
    State(state): State<Arc<AppState>>,
    Path(file_name): Path<String>,
    Query(query): Query<ContentQuery>,
) -> Response {
    if !state.authorized(query.token.as_deref()) {
        return unauthorized();
    }
    if !raw_api_log_enabled() || parse_log_file_name(&file_name).is_none() {
        return not_found();
    }

    let directory = match open_log_directory(state.cwd()).await {
        Ok(directory) => directory,
        Err(_) => return not_found(),
    };
    let file = match open_log_file(&directory, &file_name).await {
        Ok(file) => file,
        Err(_) => return not_found(),
    };
    let max_bytes = raw_log_content_limit();
    let metadata = match file.metadata().await {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return not_found(),
    };
    if metadata.len() > max_bytes {
        return err_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "raw API log exceeds the viewing limit",
        );
    }

    let mut bytes = Vec::with_capacity(metadata.len().min(max_bytes) as usize);
    let mut bounded = file.take(max_bytes.saturating_add(1));
    if let Err(error) = bounded.read_to_end(&mut bytes).await {
        tracing::warn!(kind = ?error.kind(), "raw API log read failed");
        return err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "raw API log could not be read",
        );
    }
    if bytes.len() as u64 > max_bytes {
        return err_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "raw API log exceeds the viewing limit",
        );
    }
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(_) => {
            return err_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "raw API log is not valid UTF-8",
            )
        }
    };

    sensitive_json(RawLogContent {
        file: file_name,
        content,
    })
}

fn unauthorized() -> Response {
    err_response(StatusCode::UNAUTHORIZED, "invalid or missing auth token")
}

fn not_found() -> Response {
    err_response(StatusCode::NOT_FOUND, "raw API log not found")
}

fn err_response(status: StatusCode, message: &'static str) -> Response {
    no_store((status, Json(serde_json::json!({ "error": message }))).into_response())
}

fn sensitive_json(value: impl Serialize) -> Response {
    no_store(Json(value).into_response())
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Whether the process-wide raw API logger is enabled. Keep this parser in
/// lockstep with the API crate's `raw_api_log_enabled` implementation.
pub(super) fn raw_api_log_enabled() -> bool {
    std::env::var_os("NONOCLAW_RAW_API_LOG").is_some_and(|value| {
        matches!(
            value.to_string_lossy().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
}

fn raw_log_content_limit() -> u64 {
    std::env::var("NONOCLAW_RAW_API_LOG_MAX_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_MAX_CONTENT_BYTES)
        .min(HARD_MAX_CONTENT_BYTES)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::net::SocketAddr;
    use std::sync::Mutex;

    use axum::routing::get;
    use axum::Router;
    use nonoclaw_engine::load_resolved_config;
    use reqwest::StatusCode;

    use super::*;
    use crate::serve_http::connection::upload_exploration_state;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct RawLogEnvGuard(Option<OsString>);

    impl RawLogEnvGuard {
        fn disabled() -> Self {
            let previous = std::env::var_os("NONOCLAW_RAW_API_LOG");
            std::env::remove_var("NONOCLAW_RAW_API_LOG");
            Self(previous)
        }

        fn enabled() -> Self {
            let previous = std::env::var_os("NONOCLAW_RAW_API_LOG");
            std::env::set_var("NONOCLAW_RAW_API_LOG", "1");
            Self(previous)
        }
    }

    impl Drop for RawLogEnvGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                std::env::set_var("NONOCLAW_RAW_API_LOG", previous);
            } else {
                std::env::remove_var("NONOCLAW_RAW_API_LOG");
            }
        }
    }

    /// Build a bare router with only the log endpoints, scoped to a temp cwd.
    async fn log_router(cwd: &std::path::Path) -> SocketAddr {
        let settings_path = cwd.join("settings.json");
        let config = Arc::new(load_resolved_config(cwd, Some(&settings_path), None));
        let state = upload_exploration_state(cwd.to_path_buf(), config, cwd.join("uploads"));
        let router = Router::new()
            .route("/api/logs/raw", get(list_raw_logs))
            .route("/api/logs/raw/:file", get(get_raw_log))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn disabled_is_empty_then_enabled_lists_and_serves_known_logs() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = RawLogEnvGuard::disabled();
        let temp = tempfile::tempdir().unwrap();
        let log_dir = temp.path().join(".nonoclaw/logs/api");
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("1700000000001-run-a.request.json"),
            r#"{"model":"x","messages":[{"role":"user","content":"hello"}]}"#,
        )
        .unwrap();
        std::fs::write(
            log_dir.join("1700000000002-run-b.resp.sse"),
            "data: {\"delta\":\"hi\"}\n\n",
        )
        .unwrap();
        std::fs::write(
            log_dir.join("1700000000003-run-c.summary.json"),
            r#"{"input_tokens":10,"output_tokens":3}"#,
        )
        .unwrap();
        std::fs::write(log_dir.join("1700000000004-run-d.debug.json"), "ignored").unwrap();

        let addr = log_router(temp.path()).await;
        let base = format!("http://{addr}/api/logs/raw");
        let client = reqwest::Client::new();

        let disabled: serde_json::Value = client
            .get(&base)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(disabled["enabled"], false);
        assert!(disabled["entries"].as_array().unwrap().is_empty());
        let response = client
            .get(format!("{base}/1700000000001-run-a.request.json"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        std::env::set_var("NONOCLAW_RAW_API_LOG", "1");
        let response = client.get(&base).send().await.unwrap();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let list: serde_json::Value = response.json().await.unwrap();
        assert_eq!(list["enabled"], true);
        assert_eq!(list["truncated"], false);
        let entries = list["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3, "only known raw-log formats are listed");
        assert!(entries[0]["file"]
            .as_str()
            .unwrap()
            .ends_with("summary.json"));
        assert!(entries[1]["file"].as_str().unwrap().ends_with("resp.sse"));
        assert!(entries[2]["file"]
            .as_str()
            .unwrap()
            .ends_with("request.json"));

        let content: serde_json::Value = client
            .get(format!("{base}/1700000000001-run-a.request.json"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(content["content"].as_str().unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn rejects_path_traversal_symlinks_and_missing_files() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = RawLogEnvGuard::enabled();
        let temp = tempfile::tempdir().unwrap();
        let log_dir = temp.path().join(".nonoclaw/logs/api");
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(log_dir.join("1700000000001-a.request.json"), "{}").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(
            "/etc/passwd",
            log_dir.join("1700000000002-escape.request.json"),
        )
        .unwrap();

        let addr = log_router(temp.path()).await;
        let client = reqwest::Client::new();
        let base = format!("http://{addr}/api/logs/raw");

        let response = client
            .get(format!("{base}/does-not-exist.request.json"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = client
            .get(format!("{base}/..%2Fsecret"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        #[cfg(unix)]
        {
            let response = client
                .get(format!("{base}/1700000000002-escape.request.json"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);

            let outside = tempfile::tempdir().unwrap();
            std::fs::write(
                outside.path().join("1700000000003-parent.request.json"),
                "secret",
            )
            .unwrap();
            std::fs::remove_dir_all(&log_dir).unwrap();
            std::os::unix::fs::symlink(outside.path(), &log_dir).unwrap();
            let response = client
                .get(format!("{base}/1700000000003-parent.request.json"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }
}
