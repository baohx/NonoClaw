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
    use std::io::Cursor;
    use std::time::Instant;

    use axum::routing::post;
    use axum::{Json, Router};
    use image::{DynamicImage, ImageFormat};
    use nonoclaw_engine::load_resolved_config;
    use reqwest::multipart::{Form, Part};

    const EXPLORATION_SEED: u64 = 0xA77A_C4E1_2025_0001;

    fn text_pdf_fixture() -> Vec<u8> {
        let text = "Deterministic attachment upload exploration text with enough non whitespace characters for direct PDF extraction.";
        let stream = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_string(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
            format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", index + 1, object).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn png_fixture() -> Vec<u8> {
        let image = DynamicImage::new_rgb8(2, 2);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    async fn spawn_mock_ocr() -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                Json(serde_json::json!({
                    "choices": [{"message": {"content": "synthetic OCR fixture"}}]
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), task)
    }

    async fn spawn_upload_server(
        state: Arc<AppState>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/api/upload", post(upload_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/api/upload"), task)
    }

    #[tokio::test]
    async fn property_1_unfixed_persisted_upload_settles_as_matching_wire_success() {
        // **Validates: Requirements 1.1, 1.2, 1.3, 1.4, 1.5, 1.8**
        let temp = tempfile::tempdir().unwrap();
        let upload_dir = temp.path().join("uploads");
        let (ocr_base_url, ocr_task) = spawn_mock_ocr().await;
        let settings_path = temp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            serde_json::to_vec(&serde_json::json!({
                "docModel": {
                    "provider": "deepseek_ocr",
                    "model": "fixture-doc-model",
                    "baseUrl": ocr_base_url,
                    "apiKey": "synthetic-test-key"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let config = Arc::new(load_resolved_config(
            temp.path(),
            Some(&settings_path),
            None,
        ));
        let state = super::super::connection::upload_exploration_state(
            temp.path().to_path_buf(),
            config,
            upload_dir.clone(),
        );
        let (upload_url, upload_task) = spawn_upload_server(state).await;
        let client = reqwest::Client::new();
        let fixtures = [
            (
                "markdown",
                "fixture.md",
                "text/markdown",
                "# Synthetic fixture\n\nUTF-8 text: deterministic café."
                    .as_bytes()
                    .to_vec(),
            ),
            ("png", "fixture.png", "image/png", png_fixture()),
            (
                "pdf",
                "fixture.pdf",
                "application/pdf",
                text_pdf_fixture(),
            ),
        ];

        eprintln!("upload_exploration seed={EXPLORATION_SEED:#018x}");
        for (category, filename, media_type, bytes) in fixtures {
            let started = Instant::now();
            let part = Part::bytes(bytes)
                .file_name(filename.to_string())
                .mime_str(media_type)
                .unwrap();
            let response = client
                .post(&upload_url)
                .multipart(Form::new().part("file", part))
                .send()
                .await
                .unwrap_or_else(|_| panic!("category={category} boundary=transport"));
            let status = response.status();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            let body = response
                .bytes()
                .await
                .unwrap_or_else(|_| panic!("category={category} boundary=transport_body"));
            assert!(
                status.is_success(),
                "category={category} boundary=response_build status={}",
                status.as_u16()
            );
            assert!(
                content_type.starts_with("application/json"),
                "category={category} boundary=response_headers status={}",
                status.as_u16()
            );
            let payload: serde_json::Value = serde_json::from_slice(&body)
                .unwrap_or_else(|_| panic!("category={category} boundary=wire_parse"));
            let id = payload["id"]
                .as_str()
                .unwrap_or_else(|| panic!("category={category} boundary=wire_schema"));
            assert!(
                valid_upload_id(id),
                "category={category} boundary=wire_schema field=id"
            );
            assert!(
                payload["filename"].is_string()
                    && payload["extracted_text"].as_str() == Some("")
                    && payload["image_count"].as_u64().is_some()
                    && payload["error"].is_null(),
                "category={category} boundary=wire_schema fields=success"
            );
            let stored = load_stored_attachment(&upload_dir, id)
                .unwrap_or_else(|| panic!("category={category} boundary=metadata_persistence"));
            assert_eq!(
                stored.id, id,
                "category={category} boundary=response_correlation"
            );
            eprintln!(
                "category={category} upload_id={id} phase=loopback_complete status={} body_bytes={} elapsed_ms={}",
                status.as_u16(),
                body.len(),
                started.elapsed().as_millis()
            );
        }
        upload_task.abort();
        ocr_task.abort();
    }

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
