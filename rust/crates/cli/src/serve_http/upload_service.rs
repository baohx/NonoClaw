//! Attachment upload and document extraction HTTP service.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use axum::extract::{Multipart, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use nonoclaw_core::{AppError, ErrorCode};
use nonoclaw_engine::ClientPurpose;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::connection::AppState;
use super::http_error::{error_response, json_response};
use super::protocol::{ImageRef, UploadResponse};
use crate::attachments;

const MAX_FILENAME_CHARS: usize = 255;
const MAX_FILENAME_BYTES: usize = 240;
const STORED_ATTACHMENT_FILE: &str = "attachment.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct StoredAttachment {
    pub id: String,
    pub filename: String,
    pub extracted_text: String,
    #[serde(default)]
    pub images: Vec<ImageRef>,
}

pub(super) async fn upload_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    mut multipart: Multipart,
) -> Response {
    if !state.authorized(params.get("token").map(String::as_str)) {
        return upload_error(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Authentication,
            "invalid or missing auth token",
            false,
            serde_json::json!({}),
        );
    }
    let doc_model = match state.config.doc_model() {
        Some(config) if config.is_enabled() => config,
        _ => {
            return upload_error(
                StatusCode::NOT_IMPLEMENTED,
                ErrorCode::Configuration,
                "document processing is not configured",
                false,
                serde_json::json!({ "setting": "docModel" }),
            )
        }
    };

    let mut file: Option<(String, Vec<u8>)> = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => {
                return upload_error(
                    StatusCode::BAD_REQUEST,
                    ErrorCode::InvalidRequest,
                    "invalid multipart upload",
                    false,
                    serde_json::json!({}),
                )
            }
        };
        if field.name() != Some("file") {
            continue;
        }
        if file.is_some() {
            return upload_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidRequest,
                "exactly one file is allowed per upload",
                false,
                serde_json::json!({}),
            );
        }
        let filename = field.file_name().unwrap_or("untitled").to_string();
        if filename.chars().count() > MAX_FILENAME_CHARS {
            return upload_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidRequest,
                "filename is too long",
                false,
                serde_json::json!({ "max_filename_chars": MAX_FILENAME_CHARS }),
            );
        }
        let mut bytes = Vec::new();
        let mut field = field;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len().saturating_add(chunk.len()) > attachments::MAX_FILE_SIZE as usize
                    {
                        return upload_error(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            ErrorCode::PayloadTooLarge,
                            "file exceeds the upload limit",
                            false,
                            serde_json::json!({
                                "max_bytes": attachments::MAX_FILE_SIZE
                            }),
                        );
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => {
                    return upload_error(
                        StatusCode::BAD_REQUEST,
                        ErrorCode::InvalidRequest,
                        "upload stream was interrupted",
                        true,
                        serde_json::json!({}),
                    )
                }
            }
        }
        file = Some((filename, bytes));
    }

    let Some((filename, file_bytes)) = file.filter(|(_, bytes)| !bytes.is_empty()) else {
        return upload_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidRequest,
            "no file provided",
            false,
            serde_json::json!({}),
        );
    };
    if !attachments::is_allowed_extension(&filename)
        || !attachments::file_signature_matches(&filename, &file_bytes)
    {
        return upload_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorCode::UnsupportedFormat,
            "unsupported or mismatched file format",
            false,
            serde_json::json!({
                "allowed_extensions": attachments::ALLOWED_EXTENSIONS
            }),
        );
    }

    let upload_id = Uuid::new_v4().to_string();
    let safe_name = attachments::sanitize_filename(&filename);
    if safe_name.is_empty() || safe_name.len() > MAX_FILENAME_BYTES {
        return upload_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidRequest,
            "filename is invalid or too long",
            false,
            serde_json::json!({ "max_filename_bytes": MAX_FILENAME_BYTES }),
        );
    }
    let file_dir = match safe_upload_directory(&state.upload_dir, &upload_id) {
        Ok(directory) => directory,
        Err(_) => {
            return upload_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Storage,
                "upload storage is unavailable",
                true,
                serde_json::json!({}),
            )
        }
    };
    let stored_path = file_dir.join(&safe_name);
    if write_private_file(&stored_path, &file_bytes).is_err() {
        return upload_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Storage,
            "upload could not be stored",
            true,
            serde_json::json!({}),
        );
    }

    if state
        .config
        .client_for(ClientPurpose::Document, None)
        .is_err()
    {
        return upload_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::Configuration,
            "document client configuration is invalid",
            false,
            serde_json::json!({ "setting": "docModel" }),
        );
    }
    let http = state.config.client_factory().http_client();
    let extracted = attachments::process_file(
        &doc_model,
        http.as_ref(),
        &stored_path,
        &safe_name,
        &upload_id,
    )
    .await;
    if extracted.error.is_some() {
        tracing::warn!(upload_id = %upload_id, "document extraction failed (details redacted)");
        return upload_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::UnsupportedFormat,
            "document content could not be extracted",
            false,
            serde_json::json!({ "upload_id": upload_id }),
        );
    }
    let images = extracted
        .images_base64
        .into_iter()
        .map(|image| ImageRef {
            media_type: image.media_type,
            data: image.data,
        })
        .collect::<Vec<_>>();
    let stored = StoredAttachment {
        id: extracted.id.clone(),
        filename: extracted.filename.clone(),
        extracted_text: extracted.extracted_text,
        images,
    };
    let metadata = match serde_json::to_vec(&stored) {
        Ok(metadata) => metadata,
        Err(_) => {
            return upload_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "extracted document could not be stored",
                true,
                serde_json::json!({ "upload_id": upload_id }),
            )
        }
    };
    if write_private_file(&file_dir.join(STORED_ATTACHMENT_FILE), &metadata).is_err() {
        return upload_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Storage,
            "extracted document could not be stored",
            true,
            serde_json::json!({ "upload_id": upload_id }),
        );
    }

    // Preserve the response fields for protocol compatibility, but keep raw
    // attachment content server-side. The Run request references the upload ID.
    json_response(
        StatusCode::OK,
        &UploadResponse {
            id: stored.id,
            filename: stored.filename,
            extracted_text: String::new(),
            image_count: stored.images.len(),
            images: None,
            error: None,
        },
    )
}

