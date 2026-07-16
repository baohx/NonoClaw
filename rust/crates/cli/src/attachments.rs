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

    // ── PDF: try direct text extraction first ──────────────────────────
    if mime == "application/pdf" {
        match extract_pdf_text(file_path) {
            Ok(text) if text.chars().filter(|c| !c.is_whitespace()).count() > 50 => {
                let images = extract_pdf_images(file_path);
                let image_count = images.len();
                // OCR the embedded images so text-only models can see them too.
                let enriched = ocr_embedded_images(config, &images, &text).await;
                tracing::info!(chars = enriched.len(), images = image_count, "PDF text + images extracted");
                return ExtractedDoc {
                    id: upload_id.into(),
                    filename: original_name.into(),
                    extracted_text: enriched,
                    image_count,
                    images_base64: images,
                    error: None,
                };
            }
            Ok(_) => {
                tracing::info!("PDF has sparse text — falling back to OCR");
            }
            Err(e) => {
                tracing::warn!("pdftotext failed: {e} — falling back to OCR");
            }
        }
        // Fall through to OCR path below.
    }

    // ── DOCX: try direct text extraction first ─────────────────────────
    if mime.contains("officedocument") || mime == "application/msword" {
        match extract_docx_text(file_path) {
            Ok(text) if text.chars().filter(|c| !c.is_whitespace()).count() > 50 => {
                let images = extract_docx_images(file_path);
                let image_count = images.len();
                let enriched = ocr_embedded_images(config, &images, &text).await;
                tracing::info!(chars = enriched.len(), images = image_count, "DOCX text + images extracted");
                return ExtractedDoc {
                    id: upload_id.into(),
                    filename: original_name.into(),
                    extracted_text: enriched,
                    image_count,
                    images_base64: images,
                    error: None,
                };
            }
            Ok(_) => {
                tracing::info!("DOCX has sparse text — falling back to conversion + OCR");
            }
            Err(e) => {
                tracing::warn!("DOCX text extraction failed: {e} — falling back to conversion + OCR");
            }
        }
        // Fall through to libreoffice conversion + OCR below.
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

// ── Direct text extraction (no OCR needed) ─────────────────────────────────

/// Extract text from a PDF using `pdftotext` (poppler-utils).
fn extract_pdf_text(path: &Path) -> Result<String, String> {
    let output = std::process::Command::new("pdftotext")
        .arg("-layout")
        .arg(path)
        .arg("-") // stdout
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("pdftotext: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("pdftotext failed: {stderr}"));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("invalid UTF-8: {e}"))
}

/// Extract text from a DOCX file (ZIP of XML).  Walks `word/document.xml`
/// and collects text from `<w:t>` elements.
fn extract_docx_text(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("not a valid ZIP/DOCX: {e}"))?;
    let mut doc = archive
        .by_name("word/document.xml")
        .map_err(|e| format!("word/document.xml not found: {e}"))?;
    let mut doc_str = String::new();
    std::io::Read::read_to_string(&mut doc, &mut doc_str)
        .map_err(|e| format!("failed to read document.xml: {e}"))?;
    Ok(extract_wt_text(&doc_str))
}

/// Extract text from `<w:t>` elements in a DOCX document.xml string.
/// Simple regex-free scan: finds `<w:t` ... `>` and collects content until `</w:t>`.
fn extract_wt_text(xml: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut in_wt = false;
    let mut tag_buf = String::new();
    let mut text_buf = String::new();

    for ch in xml.chars() {
        match ch {
            '<' => {
                if in_wt {
                    // Flush buffered text.
                    if !text_buf.is_empty() {
                        if !out.is_empty() { out.push(' '); }
                        out.push_str(&text_buf);
                        text_buf.clear();
                    }
                }
                in_tag = true;
                tag_buf.clear();
            }
            '>' => {
                if in_tag {
                    let t = tag_buf.trim();
                    if t.starts_with("w:t") || t.starts_with("/w:t") {
                        in_wt = !t.starts_with("/");
                        text_buf.clear();
                    }
                    in_tag = false;
                    tag_buf.clear();
                } else if in_wt {
                    text_buf.push(ch);
                }
            }
            _ => {
                if in_tag {
                    tag_buf.push(ch);
                } else if in_wt {
                    text_buf.push(ch);
                }
            }
        }
    }
    // Flush remaining.
    if in_wt && !text_buf.is_empty() {
        if !out.is_empty() { out.push(' '); }
        out.push_str(&text_buf);
    }
    out
}

/// OCR each embedded image and append descriptions to the document text.
/// For Mistral OCR, uses `/v1/ocr`; for OpenAI-compatible providers
/// (deepseek_ocr, generic_vision), uses `/v1/chat/completions`.
async fn ocr_embedded_images(
    config: &DocModelConfig,
    images: &[ImageB64],
    document_text: &str,
) -> String {
    if images.is_empty() {
        return document_text.to_string();
    }
    let mut out = document_text.to_string();
    let api_key = config.resolved_api_key();
    let client = reqwest::Client::new();

    let is_mistral = config.provider == "mistral_ocr";

    for (i, img) in images.iter().enumerate() {
        let result: Result<String, _> = if is_mistral {
            // Mistral OCR: POST /v1/ocr with image_url
            let url = format!("{}/v1/ocr", config.base_url.trim_end_matches('/'));
            let body = serde_json::json!({
                "model": config.model,
                "document": {
                    "type": "image_url",
                    "image_url": format!("data:{};base64,{}", img.media_type, img.data)
                }
            });
            match client
                .post(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&body)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        let text: String = json["pages"]
                            .as_array()
                            .map(|pages| {
                                pages
                                    .iter()
                                    .filter_map(|p| p["markdown"].as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default();
                        Ok(text)
                    } else {
                        Err("parse error".to_string())
                    }
                }
                Ok(resp) => Err(format!("HTTP {}", resp.status())),
                Err(e) => Err(format!("{e}")),
            }
        } else {
            // OpenAI-compatible: POST /v1/chat/completions
            let url = format!(
                "{}/v1/chat/completions",
                config.base_url.trim_end_matches('/')
            );
            let body = serde_json::json!({
                "model": config.model,
                "messages": [{
                    "role": "user",
                    "content": [
                        {
                            "type": "image_url",
                            "image_url": { "url": format!("data:{};base64,{}", img.media_type, img.data) }
                        },
                        {
                            "type": "text",
                            "text": "<image>\nDescribe this image briefly in Chinese. What is it? A photo, chart, signature, stamp, diagram? Include any visible text."
                        }
                    ]
                }],
                "max_tokens": 300,
                "temperature": 0.0
            });
            match client
                .post(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&body)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        Ok(json["choices"][0]["message"]["content"]
                            .as_str()
                            .unwrap_or("")
                            .to_string())
                    } else {
                        Err("parse error".to_string())
                    }
                }
                Ok(resp) => Err(format!("HTTP {}", resp.status())),
                Err(e) => Err(format!("{e}")),
            }
        };

        match result {
            Ok(desc) if !desc.is_empty() => {
                out.push_str(&format!("\n\n[Embedded Image {}]: {desc}", i + 1));
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("embedded image OCR failed for image {}: {e}", i + 1);
            }
        }
    }
    out
}

