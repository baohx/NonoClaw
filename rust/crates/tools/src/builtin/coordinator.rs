//! Coordinator tool — parallel multi-subagent dispatch. Mirrors the role of
//! `src/coordinator/`: fan out independent subtasks to subagents, gather
//! results. Uses [`SubagentRunner::run_subagents`] for concurrent execution.
//! Child agents never see Agent/Coordinator/Graph, so nesting is blocked.

use crate::tool::{normalize_optional_profile, SubagentRequest, Tool, ToolCtx, ToolResult};
use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionDecision, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const MAX_TASKS: usize = 16;
const TASKS_ARRAY_ERROR: &str = "Coordinator requires a top-level `tasks` array; top-level `description`, `prompt`, or `subagent_type` is invalid. Example: {\"tasks\":[{\"description\":\"search team A\",\"prompt\":\"Search team A and return sources\"},{\"description\":\"search team B\",\"prompt\":\"Search team B and return sources\"}]}";
const PROMPT: &str = "Dispatch independent tasks to subagents in parallel; input MUST be an object with a top-level `tasks` array of {description, prompt, profile?} objects, never top-level description/prompt/subagent_type. Each child's streaming status, tool activity, permissions, and terminal events are reported under this Coordinator call.\n\nUse this for:\n- Searching/reading/investigating multiple independent items at once\n- Anything where subtasks don't depend on each other\n\nInput: a `tasks` array of {description, prompt, profile?}, one per subtask. An optional profile names `.nonoclaw/agents/<profile>.md` and may only tighten parent permissions/tools.";

pub struct CoordinatorTool;

