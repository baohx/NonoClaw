//! File attachment processing — routes uploaded documents through a configurable
//! multimodal document-processing model to extract text + images.
//!
//! Supported providers:
//! - `mistral_ocr` → Mistral OCR API (`/v1/ocr`)
//! - `gemini` → Gemini Files API + generateContent (stub)
//! - `generic_vision` → PDF→images via pdftoppm → vision chat API (stub)
//!
//! Supported file types:
//! - .txt / .md  → direct read (no model)
//! - .pdf        → sent to the doc model
//! - .docx/.doc  → libreoffice → PDF → doc model
//! - .png/.jpg   → sent to the doc model as image

use std::path::{Path, PathBuf};
use std::process::Command;

use nonoclaw_engine::settings::DocModelConfig;
use serde::{Deserialize, Serialize};

// ── Public API ──────────────────────────────────────────────────────────────

/// Result of processing a file upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedDoc {
    pub id: String,
    pub filename: String,
    /// Markdown text extracted from the document.
    pub extracted_text: String,
    /// Number of embedded images found in the document.
    pub image_count: usize,
    /// First few extracted images as base64 (for multimodal model context).
    /// Limited to 2 images, max ~500KB each when base64-encoded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images_base64: Vec<ImageB64>,
    /// Human-readable error if extraction partially failed (empty text + error).
    pub error: Option<String>,
}

/// A lightweight base64 image reference for passing to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageB64 {
    pub media_type: String,
    pub data: String,
}

/// Top-level router: detect file type, pre-process if needed, extract content.
pub async fn process_file(
    config: &DocModelConfig,
    file_path: &Path,
    original_name: &str,
    upload_id: &str,
) -> ExtractedDoc {
    let mime = mime_type(file_path, original_name);

    tracing::info!(%original_name, %mime, "processing attachment");

    // TXT / MD: direct read — no model needed.
    if mime == "text/plain" || mime == "text/markdown" || mime == "text/x-markdown" {
        match std::fs::read_to_string(file_path) {
            Ok(text) => {
                return ExtractedDoc {
                    id: upload_id.into(),
                    filename: original_name.into(),
                    extracted_text: text,
                    image_count: 0,
                    images_base64: vec![],
                    error: None,
                };
            }
            Err(e) => {
                return ExtractedDoc {
                    id: upload_id.into(),
                    filename: original_name.into(),
                    extracted_text: String::new(),
                    image_count: 0,
                    images_base64: vec![],
                    error: Some(format!("failed to read file: {e}")),
                };
            }
        }
    }

    // DOCX / DOC: convert to PDF via libreoffice first.
    let pdf_path: Option<PathBuf>;
    let process_target = if mime.contains("officedocument") || mime == "application/msword" {
        match docx_to_pdf(file_path) {
            Ok(p) => {
                pdf_path = Some(p);
                pdf_path.as_ref().unwrap()
            }
            Err(e) => {
                return ExtractedDoc {
                    id: upload_id.into(),
                    filename: original_name.into(),
                    extracted_text: String::new(),
                    image_count: 0,
                    images_base64: vec![],
                    error: Some(e),
                };
            }
        }
    } else {
        pdf_path = None;
        file_path
    };

    // Route to the configured doc model provider.
    let result: Result<(String, usize, Vec<ImageB64>), String> = match config.provider.as_str() {
        "mistral_ocr" => process_mistral(config, process_target, &mime).await,
        "deepseek_ocr" => process_deepseek_ocr(config, process_target, &mime).await,
        _ => {
            let r: Result<(String, usize), String> = match config.provider.as_str() {
                "gemini" => process_gemini_stub(config, process_target, &mime).await,
                "generic_vision" => process_generic_vision_stub(config, process_target, &mime).await,
                other => Err(format!("unknown doc_model.provider: {other}")),
            };
            r.map(|(t, c)| (t, c, vec![]))
        }
    };

    // Clean up the temporary PDF if we created one.
    if let Some(p) = &pdf_path {
        let _ = std::fs::remove_file(p);
    }

    match result {
        Ok((text, image_count, mut images)) => {
            // Fallback: if the input itself is an image but the doc model
            // didn't extract any, include the original so the multimodal
            // conversation model can still "see" it visually.
            if images.is_empty() && mime.starts_with("image/") {
                if let Ok(data) = std::fs::read(process_target) {
                    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
                    if b64.len() < 2_000_000 {
                        images.push(ImageB64 {
                            media_type: mime.to_string(),
                            data: b64,
                        });
                    }
                }
            }
            ExtractedDoc {
                id: upload_id.into(),
                filename: original_name.into(),
                extracted_text: text,
                image_count,
                images_base64: images,
                error: None,
            }
        },
        Err(e) => ExtractedDoc {
            id: upload_id.into(),
            filename: original_name.into(),
            extracted_text: String::new(),
            image_count: 0,
            images_base64: vec![],
            error: Some(e),
        },
    }
}

