//! `GET /api/logs/raw` — serve the unredacted raw API request/response
//! payloads written by the `--log-raw-api` flag.
//!
//! The `api` crate writes per-turn files under `<cwd>/.nonoclaw/logs/api/`:
//!   `<ts>-<trace>.request.json`  — full request body (prompt, image data, …)
//!   `<ts>-<trace>.resp.sse`      — raw SSE response frames verbatim
//!   `<ts>-<trace>.summary.json`  — per-turn usage accounting
//!
//! This endpoint lists those files (newest first) and returns a single file's
//! content on demand. Api keys are NEVER present in these files (headers only),
//! so serving them is safe for diagnostics. However, payloads DO contain
//! complete prompts, so this endpoint is deliberately **opt-in**: it only
//! works when the server was started with `--log-raw-api`; otherwise the
//! endpoint returns a friendly empty state.
//!
//! The directory is resolved relative to the server's `cwd` (AppState.cwd),
//! which matches the `current_dir` the api crate uses when writing logs.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::connection::AppState;

/// List response (newest first).
#[derive(Serialize)]
pub struct RawLogList {
    /// Whether the server was started with `--log-raw-api`.
    pub enabled: bool,
    /// Directory the endpoint scans (for operator visibility).
    pub dir: String,
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

fn logs_dir(state: &Arc<AppState>) -> PathBuf {
    state.cwd().join(".nonoclaw/logs/api")
}

/// Classify a log file's kind from its filename.
fn classify(file: &str) -> &'static str {
    if file.ends_with(".request.json") {
        "request"
    } else if file.ends_with(".summary.json") {
        "summary"
    } else if file.ends_with(".resp.sse") {
        "resp"
    } else {
        "other"
    }
}

/// Parse `<ts>-<trace>` from a filename stem like `<ts>-<trace>.request.json`.
fn parse_stem(file: &str) -> (u64, String) {
    let stem = file
        .strip_suffix(".request.json")
        .or_else(|| file.strip_suffix(".summary.json"))
        .or_else(|| file.strip_suffix(".resp.sse"))
        .unwrap_or(file);
    match stem.split_once('-') {
        Some((ts, trace)) => (ts.parse().unwrap_or(0), trace.to_string()),
        None => (0, stem.to_string()),
    }
}

fn human_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// `GET /api/logs/raw` — list raw API log entries (newest first).
pub async fn list_raw_logs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Response {
    if !state.authorized(q.token.as_deref()) {
        return unauthorized();
    }
    let dir = logs_dir(&state);
    let limit = q.limit.unwrap_or(50).min(200);

    let mut entries = Vec::new();
    if let Ok(read) = fs::read_dir(&dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(file) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let (ts_ms, trace) = parse_stem(file);
            let size = fs::metadata(&path).map(|m| m.len() as usize).unwrap_or(0);
            entries.push(RawLogEntry {
                file: file.to_string(),
                kind: classify(file).to_string(),
                ts_ms,
                trace,
                size: human_size(size),
            });
        }
    }
    // Newest first; tie-break by filename for determinism.
    entries.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms).then(a.file.cmp(&b.file)));
    entries.truncate(limit);

    Json(RawLogList {
        enabled: raw_api_log_enabled_for_state(&state),
        dir: dir.to_string_lossy().to_string(),
        entries,
    })
    .into_response()
}

/// `GET /api/logs/raw/:file` — return a single log file's content.
pub async fn get_raw_log(
    State(state): State<Arc<AppState>>,
    Path(file_name): Path<String>,
    Query(q): Query<ContentQuery>,
) -> Response {
    if !state.authorized(q.token.as_deref()) {
        return unauthorized();
    }
    // Defensive: only serve basename (no path traversal).
    let safe_name: PathBuf = file_name
        .chars()
        .filter(|c| *c != '/' && *c != '\\')
        .collect::<String>()
        .into();
    let dir = logs_dir(&state);
    let path = dir.join(&safe_name);
    if !path.is_file() {
        return err_response(
            StatusCode::NOT_FOUND,
            format!("no such raw log: {}", safe_name.to_string_lossy()),
        );
    }
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to read {}: {e}", safe_name.to_string_lossy()),
            )
        }
    };
    Json(RawLogContent {
        file: safe_name.to_string_lossy().to_string(),
        content,
    })
    .into_response()
}

fn unauthorized() -> Response {
    err_response(
        StatusCode::UNAUTHORIZED,
        "invalid or missing auth token".to_string(),
    )
}

fn err_response(status: StatusCode, msg: String) -> Response {
    let body = serde_json::json!({ "error": msg });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Whether the running server has raw API logging enabled. The api crate
/// reads `NONOCLAW_RAW_API_LOG` per request; mirror that check for the UI.
fn raw_api_log_enabled_for_state(_state: &Arc<AppState>) -> bool {
    std::env::var("NONOCLAW_RAW_API_LOG").map(|v| v == "1").unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::routing::get;
    use axum::Router;
    use nonoclaw_engine::load_resolved_config;
    use reqwest::StatusCode;

    use super::*;
    use crate::serve_http::connection::upload_exploration_state;

    /// Build a bare router with only the log endpoints, scoped to a temp cwd.
    async fn log_router(cwd: &std::path::Path) -> SocketAddr {
        let settings_path = cwd.join("settings.json");
        let config = Arc::new(load_resolved_config(cwd, Some(&settings_path), None));
        let state = upload_exploration_state(cwd.to_path_buf(), config, cwd.join("uploads"));
        let router = Router::new()
            .route("/api/logs/raw", get(list_raw_logs))
            .route("/api/logs/raw/:file", get(get_raw_log))
            .with_state(state);
        // Bind on an ephemeral port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn lists_newest_first_and_serves_file_content() {
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

        let addr = log_router(temp.path()).await;
        let base = format!("http://{addr}/api/logs/raw");

        let client = reqwest::Client::new();
        // Loopback + require_auth=false → no token needed.
        let list: serde_json::Value = client
            .get(&base)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list["enabled"], false, "server not started with --log-raw-api");
        let entries = list["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3, "all three files listed");
        // Newest first: summary (0003) → resp (0002) → request (0001).
        assert!(entries[0]["file"].as_str().unwrap().ends_with("summary.json"));
        assert!(entries[1]["file"].as_str().unwrap().ends_with("resp.sse"));
        assert!(entries[2]["file"].as_str().unwrap().ends_with("request.json"));

        // Content endpoint returns the request body verbatim.
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
    async fn rejects_path_traversal_and_missing_file() {
        let temp = tempfile::tempdir().unwrap();
        let log_dir = temp.path().join(".nonoclaw/logs/api");
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(log_dir.join("a.request.json"), "{}").unwrap();

        let addr = log_router(temp.path()).await;
        let client = reqwest::Client::new();
        let base = format!("http://{addr}/api/logs/raw");

        // Missing file → 404.
        let resp = client
            .get(format!("{base}/does-not-exist.request.json"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Path traversal in filename is neutralised to a safe basename → 404
        // (the sanitized name won't exist in the dir).
        let resp = client
            .get(format!("{base}/..%2Fsecret"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
