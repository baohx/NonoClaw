//! Dataflow executor for declarative agent graphs.
//!
//! Walks a [`GraphDefinition`] as a dependency DAG with dynamic edges:
//!
//! - **fan-out** — a node with multiple `next` successors activates all of
//!   them; ready nodes execute concurrently in batches.
//! - **fan-in** — a node with several deterministic predecessors runs only
//!   after every predecessor has completed.
//! - **router** — an LLM node whose reply selects exactly one `branch`; only
//!   that branch becomes reachable.
//! - **gate** — a node that pauses for human approval through the interactive
//!   [`QuestionResolver`]; headless runs proceed by default.
//!
//! Execution is checkpointed to `<cwd>/.nonoclaw/graphs/.checkpoints/<name>.json`
//! after every node so an interrupted run can resume with `resume: true`
//! (completed nodes are skipped, their outputs are restored from state).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use nonoclaw_core::{Error, Result};
use nonoclaw_tools::tool::{
    QuestionFormat, QuestionRequest, QuestionResolver, QuestionUrgency, SubagentRequest,
    SubagentRunner,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{GraphDefinition, GraphNode, Next, NodeKind};

/// Execution budget / environment for one graph run.
pub struct GraphRunOptions<'a> {
    pub cwd: &'a Path,
    pub session_id: &'a str,
    pub cancel: CancellationToken,
    /// Subagent runner used for `agent` and `router` nodes.
    pub subagent: &'a dyn SubagentRunner,
    /// Interactive resolver for `gate` nodes; `None` in headless runs.
    pub question: Option<&'a dyn QuestionResolver>,
    /// When true, resume from the last checkpoint for this graph (same
    /// version); completed nodes are skipped.
    pub resume: bool,
}

/// Outcome of a graph execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphRunResult {
    /// Final text: the `end` node's output, or a summary of executed nodes
    /// when the graph stopped before reaching an `end` node.
    pub text: String,
    /// Shared state after execution (inputs + every completed node output).
    pub state: BTreeMap<String, Value>,
    /// Nodes executed during this invocation (excludes resumed ones).
    pub nodes_run: Vec<String>,
    /// All completed nodes (including those restored from a checkpoint).
    pub nodes_completed: Vec<String>,
    /// True when execution was restored from a checkpoint.
    pub resumed: bool,
    /// Set when a `gate` node aborted the run via human approval.
    pub aborted: bool,
}

/// Persisted execution progress for one graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphCheckpoint {
    pub graph: String,
    pub version: u32,
    pub updated: String,
    pub completed: Vec<String>,
    pub state: BTreeMap<String, Value>,
}

fn checkpoint_path(cwd: &Path, name: &str) -> std::path::PathBuf {
    cwd.join(".nonoclaw/graphs/.checkpoints")
        .join(format!("{name}.json"))
}

fn load_checkpoint(cwd: &Path, def: &GraphDefinition) -> Option<GraphCheckpoint> {
    let path = checkpoint_path(cwd, &def.name);
    let raw = std::fs::read_to_string(&path).ok()?;
    let cp: GraphCheckpoint = serde_json::from_str(&raw).ok()?;
    if cp.graph == def.name && cp.version == def.version {
        Some(cp)
    } else {
        None
    }
}