// ── MIME detection ──────────────────────────────────────────────────────────

fn mime_type(file_path: &Path, original_name: &str) -> String {
    let ext = original_name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "txt" => "text/plain".into(),
        "md" | "markdown" => "text/markdown".into(),
        "pdf" => "application/pdf".into(),
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document".into(),
        "doc" => "application/msword".into(),
        "png" => "image/png".into(),
        "jpg" | "jpeg" => "image/jpeg".into(),
        _ => {
            // Try magic bytes.
            if let Ok(bytes) = std::fs::read(file_path).map(|b| b.into_iter().take(8).collect::<Vec<u8>>()) {
                if bytes.starts_with(b"%PDF") {
                    return "application/pdf".into();
                }
                if bytes.starts_with(b"\x89PNG") {
                    return "image/png".into();
                }
                if bytes.starts_with(b"\xff\xd8") {
                    return "image/jpeg".into();
                }
                if bytes.starts_with(b"PK\x03\x04") {
                    // ZIP-based — could be DOCX. Check for word/document.xml.
                    if let Ok(data) = std::fs::read(file_path) {
                        if data.windows(18).any(|w| w == b"word/document.xml") {
                            return "application/vnd.openxmlformats-officedocument.wordprocessingml.document".into();
                        }
                    }
                    return "application/zip".into();
                }
            }
            "application/octet-stream".into()
        }
    }
}

// ── Pre-processing: DOCX → PDF ─────────────────────────────────────────────

fn docx_to_pdf(input: &Path) -> Result<PathBuf, String> {
    let dir = input.parent().unwrap_or_else(|| Path::new("."));
    let output = dir.join(format!(
        "nonoclaw_convert_{}.pdf",
        uuid::Uuid::new_v4()
    ));

    tracing::info!(input=%input.display(), output=%output.display(), "converting to PDF via libreoffice");

    let status = Command::new("libreoffice")
        .args([
            "--headless",
            "--convert-to",
            "pdf",
            "--outdir",
        ])
        .arg(dir)
        .arg(input)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|e| format!(
            "libreoffice not found — install it to process DOCX files: sudo apt install libreoffice-core (error: {e})"
        ))?;

    if !status.success() {
        return Err("libreoffice conversion failed".into());
    }

    // libreoffice names the output as <stem>.pdf in the outdir.
    let stem = input.file_stem().unwrap_or_default();
    let expected = dir.join(format!("{}.pdf", stem.to_string_lossy()));
    if expected.exists() {
        // Rename to our UUID name.
        std::fs::rename(&expected, &output)
            .map_err(|e| format!("failed to rename PDF: {e}"))?;
    } else if output.exists() {
        // Already at the target name.
    } else {
        return Err("libreoffice output PDF not found".into());
    }

    Ok(output)
}

// ── Mistral OCR provider ────────────────────────────────────────────────────

