//! Authenticated, path-confined workspace file downloads.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Form, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use nonoclaw_core::{AppError, ErrorCode};
use serde::Deserialize;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use super::connection::AppState;
use super::http_error::error_response;

const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_BUFFER_BYTES: usize = 64 * 1024;
const MAX_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
pub(super) struct DownloadRequest {
    token: Option<String>,
    path: Option<String>,
}

pub(super) async fn download_handler(
    State(state): State<Arc<AppState>>,
    Form(request): Form<DownloadRequest>,
) -> Response {
    // This check intentionally precedes every operation involving request.path.
    if !state.download_authorized(request.token.as_deref()) {
        return download_error(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Authentication,
            "invalid or missing auth token",
            "download_authentication",
            serde_json::json!({}),
        );
    }
    let Some(requested_path) = request.path.as_deref() else {
        return download_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidRequest,
            "download path is required",
            "download_validate",
            serde_json::json!({}),
        );
    };

    let max_bytes = env_u64("NONOCLAW_DOWNLOAD_MAX_BYTES", DEFAULT_MAX_BYTES);
    let buffer_bytes = env_usize(
        "NONOCLAW_DOWNLOAD_STREAM_BUFFER_BYTES",
        DEFAULT_BUFFER_BYTES,
    )
    .clamp(1, MAX_BUFFER_BYTES);
    let path = match confined_regular_file(&state.cwd, requested_path).await {
        Ok(path) => path,
        Err(response) => return response,
    };
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => {
            return download_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidRequest,
                "download target is not a regular file",
                "download_validate",
                serde_json::json!({}),
            )
        }
        Err(_) => return not_found(),
    };
    if metadata.len() > max_bytes {
        return download_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::PayloadTooLarge,
            "file exceeds the download limit",
            "download_validate",
            serde_json::json!({ "actual_bytes": metadata.len(), "max_bytes": max_bytes }),
        );
    }
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => return not_found(),
    };
    let filename = content_disposition_filename(&path);
    let stream = ReaderStream::with_capacity(file, buffer_bytes);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, metadata.len().to_string())
        .header(header::CONTENT_DISPOSITION, filename)
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff")
        .body(Body::from_stream(stream))
        .expect("download response headers are validated")
}

async fn confined_regular_file(root: &Path, requested: &str) -> Result<PathBuf, Response> {
    let relative = Path::new(requested);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(path_denied());
    }
    let canonical_root = tokio::fs::canonicalize(root)
        .await
        .map_err(|_| not_found())?;
    let joined = canonical_root.join(relative);
    let canonical = tokio::fs::canonicalize(joined)
        .await
        .map_err(|_| not_found())?;
    if !canonical.starts_with(&canonical_root) {
        return Err(path_denied());
    }
    Ok(canonical)
}

fn content_disposition_filename(path: &Path) -> String {
    let original = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    let cleaned: String = original
        .chars()
        .filter(|character| !character.is_control() && !matches!(character, '/' | '\\' | '"' | ';'))
        .collect();
    let cleaned = if cleaned.is_empty() || matches!(cleaned.as_str(), "." | "..") {
        "download"
    } else {
        cleaned.as_str()
    };
    let ascii: String = cleaned
        .chars()
        .map(|character| if character.is_ascii() { character } else { '_' })
        .collect();
    let ascii = if ascii.is_empty() || matches!(ascii.as_str(), "." | "..") {
        "download"
    } else {
        ascii.as_str()
    };
    if cleaned.is_ascii() {
        format!("attachment; filename=\"{ascii}\"")
    } else {
        format!(
            "attachment; filename=\"{ascii}\"; filename*=UTF-8''{}",
            rfc5987_encode(cleaned)
        )
    }
}

fn rfc5987_encode(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'!'
            | b'#'
            | b'$'
            | b'&'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~' => (*byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn env_u64(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

fn env_usize(name: &str, fallback: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

fn not_found() -> Response {
    download_error(
        StatusCode::NOT_FOUND,
        ErrorCode::NotFound,
        "download target was not found",
        "download_validate",
        serde_json::json!({}),
    )
}

fn path_denied() -> Response {
    download_error(
        StatusCode::FORBIDDEN,
        ErrorCode::PathDenied,
        "download path is outside the workspace",
        "download_validate",
        serde_json::json!({ "reason": "path_denied" }),
    )
}

fn download_error(
    status: StatusCode,
    code: ErrorCode,
    message: &'static str,
    operation: &'static str,
    safe_details: serde_json::Value,
) -> Response {
    error_response(
        status,
        AppError::new(code, message, false, operation)
            .with_safe_details(safe_details)
            .with_trace_id(Uuid::new_v4().to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_sanitizes_and_encodes_names() {
        let header = content_disposition_filename(Path::new("报告\";x.txt"));
        assert!(header.contains("filename=\"__x.txt\""));
        assert!(header.contains("filename*=UTF-8''%E6%8A%A5%E5%91%8Ax.txt"));
        assert!(!header.contains('"') || header.matches('"').count() == 2);
        assert!(!header.contains(";x"));
    }

    #[tokio::test]
    async fn confined_path_rejects_traversal_and_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        assert!(confined_regular_file(root.path(), "../secret")
            .await
            .is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", root.path().join("escape")).unwrap();
            assert!(confined_regular_file(root.path(), "escape").await.is_err());
        }
    }

    #[tokio::test]
    async fn endpoint_authenticates_before_path_access_and_streams_exact_bytes() {
        use axum::{routing::post, Router};

        let root = tempfile::tempdir().unwrap();
        let payload = b"exact download bytes\0\xff";
        std::fs::write(root.path().join("artifact.bin"), payload).unwrap();
        let config = std::sync::Arc::new(nonoclaw_engine::load_resolved_config(
            root.path(),
            None,
            None,
        ));
        let state = super::super::connection::upload_exploration_state(
            root.path().to_path_buf(),
            config,
            root.path().join("uploads"),
        );
        let app = Router::new()
            .route("/api/download", post(download_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();

        let unauthorized = client
            .post(format!("http://{address}/api/download"))
            .form(&[("path", "../outside")])
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let error = unauthorized.text().await.unwrap();
        assert!(error.contains("authentication"));
        assert!(!error.contains("outside"));

        let response = client
            .post(format!("http://{address}/api/download"))
            .form(&[("token", "exploration-token"), ("path", "artifact.bin")])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            payload.len().to_string()
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.bytes().await.unwrap().as_ref(), payload);
        server.abort();
    }

    #[test]
    fn configured_limit_allows_exact_boundary() {
        let size = DEFAULT_MAX_BYTES;
        assert!(!(size > DEFAULT_MAX_BYTES));
        assert!(size + 1 > DEFAULT_MAX_BYTES);
    }
}