/// Extract embedded images from a PDF using `pdfimages` (poppler-utils).
/// Returns base64-encoded JPEGs, capped at 8 images, each <1 MB.
fn extract_pdf_images(path: &Path) -> Vec<ImageB64> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let prefix = dir.join(format!("nonoclaw_pdfimg_{}", uuid::Uuid::new_v4()));
    let result = std::process::Command::new("pdfimages")
        .arg("-j") // JPEG output
        .arg(path)
        .arg(&prefix)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let mut images = Vec::new();
    if result.is_err() {
        return images;
    }
    let prefix_str = prefix.file_name().unwrap_or_default().to_string_lossy().to_string();
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
        for p in paths.iter().take(8) {
            if let Ok(data) = std::fs::read(p) {
                if data.len() < 1_000_000 {
                    let b64 = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &data,
                    );
                    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("jpg");
                    images.push(ImageB64 {
                        media_type: format!("image/{ext}"),
                        data: b64,
                    });
                }
            }
            let _ = std::fs::remove_file(p);
        }
    }
    images
}

/// Extract embedded images from a DOCX file (`word/media/` in the ZIP).
fn extract_docx_images(path: &Path) -> Vec<ImageB64> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(_) => return vec![],
    };
    let mut images = Vec::new();
    // Collect all media file names first (can't borrow archive mutably
    // and immutably at the same time in zip v2).
    let media_names: Vec<String> = archive
        .file_names()
        .filter(|n| n.starts_with("word/media/"))
        .map(|n| n.to_string())
        .collect();
    for name in media_names.iter().take(8) {
        if let Ok(mut f) = archive.by_name(name) {
            let mut buf = Vec::new();
            if std::io::Read::read_to_end(&mut f, &mut buf).is_ok() && buf.len() < 1_000_000 {
                let b64 = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &buf,
                );
                let ext = name.rsplit('.').next().unwrap_or("png");
                images.push(ImageB64 {
                    media_type: format!("image/{ext}"),
                    data: b64,
                });
            }
        }
    }
    images
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
    const TILE_SIZE: u32 = 768;
    const GLOBAL_SIZE: u32 = 1024;

    for (i, (_img_mime, img_bytes)) in images.iter().enumerate() {
        let img = match image::load_from_memory(img_bytes) {
            Ok(i) => i,
            Err(_) => {
                return Err("failed to decode image".into());
            }
        };
        let (iw, ih) = (img.width(), img.height());

        // Strategy: send one global downscaled view + tiles for detail.
        // This mirrors the model's native crop_mode behaviour.
        let global =
            img.resize_exact(GLOBAL_SIZE, GLOBAL_SIZE, image::imageops::FilterType::Lanczos3);
        let tiles = tile_image(&img, TILE_SIZE);

        // 1. Global overview first.
        let gb64 = encode_jpeg_base64(&global);
        let page_text = call_ocr_page(&client, &url, &api_key, config, &gb64, "global").await?;
        full_text.push_str(&page_text);
        full_text.push('\n');

        // 2. Then each tile.
        for (ti, tile) in tiles.iter().enumerate() {
            let tb64 = encode_jpeg_base64(tile);
            let tile_text =
                call_ocr_page(&client, &url, &api_key, config, &tb64, &format!("tile {ti}")).await?;
            full_text.push_str(&tile_text);
            full_text.push('\n');
        }

        if images.len() > 1 {
            full_text.push_str(&format!("\n## Page {} end\n\n", i + 1));
        }

        tracing::info!(
            page = i,
            dims = format!("{iw}x{ih}"),
            tiles = tiles.len(),
            chars = full_text.len(),
            "DeepSeek OCR tiled extraction"
        );
    }

    tracing::info!(
        pages = images.len(),
        chars = full_text.len(),
        "DeepSeek OCR extraction complete"
    );

    Ok((full_text.trim().to_string(), 0, vec![]))
}