async fn process_mistral(
    config: &DocModelConfig,
    file_path: &Path,
    mime: &str,
) -> Result<(String, usize, Vec<ImageB64>), String> {
    let api_key = config.resolved_api_key();
    let bytes = std::fs::read(file_path)
        .map_err(|e| format!("failed to read file for OCR: {e}"))?;
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);

    let is_image = mime.starts_with("image/");
    let data_uri = if is_image {
        format!("data:{mime};base64,{b64}")
    } else {
        format!("data:application/pdf;base64,{b64}")
    };

    let body = if is_image {
        serde_json::json!({
            "model": config.model,
            "document": {
                "type": "image_url",
                "image_url": data_uri
            },
            "include_image_base64": true
        })
    } else {
        serde_json::json!({
            "model": config.model,
            "document": {
                "type": "document_url",
                "document_url": data_uri
            },
            "include_image_base64": true
        })
    };

    let client = reqwest::Client::new();
    let url = format!("{}/v1/ocr", config.base_url.trim_end_matches('/'));
    tracing::debug!(%url, "calling Mistral OCR");

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Mistral OCR request failed: {e}"))?;

    let status = resp.status();
    let resp_text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        tracing::warn!(%status, %resp_text, "Mistral OCR error");
        return Err(format!("Mistral OCR returned {}: {}", status.as_u16(), resp_text));
    }

    tracing::info!(len = resp_text.len(), "Mistral OCR response received");

    let response: MistralOcrResponse = serde_json::from_str(&resp_text)
        .map_err(|e| format!("failed to parse Mistral OCR response: {} (body head: {})", e, &resp_text[..resp_text.len().min(300)]))?;

    // Concatenate markdown from all pages.
    let mut text = String::new();
    let mut image_count = 0usize;
    let mut images_base64: Vec<ImageB64> = Vec::new();
    for page in &response.pages {
        text.push_str(&page.markdown);
        text.push_str("\n\n");
        image_count += page.images.len();
        // Collect up to 2 images (cap per-image data at ~500KB base64).
        for img in &page.images {
            if images_base64.len() >= 2 {
                break;
            }
            if let Some(b64) = img.get("image_base64").and_then(|v| v.as_str()) {
                if b64.len() < 700_000 {
                    let media = img
                        .get("media_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("image/png");
                    images_base64.push(ImageB64 {
                        media_type: media.to_string(),
                        data: b64.to_string(),
                    });
                }
            }
        }
    }

    let result = text.trim().to_string();
    tracing::info!(
        pages = response.pages.len(),
        chars = result.len(),
        images = image_count,
        extracted_to_context = images_base64.len(),
        "Mistral OCR extraction complete"
    );

    // We return (text, image_count) but ExtractedDoc has images_base64.
    // Let me restructure the return.
    Ok((result, image_count, images_base64))
}

#[derive(Debug, Deserialize)]
struct MistralOcrResponse {
    pages: Vec<MistralOcrPage>,
}

#[derive(Debug, Deserialize)]
struct MistralOcrPage {
    #[allow(dead_code)]
    index: usize,
    markdown: String,
    #[serde(default)]
    images: Vec<serde_json::Value>,
}

// ── DeepSeek OCR provider (OpenAI-compatible) ───────────────────────────────

async fn process_deepseek_ocr(
    config: &DocModelConfig,
    file_path: &Path,
    mime: &str,
) -> Result<(String, usize, Vec<ImageB64>), String> {
    let images: Vec<(String, Vec<u8>)> = if mime == "application/pdf" {
        pdf_to_images(file_path)?
    } else {
        let bytes = std::fs::read(file_path)
            .map_err(|e| format!("failed to read image: {e}"))?;
        vec![(mime.to_string(), bytes)]
    };

    let api_key = config.resolved_api_key();
    let url = format!(
        "{}/v1/chat/completions",
        config.base_url.trim_end_matches('/')
    );
    let client = reqwest::Client::new();
    let mut full_text = String::new();

    // DeepSeek OCR 2 uses the `<image>` token + specialised prompt format.
    const OCR_PROMPT: &str = "<image>\n<|grounding|>Convert the document to markdown.";

    for (i, (_img_mime, img_bytes)) in images.iter().enumerate() {
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, img_bytes);

        let body = serde_json::json!({
            "model": config.model,
            "messages": [{
                "role": "user",
                "content": [
                    {
                        "type": "image_url",
                        "image_url": { "url": format!("data:image/png;base64,{b64}") }
                    },
                    {
                        "type": "text",
                        "text": OCR_PROMPT
                    }
                ]
            }],
            "max_tokens": 8192,
            "temperature": 0.0
        });

        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("DeepSeek OCR request failed for page {}: {e}", i + 1))?;

        let status = resp.status();
        let resp_text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            tracing::warn!(%status, %resp_text, "DeepSeek OCR error");
            return Err(format!(
                "DeepSeek OCR returned {} for page {}: {}",
                status.as_u16(), i + 1, resp_text
            ));
        }

        let response: serde_json::Value = serde_json::from_str(&resp_text)
            .map_err(|e| format!("failed to parse DeepSeek OCR response: {e}"))?;

        if let Some(t) = response["choices"][0]["message"]["content"].as_str() {
            if images.len() > 1 {
                full_text.push_str(&format!("## Page {}\n\n", i + 1));
            }
            full_text.push_str(t);
            full_text.push_str("\n\n");
        }
    }

    tracing::info!(
        pages = images.len(),
        chars = full_text.len(),
        "DeepSeek OCR extraction complete"
    );

    Ok((full_text.trim().to_string(), 0, vec![]))
}

// ── Stub providers ──────────────────────────────────────────────────────────

async fn process_gemini_stub(
    _config: &DocModelConfig,
    _file_path: &Path,
    _mime: &str,
) -> Result<(String, usize), String> {
    Err("gemini provider is not yet implemented; use mistral_ocr or generic_vision".into())
}

