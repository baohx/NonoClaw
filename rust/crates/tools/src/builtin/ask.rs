//! AskUserQuestion tool — surface a multiple-choice question to the user.
//! Mirrors `src/tools/AskUserQuestionTool/`. The interactive resolver lives in
//! the TUI (via [`ToolCtx::question`]); headless runs have none.

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tool::{QuestionRequest, Tool, ToolCtx, ToolResult};

const PROMPT: &str = "Ask the user a multiple-choice question when you genuinely need a decision\nyou can't infer (rare — most of the time, pick a sensible default and proceed).\n\nInput:\n- `question`: the question to ask (concise).\n- `options`: 2-4 short option strings.\n\nReturns the chosen option text, or a note that no answer was given. Use\nsparingly; prefer proceeding with a reasonable default.";

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
        "Ask the user a multiple-choice question."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {"type":"string","description":"The question to ask"},
                "options": {
                    "type": "array",
                    "items": {"type":"string"},
                    "minItems": 2,
                    "maxItems": 4,
                    "description": "2-4 short options"
                }
            },
            "required": ["question", "options"]
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
        _cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let question = input["question"].as_str().ok_or_else(|| Error::Tool {
            tool: "AskUserQuestion".into(),
            message: "missing `question`".into(),
        })?;
        let options: Vec<String> = input["options"]
            .as_array()
            .ok_or_else(|| Error::Tool {
                tool: "AskUserQuestion".into(),
                message: "`options` must be an array".into(),
            })?
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        if options.len() < 2 {
            return Err(Error::Tool {
                tool: "AskUserQuestion".into(),
                message: "provide at least 2 options".into(),
            });
        }

        let Some(resolver) = ctx.question else {
            return Ok(ToolResult::ok(
                "No interactive question channel available; pick a sensible default and proceed.",
            ));
        };
        let req = QuestionRequest {
            prompt: question.to_string(),
            options,
        };
        Ok(match resolver.ask(req).await {
            Some(answer) => ToolResult::ok(format!("User chose: {answer}")),
            None => ToolResult::ok("User dismissed the question; proceed with a default."),
        })
    }
}
