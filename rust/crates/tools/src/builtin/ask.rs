//! AskUserQuestion tool — surface a question to the user (Factor 7 structured
//! human-contact tool call). The interactive resolver is supplied by the active
//! UI adapter through [`ToolCtx::question`]; headless runs have none.

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tool::{QuestionFormat, QuestionRequest, QuestionUrgency, Tool, ToolCtx, ToolResult};

const PROMPT: &str = "Ask the user a question when you genuinely need a decision you can't infer\n(rare — most of the time, pick a sensible default and proceed).\n\nInput:\n- `question`: the question to ask (concise).\n- `context`: background explaining why you're asking (optional, helps the user decide).\n- `urgency`: \"low\" | \"medium\" | \"high\" — how time-sensitive or high-stakes this is (default: medium).\n- `format`: \"multiple_choice\" | \"yes_no\" | \"free_text\" — how the user should answer (default: multiple_choice).\n- `options`: 2-4 short option strings (required when format is multiple_choice or yes_no; ignored for free_text).\n\nReturns the user's answer, or a note that no answer was given. Use sparingly;\nprefer proceeding with a reasonable default.";

pub struct AskUserQuestionTool;

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &'static str {
        "AskUserQuestion"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Ask the user a question (multiple-choice, yes/no, or free text)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {"type":"string","description":"The question to ask"},
                "context": {"type":"string","description":"Background explaining why you're asking (optional)"},
                "urgency": {"type":"string","enum":["low","medium","high"],"description":"How time-sensitive or high-stakes this is (default: medium)"},
                "format": {"type":"string","enum":["multiple_choice","yes_no","free_text"],"description":"How the user should answer (default: multiple_choice)"},
                "options": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 2,
                    "maxItems": 4,
                    "description": "2-4 short options (required for multiple_choice and yes_no; ignored for free_text)"
                }
            },
            "required": ["question"]
        })
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        PermissionResult::allow()
    }
    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let question = input["question"].as_str().ok_or_else(|| Error::Tool {
            tool: "AskUserQuestion".into(),
            message: "missing `question`".into(),
        })?;

        // Parse structured Factor 7 fields.
        let context = input["context"]
            .as_str()
            .map(|s| s.to_string());

        let urgency = input["urgency"]
            .as_str()
            .and_then(|s| match s {
                "low" => Some(QuestionUrgency::Low),
                "medium" => Some(QuestionUrgency::Medium),
                "high" => Some(QuestionUrgency::High),
                _ => None,
            })
            .unwrap_or_default();
        // Jev calibration: a model-declared `high` urgency is a known source
        // of interruption fatigue. When the decision model is configured and
        // an interactive channel exists, one noul question downgrades
        // high→medium unless it agrees the situation is genuinely urgent.
        // Only-down, never-up; any failure/timeout keeps High.
        let urgency = calibrate_urgency(urgency, question, context.as_deref(), ctx).await;

        let format = input["format"]
            .as_str()
            .and_then(|s| match s {
                "multiple_choice" => Some(QuestionFormat::MultipleChoice),
                "yes_no" => Some(QuestionFormat::YesNo),
                "free_text" => Some(QuestionFormat::FreeText),
                _ => None,
            })
            .unwrap_or_default();

        // For yes_no, provide default options if none supplied.
        // For free_text, options are not required.
        // For multiple_choice, options are required (2-4).
        let options: Vec<String> = match format {
            QuestionFormat::YesNo => {
                let provided: Vec<String> = input["options"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                if provided.len() >= 2 {
                    provided
                } else {
                    vec!["Yes".to_string(), "No".to_string()]
                }
            }
            QuestionFormat::FreeText => {
                // Options ignored for free_text; pass empty.
                Vec::new()
            }
            QuestionFormat::MultipleChoice => {
                let opts: Vec<String> = input["options"]
                    .as_array()
                    .ok_or_else(|| Error::Tool {
                        tool: "AskUserQuestion".into(),
                        message: "`options` must be an array".into(),
                    })?
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                if opts.len() < 2 {
                    return Err(Error::Tool {
                        tool: "AskUserQuestion".into(),
                        message: "provide at least 2 options for multiple_choice format".into(),
                    });
                }
                opts
            }
        };

        let Some(resolver) = ctx.question else {
            return Ok(ToolResult::ok(
                "<human_response>\n  response: (no interactive channel — proceed with a sensible default)\n</human_response>",
            ));
        };
        let req = QuestionRequest {
            prompt: question.to_string(),
            options,
            context,
            urgency,
            format,
        };
        Ok(match tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            answer = resolver.ask(req) => answer,
        } {
            Some(answer) => ToolResult::ok(format!(
                "<human_response>\n  response: {answer}\n</human_response>"
            )),
            None => ToolResult::ok(
                "<human_response>\n  response: (dismissed — no answer provided)\n</human_response>"
                    .to_string(),
            ),
        })
    }
}