async fn process_generic_vision_stub(
    config: &DocModelConfig,
    file_path: &Path,
    mime: &str,
) -> Result<(String, usize), String> {
    // For images: send directly to the vision model.
    // For PDFs: convert each page to PNG via pdftoppm, then send each page.
    let images: Vec<(String, Vec<u8>)> = if mime == "application/pdf" {
        pdf_to_images(file_path)?
    } else {
        let bytes = std::fs::read(file_path)
            .map_err(|e| format!("failed to read image: {e}"))?;
        vec![(mime.to_string(), bytes)]
    };

    let api_key = config.resolved_api_key();
    let url = format!(
        "{}/v1/chat/completions",
        config.base_url.trim_end_matches('/')
    );
    let client = reqwest::Client::new();
    let mut full_text = String::new();

    for (i, (_img_mime, img_bytes)) in images.iter().enumerate() {
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, img_bytes);

        let body = serde_json::json!({
            "model": config.model,
            "messages": [{
                "role": "user",
                "content": [
                    {
                        "type": "image_url",
                        "image_url": { "url": format!("data:image/png;base64,{b64}") }
                    },
                    {
                        "type": "text",
                        "text": "Transcribe the full text content of this document page. Preserve structure, tables, headings, and describe any images or charts you see. Output in markdown format."
                    }
                ]
            }],
            "max_tokens": 4096
        });

        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("vision model request failed for page {}: {e}", i + 1))?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "vision model returned {} for page {}: {}",
                status.as_u16(),
                i + 1,
                err_body
            ));
        }

        let response: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("failed to parse vision response: {e}"))?;

        if let Some(t) = response["choices"][0]["message"]["content"].as_str() {
            if images.len() > 1 {
                full_text.push_str(&format!("## Page {}\n\n", i + 1));
            }
            full_text.push_str(t);
            full_text.push_str("\n\n");
        }
    }

    Ok((full_text.trim().to_string(), 0))
}

/// Convert PDF pages to PNG images using `pdftoppm` (from poppler-utils).
fn pdf_to_images(file_path: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let dir = file_path.parent().unwrap_or_else(|| Path::new("."));
    let prefix = dir.join(format!(
        "nonoclaw_page_{}",
        uuid::Uuid::new_v4()
    ));

    tracing::info!(input=%file_path.display(), "converting PDF to images via pdftoppm");

    let output = Command::new("pdftoppm")
        .args(["-png", "-r", "200"])
        .arg(file_path)
        .arg(&prefix)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!(
            "pdftoppm not found — install it: sudo apt install poppler-utils (error: {e})"
        ))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("pdftoppm failed: {stderr}"));
    }

    // Read back all generated PNG files matching the prefix.
    let mut images: Vec<(String, Vec<u8>)> = Vec::new();
    let prefix_str = prefix
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with(&prefix_str))
                    .unwrap_or(false)
            })
            .collect();
        paths.sort();
        for p in &paths {
            match std::fs::read(p) {
                Ok(data) => images.push(("image/png".into(), data)),
                Err(e) => tracing::warn!(path=%p.display(), "failed to read page image: {e}"),
            }
            let _ = std::fs::remove_file(p);
        }
    }

    if images.is_empty() {
        return Err("pdftoppm produced no output images".into());
    }

    Ok(images)
}

// ── Validation helpers ──────────────────────────────────────────────────────

/// Allowed file extensions.
pub const ALLOWED_EXTENSIONS: &[&str] = &[
    "pdf", "docx", "doc", "txt", "md", "markdown", "png", "jpg", "jpeg",
];

/// Max file size in bytes (32 MB).
pub const MAX_FILE_SIZE: u64 = 32 * 1024 * 1024;

/// Check extension is in the allowlist.
pub fn is_allowed_extension(filename: &str) -> bool {
    if let Some(ext) = filename.rsplit('.').next() {
        ALLOWED_EXTENSIONS.contains(&ext.to_lowercase().as_str())
    } else {
        false
    }
}

/// Sanitize a filename: strip path separators and `..`.
pub fn sanitize_filename(name: &str) -> String {
    name.replace(['/', '\\', '\0'], "_")
        .replace("..", "__")
        .trim_start_matches('.')
        .to_string()
}

/// Maximum number of chars of extracted text sent inline in the Run message.
/// If the text exceeds this, it is truncated and the server path is noted so the
/// model can use the Read tool to access the full content.
pub const MAX_INLINE_TEXT_CHARS: usize = 50_000;
