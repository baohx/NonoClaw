//! ElevenLabs speech-to-text HTTP service.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Multipart, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use futures::StreamExt;
use nonoclaw_core::{AppError, ErrorCode};
use uuid::Uuid;

use super::connection::AppState;
use super::http_error::{error_response, json_response};

const MAX_AUDIO_BYTES: usize = 16 * 1024 * 1024;
const MAX_STT_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_TRANSCRIPT_CHARS: usize = 50_000;
const STT_TIMEOUT: Duration = Duration::from_secs(60);
const ALLOWED_AUDIO_TYPES: &[&str] = &[
    "audio/webm",
    "audio/wav",
    "audio/x-wav",
    "audio/mpeg",
    "audio/mp4",
    "audio/ogg",
];

pub(super) async fn stt_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    mut multipart: Multipart,
) -> Response {
    if !state.authorized(params.get("token").map(String::as_str)) {
        return stt_error(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Authentication,
            "invalid or missing auth token",
            false,
            serde_json::json!({}),
        );
    }
    let key = match state.config.elevenlabs_api_key() {
        Some(key) if !key.is_empty() => key,
        _ => {
            return stt_error(
                StatusCode::NOT_IMPLEMENTED,
                ErrorCode::Configuration,
                "speech-to-text is not configured",
                false,
                serde_json::json!({ "setting": "elevenlabsApiKey" }),
            )
        }
    };

    let mut audio: Option<(String, Vec<u8>)> = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => {
                return stt_error(
                    StatusCode::BAD_REQUEST,
                    ErrorCode::InvalidRequest,
                    "invalid multipart audio upload",
                    false,
                    serde_json::json!({}),
                )
            }
        };
        if field.name() != Some("audio") {
            continue;
        }
        if audio.is_some() {
            return stt_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidRequest,
                "exactly one audio file is allowed",
                false,
                serde_json::json!({}),
            );
        }
        let media_type = field
            .content_type()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "application/octet-stream".into());
        if !ALLOWED_AUDIO_TYPES.contains(&media_type.as_str()) {
            return stt_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ErrorCode::UnsupportedFormat,
                "unsupported audio format",
                false,
                serde_json::json!({ "allowed_media_types": ALLOWED_AUDIO_TYPES }),
            );
        }
        let mut bytes = Vec::new();
        let mut field = field;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len().saturating_add(chunk.len()) > MAX_AUDIO_BYTES {
                        return stt_error(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            ErrorCode::PayloadTooLarge,
                            "audio exceeds the upload limit",
                            false,
                            serde_json::json!({ "max_bytes": MAX_AUDIO_BYTES }),
                        );
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => {
                    return stt_error(
                        StatusCode::BAD_REQUEST,
                        ErrorCode::InvalidRequest,
                        "audio upload was interrupted",
                        true,
                        serde_json::json!({}),
                    )
                }
            }
        }
        audio = Some((media_type, bytes));
    }
    let Some((media_type, audio_bytes)) = audio.filter(|(_, bytes)| !bytes.is_empty()) else {
        return stt_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidRequest,
            "no audio provided",
            false,
            serde_json::json!({}),
        );
    };
    if !audio_signature_matches(&media_type, &audio_bytes) {
        return stt_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorCode::UnsupportedFormat,
            "audio content does not match its declared format",
            false,
            serde_json::json!({}),
        );
    }

    let audio_len = audio_bytes.len();
    let part = match reqwest::multipart::Part::bytes(audio_bytes)
        .file_name("recording")
        .mime_str(&media_type)
    {
        Ok(part) => part,
        Err(_) => {
            return stt_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ErrorCode::UnsupportedFormat,
                "unsupported audio format",
                false,
                serde_json::json!({}),
            )
        }
    };
    let form = reqwest::multipart::Form::new()
        .text("model_id", "scribe_v2")
        .part("file", part);
    tracing::info!(audio_len, "sending bounded speech-to-text request");
    let client = state.config.client_factory().http_client();
    let response = tokio::time::timeout(
        STT_TIMEOUT,
        client
            .post("https://api.elevenlabs.io/v1/speech-to-text")
            .header("xi-api-key", key)
            .multipart(form)
            .send(),
    )
    .await;
    match response {
        Ok(Ok(response)) => {
            let status = response.status();
            if !status.is_success() {
                tracing::warn!(%status, "speech-to-text upstream rejected request (body redacted)");
                return stt_error(
                    StatusCode::BAD_GATEWAY,
                    ErrorCode::ProviderUnavailable,
                    "speech-to-text service rejected the request",
                    status.is_server_error() || status.as_u16() == 429,
                    serde_json::json!({ "upstream_status": status.as_u16() }),
                );
            }
            let bytes = match read_bounded_response(response).await {
                Some(bytes) => bytes,
                None => {
                    return stt_error(
                        StatusCode::BAD_GATEWAY,
                        ErrorCode::ProviderUnavailable,
                        "speech-to-text service returned an invalid response",
                        true,
                        serde_json::json!({}),
                    )
                }
            };
            match parse_stt_response(&bytes) {
                Some(text) => json_response(StatusCode::OK, &serde_json::json!({ "text": text })),
                None => stt_error(
                    StatusCode::BAD_GATEWAY,
                    ErrorCode::ProviderUnavailable,
                    "speech-to-text service returned an invalid response",
                    true,
                    serde_json::json!({}),
                ),
            }
        }
        Ok(Err(_)) => stt_error(
            StatusCode::BAD_GATEWAY,
            ErrorCode::ProviderUnavailable,
            "speech-to-text service is unavailable",
            true,
            serde_json::json!({}),
        ),
        Err(_) => stt_error(
            StatusCode::GATEWAY_TIMEOUT,
            ErrorCode::ProviderUnavailable,
            "speech-to-text request timed out",
            true,
            serde_json::json!({}),
        ),
    }
}

