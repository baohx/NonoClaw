//! Agent run support: request enrichment, interactive resolvers, and options.
//!
//! Connection lifecycle remains in `connection`; all run construction helpers
//! live here so model runs, forked skills, cancellation, and compaction share
//! the same canonical configuration path.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use nonoclaw_core::{
    redact_text, redact_value, ContentBlock, ImageSource, MessageContent, PermissionDecision,
};
use nonoclaw_engine::{
    ConfigSource, EngineOptions, PermissionRequest, ResolvedConfig, RunConfigOverrides,
    SkillsManager,
};
use nonoclaw_tools::tool::{QuestionRequest, QuestionResolver};
use tokio::sync::{oneshot, Mutex};
use uuid::Uuid;

use super::protocol::{send_msg, AttachmentRef, ServerMsg, Tx};
use crate::attachments;

pub(super) type PermissionMap = Mutex<HashMap<String, oneshot::Sender<PermissionDecision>>>;
pub(super) type QuestionMap = Mutex<HashMap<String, oneshot::Sender<Option<String>>>>;

const MAX_ATTACHMENTS_PER_RUN: usize = 8;
const MAX_IMAGES_PER_ATTACHMENT: usize = 8;

pub(super) struct WsQuestionResolver {
    pub request_id: String,
    pub pending: Arc<QuestionMap>,
    pub tx: Tx,
}

impl QuestionResolver for WsQuestionResolver {
    fn ask(
        &self,
        req: QuestionRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + '_>> {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let request_id = self.request_id.clone();
        Box::pin(async move {
            let (sender, receiver) = oneshot::channel();
            pending.lock().await.insert(request_id.clone(), sender);
            send_msg(
                &tx,
                ServerMsg::QuestionRequired {
                    request_id,
                    prompt: redact_text(&req.prompt),
                    options: req
                        .options
                        .into_iter()
                        .map(|option| redact_text(&option))
                        .collect(),
                },
            )
            .await;
            receiver.await.unwrap_or_default()
        })
    }
}

pub(super) fn enrich_prompt_with_attachments(
    prompt: &str,
    attachments: &Option<Vec<AttachmentRef>>,
    upload_dir: &std::path::Path,
) -> MessageContent {
    let attachments = match attachments {
        Some(attachments) if !attachments.is_empty() => attachments,
        _ => return MessageContent::from_text(prompt),
    };

    let mut blocks = vec![ContentBlock::text(
        "The user has attached the following files. Their content has already been extracted and is shown below — you do NOT need to read or process these files. Just use the content directly.\n\n",
    )];
    for attachment in attachments.iter().take(MAX_ATTACHMENTS_PER_RUN) {
        // New clients send only an opaque upload ID. The legacy inline fields
        // remain accepted as a compatibility fallback, but are bounded and are
        // never logged or reflected back through ProjectInfo/trace.
        let stored = super::upload_service::load_stored_attachment(upload_dir, &attachment.id);
        let filename = stored
            .as_ref()
            .map(|value| value.filename.clone())
            .unwrap_or_else(|| attachments::sanitize_filename(&attachment.filename));
        let filename = if filename.is_empty() {
            "attachment".to_string()
        } else {
            filename.chars().take(255).collect()
        };
        let images = stored
            .as_ref()
            .map(|value| value.images.as_slice())
            .unwrap_or(attachment.images.as_slice());
        let text = stored
            .as_ref()
            .map(|value| value.extracted_text.as_str())
            .unwrap_or(attachment.extracted_text.as_str());
        blocks.push(ContentBlock::text(format!("## File: {filename}\n\n")));
        for image in images.iter().take(MAX_IMAGES_PER_ATTACHMENT) {
            if image.data.len() < 2_000_000 {
                blocks.push(ContentBlock::Image {
                    source: ImageSource {
                        kind: "base64".into(),
                        media_type: image.media_type.clone(),
                        data: image.data.clone(),
                    },
                });
                blocks.push(ContentBlock::text(format!(
                    "(extracted image: {})\n",
                    image.media_type
                )));
            }
        }
        let display = if text.chars().count() > attachments::MAX_INLINE_TEXT_CHARS {
            let truncated: String = text
                .chars()
                .take(attachments::MAX_INLINE_TEXT_CHARS)
                .collect();
            format!("{truncated}\n\n[... content truncated]\n\n")
        } else {
            format!("{text}\n\n")
        };
        blocks.push(ContentBlock::text(display));
    }
    blocks.push(ContentBlock::text(format!(
        "---\n\n## User message\n\n{prompt}"
    )));
    MessageContent::from_blocks(blocks)
}

fn make_permission_resolver(
    tx: Tx,
    pending: Arc<PermissionMap>,
) -> nonoclaw_engine::PermissionResolver {
    Arc::new(move |request: PermissionRequest| {
        let tx = tx.clone();
        let pending = Arc::clone(&pending);
        Box::pin(async move {
            let (sender, receiver) = oneshot::channel();
            let request_id = Uuid::new_v4().to_string();
            pending.lock().await.insert(request_id.clone(), sender);
            send_msg(
                &tx,
                ServerMsg::PermissionRequired {
                    request_id,
                    tool_name: request.tool_name,
                    message: redact_text(&request.message),
                    input: redact_value(request.input),
                },
            )
            .await;
            receiver
                .await
                .unwrap_or_else(|_| PermissionDecision::deny("request cancelled"))
        })
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_options(
    config: &ResolvedConfig,
    model: String,
    max_turns: Option<u32>,
    append: Option<String>,
    arguments: Option<String>,
    tx: Tx,
    pending_permissions: Arc<PermissionMap>,
    permission_mode: nonoclaw_core::PermissionMode,
    skills_manager: Arc<RwLock<SkillsManager>>,
    background_registry: Arc<std::sync::Mutex<nonoclaw_tools::BackgroundTaskRegistry>>,
) -> EngineOptions {
    let mut options = config
        .resolve_run(RunConfigOverrides {
            source: ConfigSource::WebRequest {
                field: "run options".into(),
            },
            model: Some(model),
            max_turns,
            permission_mode: Some(permission_mode),
            append_system_prompt: append,
            arguments,
            is_non_interactive: false,
            ..Default::default()
        })
        .options;
    options.permission_resolver = Some(make_permission_resolver(tx, pending_permissions));
    options.skills_manager = Some(skills_manager);
    options.background_registry = Some(background_registry);
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_inline_attachments_are_bounded_and_filename_sanitized() {
        // **Validates: Requirements 8.8, 9.8, 11.2**
        let attachments = (0..MAX_ATTACHMENTS_PER_RUN + 2)
            .map(|index| AttachmentRef {
                id: format!("invalid-{index}"),
                filename: "../../private.txt".into(),
                extracted_text: "x".repeat(attachments::MAX_INLINE_TEXT_CHARS + 100),
                images: vec![],
            })
            .collect::<Vec<_>>();
        let content = enrich_prompt_with_attachments(
            "visible user request",
            &Some(attachments),
            std::path::Path::new("/nonexistent-upload-root"),
        );
        let MessageContent::Blocks(blocks) = content else {
            panic!("attachments must produce block content");
        };
        let text = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text.matches("## File:").count(), MAX_ATTACHMENTS_PER_RUN);
        assert!(!text.contains("../"));
        assert!(text.contains("[... content truncated]"));
        assert!(text.ends_with("visible user request"));
    }
}
