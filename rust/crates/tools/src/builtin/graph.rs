//! Graph tool — runs a declarative agent graph from
//! `.nonoclaw/graphs/<name>.md` and returns the graph's final answer.
//!
//! A graph is a markdown file whose YAML frontmatter declares nodes (subagent
//! runs, LLM routers, human gates) connected by `next` edges; the engine walks
//! the resulting dataflow DAG with fan-out/fan-in, router branches, gate
//! approvals, checkpoint resume, and shared state. See
//! `rust/crates/engine/src/graph/` for the executor.

use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionDecision, PermissionResult, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tool::{Tool, ToolCtx, ToolResult};

const PROMPT: &str = "Runs a named declarative agent graph from `.nonoclaw/graphs/<name>.md` and returns its final answer. A graph is a fixed pipeline of subagent nodes (agent / LLM router / human gate) with shared state — use it for repeatable multi-step processes instead of improvising the same steps manually.\n\nInput:\n- `graph`: the graph file name (without `.md`).\n- `args`: optional object merged into the graph's input state; values are referenced in node prompts as `{field}`.\n- `resume`: optional boolean; when true, resume from the graph's last checkpoint (completed nodes are skipped).\n\nNotes:\n- Lists available graphs when `graph` is omitted (or use `ListGraphs`).\n- Node outputs are written to state under their node id and can be referenced by later nodes.\n- A graph may pause at `gate` nodes for human approval.";

pub struct GraphTool;

#[async_trait]
impl Tool for GraphTool {
    fn name(&self) -> &'static str {
        "Graph"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Run a declarative agent graph from .nonoclaw/graphs/<name>.md."
    }
    fn search_hint(&self) -> Option<&'static str> {
        Some("graph pipeline workflow dag orchestration declarative")
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "graph": {"type":"string","description":"Graph name (filename stem under .nonoclaw/graphs/)"},
                "args": {"type":"object","description":"Optional input state merged into the graph"},
                "resume": {"type":"boolean","description":"Resume from the last checkpoint"}
            },
            "required": ["graph"]
        })
    }

    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn max_result_size_chars(&self) -> usize {
        120_000
    }

    async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        // Running a graph executes multiple subagents; surface it to the user
        // (each subagent still passes its own permission gate per tool).
        PermissionDecision::ask("run an agent graph")
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let name = input["graph"].as_str().ok_or_else(|| Error::Tool {
            tool: "Graph".into(),
            message: "missing required field `graph`".into(),
        })?;
        let args = input.get("args").cloned().unwrap_or(Value::Null);
        let resume = input["resume"].as_bool().unwrap_or(false);

        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        let Some(runner) = ctx.graph_runner else {
            return Err(Error::Tool {
                tool: "Graph".into(),
                message: "graph runner unavailable in this context".into(),
            });
        };

        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            r = runner.run_graph(name, args, resume) => r,
        }?;

        Ok(ToolResult::ok(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_exposes_graph_args_resume() {
        let schema = GraphTool.input_schema();
        assert_eq!(schema["properties"]["graph"]["type"], "string");
        assert_eq!(schema["properties"]["args"]["type"], "object");
        assert_eq!(schema["properties"]["resume"]["type"], "boolean");
        assert_eq!(schema["required"][0], "graph");
    }
}