fn audio_signature_matches(media_type: &str, bytes: &[u8]) -> bool {
    match media_type {
        "audio/webm" => bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]),
        "audio/wav" | "audio/x-wav" => {
            bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE"
        }
        "audio/mpeg" => {
            bytes.starts_with(b"ID3")
                || bytes
                    .get(..2)
                    .is_some_and(|prefix| prefix[0] == 0xff && prefix[1] & 0xe0 == 0xe0)
        }
        "audio/mp4" => bytes.len() >= 12 && &bytes[4..8] == b"ftyp",
        "audio/ogg" => bytes.starts_with(b"OggS"),
        _ => false,
    }
}

async fn read_bounded_response(response: reqwest::Response) -> Option<Vec<u8>> {
    let mut stream = response.bytes_stream();
    let mut output = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if output.len().saturating_add(chunk.len()) > MAX_STT_RESPONSE_BYTES {
            return None;
        }
        output.extend_from_slice(&chunk);
    }
    Some(output)
}

fn parse_stt_response(bytes: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let text = value.get("text")?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    let mut bounded = text.chars().take(MAX_TRANSCRIPT_CHARS).collect::<String>();
    if text.chars().count() > MAX_TRANSCRIPT_CHARS {
        bounded.push_str("…[truncated]");
    }
    Some(bounded)
}

fn stt_error(
    status: StatusCode,
    code: ErrorCode,
    message: &'static str,
    retryable: bool,
    details: serde_json::Value,
) -> Response {
    error_response(
        status,
        AppError::new(code, message, retryable, "speech_to_text")
            .with_trace_id(Uuid::new_v4().to_string())
            .with_safe_details(details),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stt_parser_returns_only_bounded_transcript_text() {
        // **Validates: Requirements 8.8, 9.8, 11.2**
        let body = br#"{"text":"hello","api_key":"secret","request_id":"private"}"#;
        assert_eq!(parse_stt_response(body).as_deref(), Some("hello"));
        assert!(parse_stt_response(br#"{"error":"Bearer secret"}"#).is_none());
        assert!(parse_stt_response(b"not-json").is_none());
    }

    #[test]
    fn stt_parser_truncates_pathological_transcripts() {
        let body = serde_json::to_vec(&serde_json::json!({
            "text": "x".repeat(MAX_TRANSCRIPT_CHARS + 100)
        }))
        .unwrap();
        let text = parse_stt_response(&body).unwrap();
        assert!(text.ends_with("…[truncated]"));
        assert_eq!(text.chars().count(), MAX_TRANSCRIPT_CHARS + 12);
    }

    #[test]
    fn audio_signatures_must_match_the_declared_allowlisted_type() {
        assert!(audio_signature_matches(
            "audio/webm",
            &[0x1a, 0x45, 0xdf, 0xa3, 0x00]
        ));
        assert!(audio_signature_matches(
            "audio/wav",
            b"RIFF\x00\x00\x00\x00WAVEdata"
        ));
        assert!(audio_signature_matches("audio/mpeg", b"ID3fixture"));
        assert!(audio_signature_matches("audio/ogg", b"OggSfixture"));
        assert!(audio_signature_matches(
            "audio/mp4",
            b"\x00\x00\x00\x18ftypisom"
        ));
        assert!(!audio_signature_matches("audio/webm", b"RIFFspoof"));
        assert!(!audio_signature_matches(
            "application/octet-stream",
            b"OggS"
        ));
    }
}
