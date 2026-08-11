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
use nonoclaw_tools::tool::{QuestionFormat, QuestionRequest, QuestionResolver, QuestionUrgency};
use tokio::sync::{oneshot, Mutex};
use uuid::Uuid;

use super::protocol::{send_msg, AttachmentRef, ServerMsg, Tx};
use crate::attachments;

pub(super) type PermissionMap = Mutex<HashMap<String, oneshot::Sender<PermissionDecision>>>;
pub(super) type QuestionMap = Mutex<HashMap<String, oneshot::Sender<Option<String>>>>;

const MAX_ATTACHMENTS_PER_RUN: usize = 8;
const MAX_IMAGES_PER_ATTACHMENT: usize = 8;

pub(super) struct WsQuestionResolver {
    pub pending: Arc<QuestionMap>,
    pub meta: super::permission_api::PendingQuestionMeta,
    pub tx: Tx,
}

impl QuestionResolver for WsQuestionResolver {
    fn ask(
        &self,
        req: QuestionRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + '_>> {
        let tx = self.tx.clone();
        let pending = Arc::clone(&self.pending);
        let meta = Arc::clone(&self.meta);
        Box::pin(async move {
            let (sender, receiver) = oneshot::channel();
            let request_id = Uuid::new_v4().to_string();
            pending.lock().await.insert(request_id.clone(), sender);
            // Store metadata so REST API can list/resolve the question.
            meta.lock().await.insert(
                request_id.clone(),
                super::permission_api::PendingQuestionInfo {
                    request_id: request_id.clone(),
                    prompt: redact_text(&req.prompt),
                    context: req.context.as_deref().map(redact_text),
                    options: req
                        .options
                        .iter()
                        .map(|o| redact_text(o))
                        .collect(),
                    urgency: match req.urgency {
                        QuestionUrgency::Low => "low".to_string(),
                        QuestionUrgency::Medium => "medium".to_string(),
                        QuestionUrgency::High => "high".to_string(),
                    },
                    format: match req.format {
                        QuestionFormat::MultipleChoice => "multiple_choice".to_string(),
                        QuestionFormat::YesNo => "yes_no".to_string(),
                        QuestionFormat::FreeText => "free_text".to_string(),
                    },
                },
            );
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
                    context: req.context.map(|c| redact_text(&c)),
                    urgency: match req.urgency {
                        QuestionUrgency::Low => "low",
                        QuestionUrgency::Medium => "medium",
                        QuestionUrgency::High => "high",
                    }
                    .to_string(),
                    format: match req.format {
                        QuestionFormat::MultipleChoice => "multiple_choice",
                        QuestionFormat::YesNo => "yes_no",
                        QuestionFormat::FreeText => "free_text",
                    }
                    .to_string(),
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
    include_images: bool,
    attachment_max_chars: usize,
) -> MessageContent {
    let attachments = match attachments {
        Some(attachments) if !attachments.is_empty() && attachment_max_chars > 0 => attachments,
        _ => return MessageContent::from_text(prompt),
    };
    let selected = attachments
        .iter()
        .take(MAX_ATTACHMENTS_PER_RUN)
        .collect::<Vec<_>>();
    let intro = "The user attached files. Extracted content follows; use it directly.\n\n";
    if intro.chars().count() > attachment_max_chars {
        return MessageContent::from_text(prompt);
    }

    let mut blocks = vec![ContentBlock::text(intro)];
    let mut remaining = attachment_max_chars - intro.chars().count();
    for (index, attachment) in selected.iter().enumerate() {
        if remaining == 0 {
            break;
        }
        // Divide the remaining budget across remaining files so one large
        // document cannot starve every later attachment.
        let slots = selected.len() - index;
        let share = remaining / slots.max(1);
        if share == 0 {
            break;
        }
        let mut file_remaining = share;

        // New clients send only an opaque upload ID. Legacy inline fields are
        // retained as a bounded compatibility fallback.
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

        push_attachment_text(
            &mut blocks,
            &format!("## File: {filename}\n\n"),
            &mut file_remaining,
            false,
        );
        // Prefer extracted/OCR text. It is cheaper and works for every
        // provider; raw base64 images consume only leftover budget.
        let text_was_truncated = text.chars().count() > file_remaining;
        push_attachment_text(&mut blocks, text, &mut file_remaining, text_was_truncated);
        push_attachment_text(&mut blocks, "\n\n", &mut file_remaining, false);

        if include_images {
            for image in images.iter().take(MAX_IMAGES_PER_ATTACHMENT) {
                let image_chars = image.data.chars().count()
                    + image.media_type.chars().count()
                    + "base64".chars().count();
                if image_chars <= file_remaining && image.data.len() < 2_000_000 {
                    blocks.push(ContentBlock::Image {
                        source: ImageSource {
                            kind: "base64".into(),
                            media_type: image.media_type.clone(),
                            data: image.data.clone(),
                        },
                    });
                    file_remaining -= image_chars;
                } else {
                    push_attachment_text(
                        &mut blocks,
                        "[image omitted by attachment token budget]\n",
                        &mut file_remaining,
                        false,
                    );
                }
            }
        }
        remaining = remaining.saturating_sub(share - file_remaining);
    }

    blocks.push(ContentBlock::text(format!(
        "---\n\n## User message\n\n{prompt}"
    )));
    MessageContent::from_blocks(blocks)
}

fn push_attachment_text(
    blocks: &mut Vec<ContentBlock>,
    text: &str,
    remaining: &mut usize,
    mark_truncated: bool,
) {
    if text.is_empty() || *remaining == 0 {
        return;
    }
    const MARKER: &str = "\n[attachment content truncated]\n";
    let marker_chars = if mark_truncated {
        MARKER.chars().count().min(*remaining)
    } else {
        0
    };
    let body_limit = remaining.saturating_sub(marker_chars);
    let mut rendered = text.chars().take(body_limit).collect::<String>();
    if mark_truncated {
        rendered.push_str(&MARKER.chars().take(marker_chars).collect::<String>());
    }
    let chars = rendered.chars().count();
    if chars > 0 {
        blocks.push(ContentBlock::text(rendered));
        *remaining = remaining.saturating_sub(chars);
    }
}

fn make_permission_resolver(
    tx: Tx,
    pending: Arc<PermissionMap>,
    meta: super::permission_api::PendingPermissionMeta,
) -> nonoclaw_engine::PermissionResolver {
    Arc::new(move |request: PermissionRequest| {
        let tx = tx.clone();
        let pending = Arc::clone(&pending);
        let meta = Arc::clone(&meta);
        Box::pin(async move {
            let (sender, receiver) = oneshot::channel();
            let request_id = Uuid::new_v4().to_string();
            pending.lock().await.insert(request_id.clone(), sender);
            meta.lock().await.insert(
                request_id.clone(),
                super::permission_api::PendingPermissionInfo {
                    request_id: request_id.clone(),
                    tool_name: request.tool_name.clone(),
                    message: redact_text(&request.message),
                    input: redact_value(request.input.clone()),
                },
            );
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
    permission_meta: super::permission_api::PendingPermissionMeta,
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
    options.permission_resolver = Some(make_permission_resolver(
        tx,
        pending_permissions,
        permission_meta,
    ));
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
                extracted_text: "x".repeat(50_000 + 100),
                images: vec![],
            })
            .collect::<Vec<_>>();
        let content = enrich_prompt_with_attachments(
            "visible user request",
            &Some(attachments),
            std::path::Path::new("/nonexistent-upload-root"),
            false,
            50_000,
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
        assert!(text.contains("[attachment content truncated]"));
        assert!(text.ends_with("visible user request"));
    }

    fn attachment_with_image() -> AttachmentRef {
        AttachmentRef {
            id: "missing-upload".into(),
            filename: "report.pdf".into(),
            extracted_text: "Extracted PDF text".into(),
            images: vec![super::super::protocol::ImageRef {
                media_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            }],
        }
    }

    #[test]
    fn text_only_attachment_enrichment_omits_images_but_keeps_extracted_text() {
        let content = enrich_prompt_with_attachments(
            "summarize",
            &Some(vec![attachment_with_image()]),
            std::path::Path::new("/nonexistent-upload-root"),
            false,
            usize::MAX,
        );
        let MessageContent::Blocks(blocks) = content else {
            panic!("attachments must produce block content");
        };
        assert!(!blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. })));
        let text = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.contains("Extracted PDF text"));
        assert!(text.ends_with("summarize"));
    }

    #[test]
    fn vision_attachment_enrichment_keeps_images_and_extracted_text() {
        let content = enrich_prompt_with_attachments(
            "summarize",
            &Some(vec![attachment_with_image()]),
            std::path::Path::new("/nonexistent-upload-root"),
            true,
            usize::MAX,
        );
        let MessageContent::Blocks(blocks) = content else {
            panic!("attachments must produce block content");
        };
        assert_eq!(
            blocks
                .iter()
                .filter(|block| matches!(block, ContentBlock::Image { .. }))
                .count(),
            1
        );
        assert!(blocks.iter().any(|block| {
            matches!(block, ContentBlock::Text { text, .. } if text.contains("Extracted PDF text"))
        }));
    }

    #[test]
    fn attachment_partition_is_global_and_counts_encoded_images() {
        let mut attachment = attachment_with_image();
        attachment.extracted_text = "document ".repeat(200);
        attachment.images[0].data = "a".repeat(1_000);
        let content = enrich_prompt_with_attachments(
            "keep this user request",
            &Some(vec![attachment]),
            std::path::Path::new("/nonexistent-upload-root"),
            true,
            120,
        );
        let MessageContent::Blocks(blocks) = content else {
            panic!("attachments must produce block content");
        };
        assert!(!blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. })));
        let attachment_chars: usize = blocks[..blocks.len() - 1]
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, .. } => text.chars().count(),
                ContentBlock::Image { source } => source.data.chars().count(),
                _ => 0,
            })
            .sum();
        assert!(attachment_chars <= 120);
        assert!(matches!(
            blocks.last(),
            Some(ContentBlock::Text { text, .. }) if text.ends_with("keep this user request")
        ));
    }
}