/// P(yes) below this downgrades High → Medium. Calibrated: noul is P(yes),
/// so "not clearly urgent" (< 0.5) is the demotion bar — a coin-flip answer
/// is not evidence of genuine urgency.
const URGENCY_DEMOTE_THRESHOLD: f64 = 0.5;

/// Budget for the calibration question before it can delay a user-visible
/// prompt. JevClient's own HTTP timeout is 5s; this bounds added latency.
const URGENCY_CALIBRATION_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Downgrade rule as a pure function: only High can move, only downward.
fn apply_urgency_calibration(urgency: QuestionUrgency, p_urgent: Option<f64>) -> QuestionUrgency {
    match (urgency, p_urgent) {
        (QuestionUrgency::High, Some(p)) if p < URGENCY_DEMOTE_THRESHOLD => {
            QuestionUrgency::Medium
        }
        (u, _) => u,
    }
}

/// Ask Jev whether this High-urgency question is genuinely high-stakes /
/// time-sensitive. No-op (returns input unchanged) when: urgency isn't High,
/// no interactive resolver (headless), Jev unconfigured, call fails, or the
/// 2s budget elapses. Fail-open keeps the model's original claim.
async fn calibrate_urgency(
    urgency: QuestionUrgency,
    question: &str,
    context: Option<&str>,
    ctx: &ToolCtx<'_>,
) -> QuestionUrgency {
    if urgency != QuestionUrgency::High || ctx.question.is_none() {
        return urgency;
    }
    let state = format!(
        "An AI coding agent is about to interrupt the user with an \
         urgent-marked question.\nQuestion: {question}\nContext: {}",
        context.unwrap_or("(none)")
    );
    let instructions = "Is this a genuinely high-stakes or time-sensitive \
        situation where interrupting the user now is warranted (data loss, \
        destructive action, blocking failure), rather than a routine \
        clarification the agent could handle more patiently?";
    let p_urgent = tokio::time::timeout(URGENCY_CALIBRATION_BUDGET, async {
        nonoclaw_api::jev::global_noul(&state, instructions).await
    })
    .await
    .unwrap_or_default();
    if let Some(p) = p_urgent {
        tracing::debug!(p, "ask: jev urgency calibration answered");
    } else {
        tracing::debug!("ask: jev urgency calibration unavailable — keeping High");
    }
    apply_urgency_calibration(urgency, p_urgent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_only_downgrades_high_below_threshold() {
        use QuestionUrgency::*;
        // High + clearly-not-urgent → Medium.
        assert_eq!(apply_urgency_calibration(High, Some(0.1)), Medium);
        // High + clearly-urgent stays High.
        assert_eq!(apply_urgency_calibration(High, Some(0.9)), High);
        // High + no answer (unconfigured/failed/timeout) stays High.
        assert_eq!(apply_urgency_calibration(High, None), High);
        // Never upgrades lower urgencies.
        assert_eq!(apply_urgency_calibration(Medium, Some(0.99)), Medium);
        assert_eq!(apply_urgency_calibration(Low, Some(0.99)), Low);
        // Default (unspecified → Medium) untouched.
        assert_eq!(apply_urgency_calibration(QuestionUrgency::default(), None), Medium);
    }
}
