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

/// Write `.nonoclaw/plans/<graph>-<timestamp>.md`: a durable planning
/// artifact with the AutoGenesis plan.md five-section shape (objective,
/// steps, execution history, results, open items). Soft-fail.
fn write_plan_artifact(
    cwd: &Path,
    def: &GraphDefinition,
    completed: &[String],
    nodes_run: &[String],
    aborted: bool,
    state: &BTreeMap<String, Value>,
) {
    let dir = cwd.join(".nonoclaw/plans");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut md = String::new();
    md.push_str(&format!("# Plan: graph `{}`\n\n", def.name));
    md.push_str("## Objective\n\nDeclared nodes of this run.\n\n## Steps\n\n");
    for id in def.nodes.keys() {
        let mark = if completed.contains(id) {
            "[x]"
        } else if aborted {
            "[ ]"
        } else {
            "[ ]"
        };
        md.push_str(&format!("- {mark} {id}\n"));
    }
    md.push_str("\n## Execution History\n\n");
    for id in nodes_run {
        md.push_str(&format!("- {id}\n"));
    }
    md.push_str("\n## Results\n\n");
    for id in nodes_run {
        if let Some(out) = state.get(id).and_then(Value::as_str) {
            let snippet: String = out.chars().take(400).collect();
            md.push_str(&format!("### {id}\n{snippet}\n\n"));
        }
    }
    let open: Vec<&String> = def
        .nodes
        .keys()
        .filter(|id| !completed.contains(id))
        .collect();
    md.push_str("## Open Items\n\n");
    if open.is_empty() {
        md.push_str("(none — all nodes completed)\n");
    } else {
        for id in open {
            md.push_str(&format!(
                "- {id} not completed{}\n",
                if aborted {
                    " (aborted by human gate)"
                } else {
                    ""
                }
            ));
        }
    }
    let _ = std::fs::write(dir.join(format!("{}-{ts}.md", def.name)), md);
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
        let Some(node) = def.nodes.get(id) else {
            continue;
        };
        if node.kind == NodeKind::Router
            || (node.kind == NodeKind::Score && !node.branches.is_empty())
        {
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
            state.insert(node_id.clone(), Value::String(output.clone()));
            save_checkpoint(
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
            // Router / Score: record the chosen branch so a resume can replay
            // the dynamic edge, and activate a not-yet-run branch (Router
            // semantics). A Score node's chosen candidate normally already
            // ran (arena: score follows its branches) — its successor edges
            // already fired, so only the winner is recorded. Annotation-mode
            // score nodes (no branches) skip this entirely.
            let is_dynamic_choice = node.kind == NodeKind::Router
                || (node.kind == NodeKind::Score && !node.branches.is_empty());
            if is_dynamic_choice {
                if let Some(branch) = next_nodes.first() {
                    if !done.contains(branch) {
                        reachable.insert(branch.clone());
                    }
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

    // --- Plan artifact (AutoGenesis plan.md borrow) ---
    // Persist a human/agent-readable planning artifact so global intent
    // survives session resume / compaction and is inspectable on disk.
    write_plan_artifact(opts.cwd, def, &completed, &nodes_run, aborted, &state);

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
                    prompt: format!("{prompt}\n\nProceed with the next step or abort the graph?",),
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
                Some(reply) => Ok((format!("approved: {reply}"), node.next.ids(), false)),
            }
        }
        NodeKind::Score => {
            let prompt = render_prompt(&node.prompt, state);
            let branches = node.branches.clone();
            if branches.is_empty() {
                // No candidates declared: the score node only annotates —
                // jev rates the state, output is the verdict text.
                let (text, _) = score_with_jev(&prompt, &[], state, node_id, def, opts).await?;
                return Ok((text, node.next.ids(), false));
            }
            let (output, chosen) = score_with_jev(&prompt, &branches, state, node_id, def, opts)
                .await?;
            Ok((output, vec![chosen], false))
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

/// Score the candidate node outputs and pick the best one.
///
/// Primary path: a Jev `Choice` question whose options are the candidate
/// node outputs (`branches` = candidate ids; each output is read from state).
/// Fallback path (fail-open): if jev is unavailable or its answer is not
/// trusted, a Router-style subagent judges the candidates, so the graph
/// still converges. Returns `(verdict_text, chosen_branch_id)`; when
/// `branches` is empty only the verdict text is meaningful.
async fn score_with_jev(
    rendered_prompt: &str,
    branches: &[String],
    state: &BTreeMap<String, Value>,
    node_id: &str,
    def: &GraphDefinition,
    opts: &GraphRunOptions<'_>,
) -> Result<(String, String)> {
    // Candidate outputs, joined for the state payload.
    let mut candidates = Vec::with_capacity(branches.len());
    for id in branches {
        let text = state
            .get(id)
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        candidates.push(serde_json::json!({ "id": id, "output": truncate(&text, 4000) }));
    }

    if let Some(jev) = crate::jev_reward::global_jev() {
        let state_payload = serde_json::json!({
            "task_prompt": rendered_prompt,
            "candidates": candidates,
            "context": "Scoring gate in an agent graph: pick the best candidate output for the task.",
        })
        .to_string();
        let question = nonoclaw_api::jev::JevQuestion::choice(
            "Pick the candidate output that best completes the task prompt: correct, complete, and faithful to the instructions.",
            &branches
                .iter()
                .map(|b| (b.as_str(), ""))
                .collect::<Vec<_>>(),
        );
        match jev
            .ask_raw(&state_payload, &[("best_candidate".to_string(), question)])
            .await
        {
            Ok(answers) => {
                if let Some(answer) = answers.get("best_candidate") {
                    let chosen = answer
                        .choice
                        .as_deref()
                        .and_then(|c| normalize_branch(c, branches));
                    if let Some(id) = chosen {
                        let confidence = answer.confidence;
                        let probs = answer
                            .probabilities
                            .iter()
                            .map(|(k, v)| format!("{}={:.2}", k, v))
                            .collect::<Vec<_>>()
                            .join(" ");
                        let output = format!(
                            "jev-scored: winner={id} confidence={:?} probs=[{probs}]",
                            confidence.unwrap_or(0.0)
                        );
                        tracing::info!(node = %node_id, winner = %id, "score node jev verdict");
                        return Ok((output, id));
                    }
                }
                tracing::warn!(node = %node_id, "score node jev answer unusable; falling back to subagent judgment");
            }
            Err(e) => {
                tracing::warn!(node = %node_id, error = %e, "score node jev failed; falling back to subagent judgment");
            }
        }
    }

    // Fail-open fallback: Router-style subagent judgment over the candidates.
    let node = &def.nodes[node_id];
    if branches.is_empty() {
        // Annotation mode: no candidates to pick between — the judge's free
        // text IS the verdict.
        let answer = run_node_subagent(node, rendered_prompt, node_id, opts).await?;
        return Ok((format!("judge-scored: {answer} (jev unavailable)"), String::new()));
    }
    let mut decision_prompt = format!(
        "{rendered_prompt}\n\nYou are the scoring judge. Below are candidate outputs; \
         reply with EXACTLY one candidate id on the last line.\n"
    );
    for id in branches {
        let text = state
            .get(id)
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        decision_prompt.push_str(&format!("\n--- candidate {id} ---\n{}\n", truncate(&text, 2000)));
    }
    let answer = run_node_subagent(node, &decision_prompt, node_id, opts).await?;
    let chosen = normalize_branch(
        answer.lines().rev().find_map(|l| {
            let t = l.trim();
            (!t.is_empty()).then_some(t)
        }).unwrap_or_default(),
        branches,
    )
    .ok_or_else(|| Error::Tool {
        tool: "graph".into(),
        message: format!(
            "score node '{node_id}': judge reply did not name any candidate id"
        ),
    })?;
    Ok((format!("judge-scored: winner={chosen} (jev unavailable)"), chosen))
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
        let dir = std::env::temp_dir().join(format!("nc-graph-test-{name}-{}", std::process::id()));
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
        assert_eq!(
            normalize_branch("Rewrite.", &branches),
            Some("rewrite".into())
        );
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

    /// Subagent runner whose reply names a fixed branch id — for score-node
    /// tests exercising the fail-open (jev unavailable) path.
    struct JudgeRunner {
        pick: &'static str,
    }
    impl SubagentRunner for JudgeRunner {
        fn run_subagent<'a>(
            &'a self,
            request: SubagentRequest,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
            Box::pin(async move {
                // Agent nodes echo; the score node's judge reply names the pick.
                if request.description.starts_with("graph:") {
                    Ok(self.pick.to_string())
                } else {
                    Ok(format!("ran:{}", request.description))
                }
            })
        }
    }

    #[tokio::test]
    async fn score_node_falls_back_to_judge_and_activates_winner_branch() {
        let raw = r#"---
name: s
version: 1
start: a
nodes:
  a:
    prompt: "implement X"
    next: [b, c]
  b:
    prompt: "candidate b"
    next: score
  c:
    prompt: "candidate c"
    next: score
  score:
    kind: score
    prompt: "pick the best implementation of X"
    branches: [b, c]
    end: true
---
"#;
        let def = super::super::parse_graph(raw, Path::new("/tmp/s.md")).unwrap();
        let cwd = test_dir("score_fallback");
        // No global jev client in tests → fail-open subagent judgment.
        let opts = GraphRunOptions {
            cwd: &cwd,
            session_id: "test",
            cancel: CancellationToken::new(),
            subagent: &JudgeRunner { pick: "c" },
            question: None,
            resume: false,
        };
        let result = run_graph(&def, &json!({}), &opts)
            .await
            .expect("graph runs");
        // Score node completed after both candidates; verdict recorded in state.
        let verdict = result.state["score"]
            .as_str()
            .expect("score output is a string");
        assert!(verdict.contains("winner=c"), "verdict: {verdict}");
        assert!(result.nodes_completed.contains(&"score".to_string()));
        clear_checkpoint(&cwd, &def.name);
    }

    #[tokio::test]
    async fn score_node_records_winner_and_runs_successor() {
        // Arena shape: 3 contenders → score (jev/judge) → report successor.
        // Verifies the dynamic-choice bookkeeping: the winner is recorded
        // under `_router:score` and the score node's own `next` still runs.
        let raw = r#"---
name: sr
start: fan
nodes:
  fan:
    prompt: "task"
    next: [x, y]
  x:
    prompt: "candidate x"
    next: score
  y:
    prompt: "candidate y"
    next: score
  score:
    kind: score
    prompt: "pick the best candidate"
    branches: [x, y]
    next: report
  report:
    prompt: "report: {score}"
    end: true
---
"#;
        let def = super::super::parse_graph(raw, Path::new("/tmp/sr.md")).unwrap();
        let cwd = test_dir("score_successor");
        let opts = GraphRunOptions {
            cwd: &cwd,
            session_id: "test",
            cancel: CancellationToken::new(),
            subagent: &JudgeRunner { pick: "y" },
            question: None,
            resume: false,
        };
        let result = run_graph(&def, &json!({}), &opts)
            .await
            .expect("graph runs");
        // Winner recorded as a replayable dynamic edge.
        assert_eq!(result.state["_router:score"], json!("y"));
        // The score node's own successor ran to completion.
        assert!(result.nodes_completed.contains(&"report".to_string()));
        clear_checkpoint(&cwd, &def.name);
    }

    #[tokio::test]
    async fn score_node_annotation_mode_without_branches() {
        let raw = r#"---
name: sa
version: 1
start: a
nodes:
  a:
    prompt: "do work"
    next: judge
  judge:
    kind: score
    prompt: "rate the work in {a}"
    end: true
---
"#;
        let def = super::super::parse_graph(raw, Path::new("/tmp/sa.md")).unwrap();
        let cwd = test_dir("score_annotate");
        let opts = GraphRunOptions {
            cwd: &cwd,
            session_id: "test",
            cancel: CancellationToken::new(),
            subagent: &JudgeRunner { pick: "unused" },
            question: None,
            resume: false,
        };
        let result = run_graph(&def, &json!({}), &opts)
            .await
            .expect("graph runs");
        // Annotation mode: verdict text is produced and the graph ends.
        assert!(result.state.contains_key("judge"));
        assert!(!result.aborted);
        clear_checkpoint(&cwd, &def.name);
    }
}