#[async_trait]
impl Tool for CoordinatorTool {
    fn name(&self) -> &'static str {
        "Coordinator"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Dispatch parallel subagents; input MUST contain a top-level `tasks` array of {description, prompt, profile?} objects."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "tasks":{
                    "type":"array",
                    "minItems":1,
                    "maxItems":MAX_TASKS,
                    "items":{
                        "type":"object",
                        "properties":{
                            "description":{"type":"string"},
                            "prompt":{"type":"string"},
                            "profile":{"type":"string","description":"Optional agent profile name from .nonoclaw/agents/<name>.md"}
                        },
                        "required":["description","prompt"]
                    }
                }
            },
            "required":["tasks"]
        })
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        PermissionDecision::ask("dispatch parallel subagents")
    }
    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let task_values = input["tasks"].as_array().ok_or_else(|| Error::Tool {
            tool: "Coordinator".into(),
            message: TASKS_ARRAY_ERROR.into(),
        })?;
        if task_values.is_empty() {
            return Err(Error::Tool {
                tool: "Coordinator".into(),
                message: "`tasks` must contain at least one task".into(),
            });
        }
        if task_values.len() > MAX_TASKS {
            return Err(Error::Tool {
                tool: "Coordinator".into(),
                message: format!("`tasks` exceeds the maximum of {MAX_TASKS}"),
            });
        }
        let mut tasks = Vec::with_capacity(task_values.len());
        for (index, value) in task_values.iter().enumerate() {
            let object = value.as_object().ok_or_else(|| Error::Tool {
                tool: "Coordinator".into(),
                message: format!("tasks[{index}] must be an object"),
            })?;
            let description = object
                .get("description")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Tool {
                    tool: "Coordinator".into(),
                    message: format!("tasks[{index}].description must be a string"),
                })?;
            let prompt = object
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Tool {
                    tool: "Coordinator".into(),
                    message: format!("tasks[{index}].prompt must be a string"),
                })?;
            let profile =
                normalize_optional_profile(object.get("profile")).map_err(|()| Error::Tool {
                    tool: "Coordinator".into(),
                    message: format!("tasks[{index}].profile must be a string, null, or omitted"),
                })?;
            tasks.push(SubagentRequest {
                prompt: prompt.to_owned(),
                description: description.to_owned(),
                profile,
                parent_tool_use_id: ctx.tool_use_id.to_owned(),
                index: Some(index as u32),
            });
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let Some(runner) = ctx.subagent else {
            return Err(Error::Tool {
                tool: "Coordinator".into(),
                message: "subagent runner unavailable".into(),
            });
        };
        let results = tokio::select! {
            biased; _ = cancel.cancelled() => return Err(Error::Cancelled),
            r = runner.run_subagents(tasks.clone()) => r,
        };
        if results.len() != tasks.len() {
            return Err(Error::Tool {
                tool: "Coordinator".into(),
                message: format!(
                    "subagent runner returned {} results for {} tasks",
                    results.len(),
                    tasks.len()
                ),
            });
        }
        let mut out = String::new();
        for (i, (task, result)) in tasks.iter().zip(results.iter()).enumerate() {
            let body = match result {
                Ok(answer) => answer.as_str(),
                Err(error) => {
                    out.push_str(&format!(
                        "--- Subtask {}: {}\nError: {}\n\n",
                        i + 1,
                        task.description,
                        error
                    ));
                    continue;
                }
            };
            out.push_str(&format!(
                "--- Subtask {}: {}\n{}\n\n",
                i + 1,
                task.description,
                body
            ));
        }
        Ok(ToolResult::ok(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingRunner {
        requests: Mutex<Vec<SubagentRequest>>,
    }

    impl crate::tool::SubagentRunner for RecordingRunner {
        fn run_subagent<'a>(
            &'a self,
            request: SubagentRequest,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request);
                Ok("fixture result".into())
            })
        }
    }

    #[test]
    fn schema_exposes_optional_profile_and_bounded_tasks() {
        let schema = CoordinatorTool.input_schema();
        let tasks = &schema["properties"]["tasks"];
        assert_eq!(tasks["maxItems"], MAX_TASKS);
        assert_eq!(tasks["items"]["properties"]["profile"]["type"], "string");
        assert!(!tasks["items"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "profile"));
    }

    #[test]
    fn coordinator_is_active_and_exposes_tasks_schema_without_tool_search() {
        let (registry, _) = crate::builtin::register_all();
        let active = registry.active_definitions(None);
        let coordinator = active
            .iter()
            .find(|definition| definition.name == "Coordinator")
            .expect("Coordinator must be sent to the provider on the first turn");
        assert_eq!(
            coordinator.input_schema["properties"]["tasks"]["type"],
            "array"
        );
        assert!(active
            .iter()
            .all(|definition| definition.name != "WebSearch"));

        let first_line = CoordinatorTool.prompt().lines().next().unwrap();
        assert!(first_line.contains("top-level `tasks` array"));
        assert!(CoordinatorTool
            .description()
            .contains("top-level `tasks` array"));
    }

    #[tokio::test]
    async fn agent_and_coordinator_normalize_optional_profile_consistently() {
        let runner = RecordingRunner::default();
        let options = crate::tool::ToolOptions {
            model: "fixture-model".into(),
            permission_mode: nonoclaw_core::PermissionMode::Auto,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        let cancel = CancellationToken::new();
        let context = ToolCtx {
            cwd: std::path::Path::new("/tmp"),
            options: &options,
            cancel: &cancel,
            tool_use_id: "profile-normalization-fixture",
            task_scope: None,
            subagent: Some(&runner),
            graph_runner: None,
            question: None,
            background_registry: None,
        };
        let cases = vec![
            (None, None),
            (Some(Value::Null), None),
            (Some(Value::String(String::new())), None),
            (Some(Value::String("  \t ".into())), None),
            (
                Some(Value::String("  safe-profile  ".into())),
                Some("safe-profile"),
            ),
        ];

        for (profile_value, expected) in cases {
            let mut task = json!({
                "description": "fixture task",
                "prompt": "return fixture result"
            });
            let mut agent_input = task.clone();
            if let Some(profile_value) = profile_value {
                task.as_object_mut()
                    .unwrap()
                    .insert("profile".into(), profile_value.clone());
                agent_input
                    .as_object_mut()
                    .unwrap()
                    .insert("profile".into(), profile_value);
            }
            CoordinatorTool
                .call(json!({"tasks": [task]}), &context, cancel.child_token())
                .await
                .unwrap();
            crate::builtin::AgentTool
                .call(agent_input, &context, cancel.child_token())
                .await
                .unwrap();

            let requests = runner.requests.lock().unwrap();
            let coordinator_profile = requests[requests.len() - 2].profile.as_deref();
            let agent_profile = requests[requests.len() - 1].profile.as_deref();
            assert_eq!(coordinator_profile, expected);
            assert_eq!(agent_profile, expected);
        }

        let before = runner.requests.lock().unwrap().len();
        let coordinator_error = CoordinatorTool
            .call(
                json!({"tasks": [{
                    "description": "fixture task",
                    "prompt": "return fixture result",
                    "profile": 7
                }]}),
                &context,
                cancel.child_token(),
            )
            .await
            .unwrap_err();
        assert!(coordinator_error
            .to_string()
            .contains("profile must be a string, null, or omitted"));
        let agent_error = crate::builtin::AgentTool
            .call(
                json!({
                    "description": "fixture task",
                    "prompt": "return fixture result",
                    "profile": {"name": "unsafe-shape"}
                }),
                &context,
                cancel.child_token(),
            )
            .await
            .unwrap_err();
        assert!(agent_error
            .to_string()
            .contains("profile` must be a string, null, or omitted"));
        assert_eq!(runner.requests.lock().unwrap().len(), before);
    }

    #[tokio::test]
    async fn legacy_top_level_shape_is_rejected_with_a_valid_tasks_example() {
        let options = crate::tool::ToolOptions {
            model: "fixture-model".into(),
            permission_mode: nonoclaw_core::PermissionMode::Auto,
            is_non_interactive: true,
            max_budget_usd: None,
        };
        let cancel = CancellationToken::new();
        let context = ToolCtx {
            cwd: std::path::Path::new("/tmp"),
            options: &options,
            cancel: &cancel,
            tool_use_id: "coordinator-fixture",
            task_scope: None,
            subagent: None,
            graph_runner: None,
            question: None,
            background_registry: None,
        };
        let error = CoordinatorTool
            .call(
                json!({
                    "description": "parallel search",
                    "prompt": "create two child agents",
                    "subagent_type": "agent-reach"
                }),
                &context,
                cancel.clone(),
            )
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("top-level `tasks` array"));
        assert!(
            message.contains("top-level `description`, `prompt`, or `subagent_type` is invalid")
        );
        assert!(message.contains("{\"tasks\":[{\"description\":\"search team A\""));
        assert!(!message.contains("subagent runner unavailable"));
    }
}