pub(super) fn load_stored_attachment(upload_root: &Path, id: &str) -> Option<StoredAttachment> {
    if !valid_upload_id(id) {
        return None;
    }
    let root = upload_root.canonicalize().ok()?;
    let path = root.join(id).join(STORED_ATTACHMENT_FILE);
    let canonical = path.canonicalize().ok()?;
    if !canonical.starts_with(&root) {
        return None;
    }
    let bytes = std::fs::read(canonical).ok()?;
    if bytes.len() > attachments::MAX_FILE_SIZE as usize * 3 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn valid_upload_id(id: &str) -> bool {
    Uuid::parse_str(id).is_ok_and(|parsed| parsed.hyphenated().to_string() == id)
}

fn safe_upload_directory(root: &Path, id: &str) -> std::io::Result<std::path::PathBuf> {
    if !valid_upload_id(id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "invalid upload identifier",
        ));
    }
    std::fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        let canonical_root = root.canonicalize()?;
        let directory = canonical_root.join(id);
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700).create(&directory)?;
        let canonical_directory = directory.canonicalize()?;
        if !canonical_directory.starts_with(&canonical_root) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "upload path denied",
            ));
        }
        Ok(canonical_directory)
    }
    #[cfg(not(unix))]
    {
        let canonical_root = root.canonicalize()?;
        let directory = canonical_root.join(id);
        std::fs::create_dir(&directory)?;
        let canonical_directory = directory.canonicalize()?;
        if !canonical_directory.starts_with(&canonical_root) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "upload path denied",
            ));
        }
        Ok(canonical_directory)
    }
}

fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)
}

fn upload_error(
    status: StatusCode,
    code: ErrorCode,
    message: &'static str,
    retryable: bool,
    details: serde_json::Value,
) -> Response {
    error_response(
        status,
        AppError::new(code, message, retryable, "upload")
            .with_trace_id(Uuid::new_v4().to_string())
            .with_safe_details(details),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_ids_and_storage_paths_are_canonical_and_private() {
        // **Validates: Requirements 8.8, 11.2**
        let temp = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4().to_string();
        let directory = safe_upload_directory(temp.path(), &id).unwrap();
        assert!(directory.starts_with(temp.path().canonicalize().unwrap()));
        assert!(safe_upload_directory(temp.path(), "../escape").is_err());
        write_private_file(&directory.join("fixture.txt"), b"safe").unwrap();
        assert!(write_private_file(&directory.join("fixture.txt"), b"overwrite").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(directory.join("fixture.txt"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn stored_attachment_lookup_rejects_traversal_and_round_trips_locally() {
        let temp = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4().to_string();
        let directory = safe_upload_directory(temp.path(), &id).unwrap();
        let stored = StoredAttachment {
            id: id.clone(),
            filename: "fixture.txt".into(),
            extracted_text: "private attachment body".into(),
            images: vec![],
        };
        write_private_file(
            &directory.join(STORED_ATTACHMENT_FILE),
            &serde_json::to_vec(&stored).unwrap(),
        )
        .unwrap();
        assert_eq!(
            load_stored_attachment(temp.path(), &id)
                .unwrap()
                .extracted_text,
            "private attachment body"
        );
        assert!(load_stored_attachment(temp.path(), "../escape").is_none());
    }
}