/// Resize for OCR API: scale to `max_dim`, output as JPEG (much smaller than
/// PNG for photos).  Passes through unchanged if already small enough or if
/// decoding fails.
fn resize_for_ocr(bytes: &[u8], max_dim: u32) -> Vec<u8> {
    let img = match image::load_from_memory(bytes) {
        Ok(i) => i,
        Err(_) => return bytes.to_vec(),
    };
    let (w, h) = (img.width(), img.height());
    // If already small, re-encode as JPEG anyway (may still shrink).
    let ratio = if w <= max_dim && h <= max_dim {
        1.0
    } else {
        max_dim as f64 / w.max(h) as f64
    };
    let nw = (w as f64 * ratio) as u32;
    let nh = (h as f64 * ratio) as u32;
    let resized = img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3);
    let mut out = std::io::Cursor::new(Vec::new());
    // JPEG quality 80: good OCR readability at small file size.
    match resized.write_to(&mut out, image::ImageFormat::Jpeg) {
        Ok(()) => out.into_inner(),
        Err(_) => bytes.to_vec(),
    }
}

/// Split an image into `tile_size × tile_size` tiles, with overlap.
fn tile_image(img: &image::DynamicImage, tile_size: u32) -> Vec<image::DynamicImage> {
    let (w, h) = (img.width(), img.height());
    // If the image is smaller than tile_size, no tiling needed — just return it.
    if w <= tile_size && h <= tile_size {
        return vec![img.clone()];
    }
    let step = tile_size / 2; // 50 % overlap
    let mut tiles = Vec::new();
    let mut y = 0u32;
    while y < h {
        let mut x = 0u32;
        while x < w {
            let tw = tile_size.min(w - x);
            let th = tile_size.min(h - y);
            let tile = img.crop_imm(x, y, tw, th);
            tiles.push(tile);
            x += step;
        }
        y += step;
    }
    // Cap at 12 tiles (matches model's default MAX_CROPS=6 for 2-axis).
    if tiles.len() > 12 {
        tiles.truncate(12);
    }
    tiles
}

/// Encode an image as JPEG base64, for the OCR API.
fn encode_jpeg_base64(img: &image::DynamicImage) -> String {
    let mut buf = std::io::Cursor::new(Vec::new());
    // Best-effort: ignore encoding errors and fall back to raw bytes.
    let data = if img
        .write_to(&mut buf, image::ImageFormat::Jpeg)
        .is_ok()
    {
        buf.into_inner()
    } else {
        return String::new();
    };
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data)
}

/// Call the OCR API for one image (global or tile), return the markdown text.
async fn call_ocr_page(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    config: &DocModelConfig,
    b64: &str,
    label: &str,
) -> Result<String, String> {
    if b64.is_empty() {
        return Ok(String::new());
    }
    let body = serde_json::json!({
        "model": config.model,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image_url",
                    "image_url": { "url": format!("data:image/jpeg;base64,{b64}") }
                },
                {
                    "type": "text",
                    "text": "<image>\n<|grounding|>Convert the document to markdown."
                }
            ]
        }],
        "max_tokens": 4096,
        "temperature": 0.0
    });

    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("OCR request failed ({label}): {e}"))?;

    let status = resp.status();
    let resp_text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "OCR returned {} ({label}): {}",
            status.as_u16(),
            resp_text
        ));
    }

    let response: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| format!("OCR parse error ({label}): {e}"))?;

    Ok(response["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string())
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