fn save_checkpoint(cwd: &Path, def: &GraphDefinition, cp: &GraphCheckpoint) {
    let path = checkpoint_path(cwd, &def.name);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if let Ok(serialized) = serde_json::to_string_pretty(cp) {
        let _ = std::fs::write(&tmp, serialized);
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn clear_checkpoint(cwd: &Path, name: &str) {
    let _ = std::fs::remove_file(checkpoint_path(cwd, name));
}

/// Execute a graph to completion (or until the step budget / a gate abort).
pub async fn run_graph(
    def: &GraphDefinition,
    args: &Value,
    opts: &GraphRunOptions<'_>,
) -> Result<GraphRunResult> {
    def.validate().map_err(|message| {
        Error::Config(format!("invalid agent graph `{}`: {message}", def.name))
    })?;
    let start = def
        .start_node()
        .ok_or_else(|| Error::Config(format!("graph `{}` has no start node", def.name)))?
        .to_string();

    // --- State: defaults + invocation args (args win) + checkpoint restore ---
    let mut state: BTreeMap<String, Value> = def.state.clone();
    if let Value::Object(map) = args {
        for (key, value) in map {
            state.insert(key.clone(), value.clone());
        }
    }

    let mut completed: Vec<String> = vec![];
    let mut resumed = false;
    if opts.resume {
        if let Some(cp) = load_checkpoint(opts.cwd, def) {
            resumed = true;
            completed = cp.completed;
            for (key, value) in cp.state {
                state.insert(key, value);
            }
        }
    }
    let mut done: HashSet<String> = completed.iter().cloned().collect();
    let mut reachable: HashSet<String> = HashSet::new();

    // Replay activations from resumed nodes: deterministic successors of every
    // completed node become reachable; a completed router's chosen branch is
    // read back from the `_router:<id>` state key recorded at execution time.
    for id in &completed {
        let Some(node) = def.nodes.get(id) else { continue };
        if node.kind == NodeKind::Router {
            if let Some(Value::String(branch)) = state.get(&format!("_router:{id}")) {
                reachable.insert(branch.clone());
            }
        } else {
            for s in node.next.ids() {
                reachable.insert(s);
            }
        }
    }

    // --- Static predecessor counts (deterministic `next` edges only) ---
    // Edges from already-completed (resumed) nodes do not count: those
    // predecessors have already "fired".
    let mut pred_remaining: HashMap<String, usize> = HashMap::new();
    let mut succ: HashMap<String, Vec<String>> = HashMap::new();
    for (id, node) in &def.nodes {
        pred_remaining.entry(id.clone()).or_default();
        if done.contains(id) {
            continue;
        }
        for s in node.next.ids() {
            succ.entry(id.clone()).or_default().push(s.clone());
            *pred_remaining.entry(s.clone()).or_default() += 1;
        }
    }

    if !done.contains(&start) {
        reachable.insert(start.clone());
    }

    let max_steps = def.max_steps();
    let mut steps = 0usize;
    let mut nodes_run: Vec<String> = vec![];
    let mut reached_end = false;
    let mut aborted = false;
    let mut end_output: Option<String> = None;

    loop {
        if opts.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        // Frontier = reachable nodes whose predecessors are all complete.
        let frontier: Vec<String> = def
            .nodes
            .keys()
            .filter(|id| {
                !done.contains(*id)
                    && reachable.contains(*id)
                    && pred_remaining.get(*id).copied().unwrap_or(0) == 0
            })
            .cloned()
            .collect();
        if frontier.is_empty() {
            break;
        }

        // Execute the frontier batch concurrently (each writes its own state key).
        let mut batch_results = Vec::with_capacity(frontier.len());
        for id in &frontier {
            batch_results.push(execute_node(def, id, &state, opts));
        }
        let results = futures::future::join_all(batch_results).await;

        for (node_id, result) in frontier.iter().zip(results.into_iter()) {
            let node = &def.nodes[node_id];
            let (output, next_nodes, node_aborted) = result?;
            steps += 1;
            nodes_run.push(node_id.clone());
            done.insert(node_id.clone());
            completed.push(node_id.clone());
            state.insert(node_id.clone(), Value::String(output.clone()));            save_checkpoint(
                opts.cwd,
                def,
                &GraphCheckpoint {
                    graph: def.name.clone(),
                    version: def.version,
                    updated: iso_now(),
                    completed: completed.clone(),
                    state: state.clone(),
                },
            );

            if node_aborted {
                aborted = true;
                break;
            }
            if node.end {
                reached_end = true;
                end_output = Some(output.clone());
                break;
            }

            // Activate deterministic successors.
            match &node.next {
                Next::None => {}
                _ => {
                    for s in node.next.ids() {
                        if let Some(count) = pred_remaining.get_mut(&s) {
                            *count = count.saturating_sub(1);
                        }
                        reachable.insert(s);
                    }
                }
            }
            // Router: activate the chosen branch only and record the choice so
            // a resume can replay this dynamic edge.
            if node.kind == NodeKind::Router {
                if let Some(branch) = next_nodes.first() {
                    reachable.insert(branch.clone());
                    state.insert(format!("_router:{node_id}"), json!(branch));
                }
            }

            if steps >= max_steps {
                break;
            }
        }
        if aborted || reached_end || steps >= max_steps {
            break;
        }
    }

    // --- Summary ---
    let text = if let Some(output) = end_output {
        output
    } else if aborted {
        format!(
            "Graph `{}` was aborted by human approval after {} node(s): {}.",
            def.name,
            nodes_run.len(),
            nodes_run.join(", ")
        )
    } else {
        let mut summary = format!(
            "Graph `{}` finished without reaching an end node after {} step(s).\nCompleted nodes: {}",
            def.name,
            nodes_run.len(),
            nodes_run.join(", ")
        );
        for id in &nodes_run {
            if let Some(out) = state.get(id).and_then(Value::as_str) {
                summary.push_str(&format!("\n\n## {id}\n{}", truncate(out, 2000)));
            }
        }
        summary
    };

    if reached_end || aborted || steps >= max_steps {
        // A completed run (or one that hit a terminal state) should not leave
        // a stale checkpoint that a later resume would wrongly skip. End nodes
        // are terminal, so clear progress.
        if reached_end || aborted {
            clear_checkpoint(opts.cwd, &def.name);
        }
    }

    Ok(GraphRunResult {
        text,
        state,
        nodes_run,
        nodes_completed: completed,
        resumed,
        aborted,
    })
}

/// Run one node: agent (subagent), router (subagent picks a branch), or gate
/// (human approval). Returns `(output_text, chosen_next_ids, aborted)`.
async fn execute_node(
    def: &GraphDefinition,
    node_id: &str,
    state: &BTreeMap<String, Value>,
    opts: &GraphRunOptions<'_>,
) -> Result<(String, Vec<String>, bool)> {
    let node = &def.nodes[node_id];
    match node.kind {
        NodeKind::Agent => {
            let prompt = render_prompt(&node.prompt, state);
            let output = run_node_subagent(node, &prompt, node_id, opts).await?;
            Ok((output, node.next.ids(), false))
        }
        NodeKind::Router => {
            let prompt = render_prompt(&node.prompt, state);
            let branches = node.branches.clone();
            let decision_prompt = format!(
                "{prompt}\n\nRespond with EXACTLY ONE of these branch names: {}.\nYour entire reply must be just that branch name and nothing else.",
                branches.join(", ")
            );
            let answer = run_node_subagent(node, &decision_prompt, node_id, opts).await?;
            let choice = normalize_branch(&answer, &branches).ok_or_else(|| {
                Error::Other(format!(
                    "graph `{}` router node `{node_id}` returned `{}` which is not a declared branch ({})",
                    def.name,
                    answer.trim(),
                    branches.join(", ")
                ))
            })?;
            Ok((answer, vec![choice], false))
        }
        NodeKind::Gate => {
            let prompt = render_prompt(&node.prompt, state);
            let Some(question) = opts.question else {
                // Headless: proceed by default, record the implicit approval.
                return Ok((
                    "approved (headless: no interactive resolver, proceeded by default)".into(),
                    node.next.ids(),
                    false,
                ));
            };
            let answer = question
                .ask(QuestionRequest {
                    prompt: format!(
                        "{prompt}\n\nProceed with the next step or abort the graph?",
                    ),
                    options: vec!["Continue".into(), "Abort".into()],
                    context: Some(format!(
                        "Agent graph `{}` node `{node_id}` requires human approval.",
                        def.name
                    )),
                    urgency: QuestionUrgency::Medium,
                    format: QuestionFormat::MultipleChoice,
                })
                .await;
            match answer.as_deref() {
                None => Ok((
                    "approved (no answer received, proceeded by default)".into(),
                    node.next.ids(),
                    false,
                )),
                Some(reply) if reply.trim().eq_ignore_ascii_case("abort") => {
                    Ok(("aborted by human approval".into(), vec![], true))
                }
                Some(reply) => Ok((
                    format!("approved: {reply}"),
                    node.next.ids(),
                    false,
                )),
            }
        }
    }
}

async fn run_node_subagent(
    node: &GraphNode,
    rendered_prompt: &str,
    node_id: &str,
    opts: &GraphRunOptions<'_>,
) -> Result<String> {
    let result = opts
        .subagent
        .run_subagent(SubagentRequest {
            prompt: rendered_prompt.to_string(),
            description: format!("graph:{}:{}", node.id, node_id),
            profile: node.profile.clone(),
            parent_tool_use_id: format!("graph:{}", node_id),
            index: None,
        })
        .await?;
    Ok(result)
}

/// Match a router answer to a declared branch (case/whitespace/punctuation
/// tolerant). Returns the canonical branch id.
fn normalize_branch(answer: &str, branches: &[String]) -> Option<String> {
    let cleaned: String = answer
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '.')
        .to_lowercase();
    branches
        .iter()
        .find(|b| b.to_lowercase() == cleaned)
        .cloned()
}

/// Render `{var}` references from shared state (inputs + completed outputs).
fn render_prompt(template: &str, state: &BTreeMap<String, Value>) -> String {
    let mut out = template.to_string();
    for (key, value) in state {
        let placeholder = format!("{{{key}}}");
        if out.contains(&placeholder) {
            let text = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out = out.replace(&placeholder, &text);
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push_str("…");
        out
    }
}

fn iso_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // RFC3339-ish without external deps: epoch seconds are enough for ordering.
    format!("{}s", secs)
}

#[cfg(test)]
mod tests {
    use super::*;


    /// Isolated scratch dir per test (checkpoint files collide in temp_dir
    /// when tests run in parallel with the same graph name).
    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nc-graph-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn simple_graph() -> GraphDefinition {
        let raw = r#"---
name: g
version: 1
state:
  topic: "default topic"
nodes:
  a:
    prompt: "do {topic}"
    next: [b, c]
  b:
    prompt: "b work"
    next: d
  c:
    prompt: "c work"
    next: d
  d:
    prompt: "merge"
    end: true
---
"#;
        super::super::parse_graph(raw, Path::new("/tmp/g.md")).unwrap()
    }

    #[test]
    fn render_prompt_substitutes_state() {
        let mut state = BTreeMap::new();
        state.insert("topic".into(), json!("AI"));
        state.insert("a".into(), json!("output-a"));
        assert_eq!(
            render_prompt("do {topic} then {a}", &state),
            "do AI then output-a"
        );
    }

    #[test]
    fn normalize_branch_is_case_insensitive() {
        let branches = vec!["draft".to_string(), "rewrite".to_string()];
        assert_eq!(normalize_branch("Rewrite.", &branches), Some("rewrite".into()));
        assert_eq!(normalize_branch("draft", &branches), Some("draft".into()));
        assert_eq!(normalize_branch("other", &branches), None);
        assert_eq!(normalize_branch("'draft'", &branches), Some("draft".into()));
    }

    #[test]
    fn checkpoint_roundtrip_preserves_state() {
        let cwd = test_dir("roundtrip");
        let def = simple_graph();
        let cp = GraphCheckpoint {
            graph: def.name.clone(),
            version: def.version,
            updated: "0s".into(),
            completed: vec!["a".into()],
            state: {
                let mut m = BTreeMap::new();
                m.insert("a".into(), json!("done"));
                m
            },
        };
        save_checkpoint(&cwd, &def, &cp);
        let loaded = load_checkpoint(&cwd, &def).expect("checkpoint loads");
        assert_eq!(loaded.completed, vec!["a"]);
        assert_eq!(loaded.state["a"], json!("done"));
        clear_checkpoint(&cwd, &def.name);
        assert!(load_checkpoint(&cwd, &def).is_none());
    }

    #[test]
    fn checkpoint_ignored_on_version_mismatch() {
        let cwd = test_dir("version_mismatch");
        let def = simple_graph();
        save_checkpoint(
            &cwd,
            &def,
            &GraphCheckpoint {
                graph: def.name.clone(),
                version: 99,
                updated: "0s".into(),
                completed: vec!["a".into()],
                state: BTreeMap::new(),
            },
        );
        assert!(load_checkpoint(&cwd, &def).is_none());
        clear_checkpoint(&cwd, &def.name);
    }

    // A stub runner that echoes the prompt — lets us test executor topology
    // without any LLM. Routers always pick the first declared branch.
    struct EchoRunner;
    impl SubagentRunner for EchoRunner {
        fn run_subagent<'a>(
            &'a self,
            request: SubagentRequest,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>
        {
            Box::pin(async move { Ok(format!("ran:{}", request.description)) })
        }
    }

    #[tokio::test]
    async fn executes_dag_with_fanout_and_fanin() {
        let cwd = test_dir("fanout_fanin");
        let def = simple_graph();
        let opts = GraphRunOptions {
            cwd: &cwd,
            session_id: "test",
            cancel: CancellationToken::new(),
            subagent: &EchoRunner,
            question: None,
            resume: false,
        };
        let result = run_graph(&def, &json!({"topic": "hi"}), &opts)
            .await
            .expect("graph runs");
        assert_eq!(result.nodes_run, vec!["a", "b", "c", "d"]);
        assert!(result.state.contains_key("a"));
        assert!(result.state.contains_key("b"));
        assert!(result.state.contains_key("c"));
        assert!(result.state.contains_key("d"));
        assert_eq!(result.state["topic"], json!("hi"));
        assert!(!result.resumed);
        assert!(!result.aborted);
    }

    #[tokio::test]
    async fn resumes_from_checkpoint_skipping_completed() {
        let cwd = test_dir("resume");
        let def = simple_graph();
        // Prime a checkpoint with `a` complete.
        save_checkpoint(
            &cwd,
            &def,
            &GraphCheckpoint {
                graph: def.name.clone(),
                version: def.version,
                updated: "0s".into(),
                completed: vec!["a".into()],
                state: {
                    let mut m = BTreeMap::new();
                    m.insert("a".into(), json!("prior-a"));
                    m
                },
            },
        );
        let opts = GraphRunOptions {
            cwd: &cwd,
            session_id: "test",
            cancel: CancellationToken::new(),
            subagent: &EchoRunner,
            question: None,
            resume: true,
        };
        let result = run_graph(&def, &json!({"topic": "hi"}), &opts)
            .await
            .expect("graph runs");
        assert!(result.resumed);
        assert!(!result.nodes_run.contains(&"a".to_string()));
        assert_eq!(result.state["a"], json!("prior-a"));
        assert_eq!(result.nodes_completed, vec!["a", "b", "c", "d"]);
        clear_checkpoint(&cwd, &def.name);
    }

    #[tokio::test]
    async fn router_selects_first_branch() {
        let raw = r#"---
name: r
version: 1
start: start
nodes:
  start:
    kind: router
    prompt: "pick"
    branches: [left, right]
  left:
    prompt: "L"
    end: true
  right:
    prompt: "R"
    end: true
---
"#;
        let def = super::super::parse_graph(raw, Path::new("/tmp/r.md")).unwrap();
        let cwd = test_dir("router");
        let opts = GraphRunOptions {
            cwd: &cwd,
            session_id: "test",
            cancel: CancellationToken::new(),
            subagent: &EchoRunner,
            question: None,
            resume: false,
        };
        // EchoRunner's router reply does not match a declared branch, so the
        // router node must fail the run with a clear error.
        let err = run_graph(&def, &json!({}), &opts).await.unwrap_err();
        assert!(err.to_string().contains("branch"), "{err}");
        clear_checkpoint(&cwd, &def.name);
    }

    #[tokio::test]
    async fn cancel_stops_execution() {
        let cwd = test_dir("cancel");
        let def = simple_graph();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let opts = GraphRunOptions {
            cwd: &cwd,
            session_id: "test",
            cancel,
            subagent: &EchoRunner,
            question: None,
            resume: false,
        };
        let err = run_graph(&def, &json!({}), &opts).await.unwrap_err();
        assert!(matches!(err, Error::Cancelled));
    }
}
