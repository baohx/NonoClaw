//! Declarative agent graphs — `.nonoclaw/graphs/<name>.md`.
//!
//! A graph is a markdown file whose YAML frontmatter declares a set of
//! **nodes** (each a subagent run, an LLM router, or a human gate) connected by
//! `next` edges. The executor (see [`crate::graph::executor`]) walks the
//! resulting dataflow DAG: nodes whose predecessors have all completed run
//! (fan-out in parallel, fan-in after every predecessor), router nodes pick a
//! single branch, gate nodes pause for human approval, and execution stops at
//! an `end` node or after `max_steps`.
//!
//! # File format
//!
//! ```markdown
//! ---
//! name: research
//! description: 调研 → 分析 → 评审 → 报告
//! version: 1
//! start: gather
//! max_steps: 20
//! state:
//!   topic: ""                 # input parameter
//! nodes:
//!   gather:
//!     profile: researcher     # optional subagent profile
//!     prompt: "调研 {topic} 并返回结构化要点。"
//!     next: analyze           # string or list (parallel fan-out)
//!   analyze:
//!     prompt: "基于 {gather} 分析，给出结论。"
//!     next: decide
//!   decide:
//!     kind: router            # LLM picks one of `branches`
//!     prompt: "根据进展选择下一步。"
//!     branches: [draft, rewrite]
//!   draft:
//!     prompt: "起草方案。"
//!     next: review
//!   rewrite:
//!     prompt: "重写方案。"
//!     next: review
//!   review:
//!     kind: gate              # human approval
//!     prompt: "方案需要人工确认。"
//!     next: report
//!   report:
//!     prompt: "汇总最终报告。"
//!     end: true
//! ---
//! ```
//!
//! Prompt templates support `{state.field}` (a top-level state value or input
//! argument) and `{node_id}` (the text output of a completed node, which is
//! stored in state under its id). Node outputs always land in `state[node.id]`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nonoclaw_core::{Error, Result};
use serde::{Deserialize, Serialize};

pub mod executor;

/// How a node executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Run a subagent with the rendered prompt (default).
    #[default]
    Agent,
    /// Run a subagent that must answer with exactly one of `branches`; only
    /// that branch's successors are activated.
    Router,
    /// Pause for human approval via the interactive question resolver.
    Gate,
}

/// A single node in a graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    /// Node id (key in the `nodes` map).
    #[serde(skip)]
    pub id: String,
    #[serde(default)]
    pub kind: NodeKind,
    /// Optional `.nonoclaw/agents/<profile>.md` for agent/router nodes.
    #[serde(default)]
    pub profile: Option<String>,
    /// Prompt template with `{var}` references (state fields / node outputs).
    #[serde(default)]
    pub prompt: String,
    /// Router branches: node ids the router may choose between.
    #[serde(default)]
    pub branches: Vec<String>,
    /// Unconditional successor(s): a single id or a list (parallel fan-out).
    /// Mutually exclusive with `kind: router` (which uses `branches`).
    #[serde(default)]
    pub next: Next,
    /// When true, finishing this node completes the graph.
    #[serde(default)]
    pub end: bool,
    /// Human-readable node purpose (optional).
    #[serde(default)]
    pub description: String,
}

/// A node's successors. YAML accepts either a plain string or a list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Next {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl Next {
    /// All successor node ids.
    pub fn ids(&self) -> Vec<String> {
        match self {
            Next::None => vec![],
            Next::One(id) => vec![id.clone()],
            Next::Many(ids) => ids.clone(),
        }
    }
}

/// A graph definition parsed from `.nonoclaw/graphs/<name>.md`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphDefinition {
    /// Graph name (frontmatter `name` or filename stem).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: u32,
    /// Starting node id. Defaults to the first declared node.
    #[serde(default)]
    pub start: Option<String>,
    /// Input state fields with defaults; invocation arguments override these.
    #[serde(default)]
    pub state: BTreeMap<String, serde_json::Value>,
    /// Node map (order preserved; first node is the default start).
    #[serde(default)]
    pub nodes: BTreeMap<String, GraphNode>,
    /// Hard cap on executed node steps (loop protection). Default 20.
    #[serde(default)]
    pub max_steps: Option<usize>,
    /// Markdown body after the frontmatter (graph-level instructions).
    #[serde(skip)]
    pub body: String,
    /// Source file path (for diagnostics / checkpoint keys).
    #[serde(skip)]
    pub source: PathBuf,
}

impl GraphDefinition {
    /// Default maximum executed steps when `max_steps` is absent.
    pub const DEFAULT_MAX_STEPS: usize = 20;
    /// Absolute cap — even explicit `max_steps` cannot exceed this.
    pub const HARD_MAX_STEPS: usize = 200;

    /// Starting node id (explicit `start` or the first declared node).
    pub fn start_node(&self) -> Option<&str> {
        if let Some(start) = self.start.as_deref() {
            return self.nodes.get(start).map(|_| start);
        }
        self.nodes.keys().next().map(|s| s.as_str())
    }

    pub fn max_steps(&self) -> usize {
        self.max_steps
            .unwrap_or(Self::DEFAULT_MAX_STEPS)
            .min(Self::HARD_MAX_STEPS)
    }

    /// Structural validation: start exists, references resolve, no node
    /// declares both `next` and `branches`/router semantics, and the graph is
    /// not trivially empty.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.nodes.is_empty() {
            return Err(format!("graph `{}` declares no nodes", self.name));
        }
        let start = self.start_node().ok_or_else(|| {
            format!("graph `{}` has an invalid or missing start node", self.name)
        })?;
        let mut pending: Vec<String> = vec![start.to_string()];
        // Walk successors to (a) resolve every reference and (b) detect
        // references that dangle outside the declared node set.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let node = self.nodes.get(&id).ok_or_else(|| {
                format!("graph `{}` references unknown node `{id}`", self.name)
            })?;
            if node.kind == NodeKind::Router {
                if node.next != Next::None {
                    return Err(format!(
                        "graph `{}` node `{id}` is a router: use `branches`, not `next`",
                        self.name
                    ));
                }
                if node.branches.is_empty() {
                    return Err(format!(
                        "graph `{}` router node `{id}` declares no branches",
                        self.name
                    ));
                }
                for branch in &node.branches {
                    if !self.nodes.contains_key(branch) {
                        return Err(format!(
                            "graph `{}` router node `{id}` branch `{branch}` is unknown",
                            self.name
                        ));
                    }
                    pending.push(branch.clone());
                }
            } else {
                for succ in node.next.ids() {
                    if !self.nodes.contains_key(&succ) {
                        return Err(format!(
                            "graph `{}` node `{id}` references unknown successor `{succ}`",
                            self.name
                        ));
                    }
                    pending.push(succ);
                }
            }
        }
        Ok(())
    }
}

/// Discover + parse a graph by safe name from `<cwd>/.nonoclaw/graphs/`.
/// Mirrors the strictness of [`crate::agents::load_profile_checked`].
pub fn load_graph_checked(cwd: &Path, name: &str) -> Result<GraphDefinition> {
    validate_graph_name(name)?;
    let path = cwd.join(".nonoclaw/graphs").join(format!("{name}.md"));
    let raw = std::fs::read_to_string(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Error::Config(format!(
                "agent graph `{name}` was not found (expected {})",
                path.display()
            ))
        } else {
            Error::Config(format!(
                "failed to read agent graph `{name}`: {error}"
            ))
        }
    })?;
    parse_graph(&raw, &path)
}

fn validate_graph_name(name: &str) -> Result<()> {
    let safe = !name.is_empty()
        && name.chars().count() <= 128
        && !name.starts_with('.')
        && !name.contains("..")
        && !name.contains(['/', '\\'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if safe {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "invalid agent graph name `{name}`: expected a safe non-hidden basename"
        )))
    }
}

/// Parse a graph file: YAML frontmatter + optional markdown body.
pub fn parse_graph(raw: &str, source: &Path) -> Result<GraphDefinition> {
    let fm_text = extract_frontmatter(raw).ok_or_else(|| {
        Error::Config(format!(
            "agent graph `{}` must contain YAML frontmatter",
            source.display()
        ))
    })?;
    let body = strip_frontmatter_text(raw);
    let mut def: GraphDefinition = serde_yaml::from_str(&fm_text).map_err(|error| {
        Error::Config(format!(
            "failed to parse agent graph `{}` frontmatter: {error}",
            source.display()
        ))
    })?;
    if def.name.trim().is_empty() {
        def.name = source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed")
            .to_string();
    }
    // Stamp node ids from their map keys.
    for (id, node) in def.nodes.iter_mut() {
        node.id = id.clone();
    }
    // When `start` is omitted, default to the *first declared* node (YAML
    // preserves declaration order; a BTreeMap does not, so read it here).
    if def.start.is_none() {
        if let Ok(serde_yaml::Value::Mapping(m)) =
            serde_yaml::from_str::<serde_yaml::Value>(&fm_text)
        {
            if let Some(serde_yaml::Value::Mapping(nodes)) =
                m.get(serde_yaml::Value::String("nodes".into()))
            {
                if let Some((key, _)) = nodes.iter().next() {
                    if let Some(name) = key.as_str() {
                        if def.nodes.contains_key(name) {
                            def.start = Some(name.to_string());
                        }
                    }
                }
            }
        }
    }
    def.body = body;
    def.source = source.to_path_buf();
    def.validate().map_err(|message| {
        Error::Config(format!("invalid agent graph `{}`: {message}", def.name))
    })?;
    Ok(def)
}

/// List all graph names in `<cwd>/.nonoclaw/graphs/` (best effort).
pub fn list_graphs(cwd: &Path) -> Vec<GraphDefinition> {
    let dir = cwd.join(".nonoclaw/graphs");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut out: Vec<GraphDefinition> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .filter_map(|p| parse_graph(&std::fs::read_to_string(&p).ok()?, &p).ok())
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn extract_frontmatter(raw: &str) -> Option<String> {
    let s = raw.trim_start();
    if !s.starts_with("---") {
        return None;
    }
    let after = &s[3..];
    let end = after.find("\n---")?;
    Some(after[..end].to_string())
}

/// Parse `/graph` command arguments: either a JSON object, or whitespace
/// separated `key=value` pairs (bare tokens become the `input` key).
pub fn parse_args(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return serde_json::Value::Null;
    }
    if trimmed.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
            return value;
        }
    }
    let mut map = serde_json::Map::new();
    for token in trimmed.split_whitespace() {
        if let Some((key, value)) = token.split_once('=') {
            map.insert(
                key.to_string(),
                serde_json::Value::String(value.trim_matches('"').to_string()),
            );
        } else {
            map.insert(
                "input".to_string(),
                serde_json::Value::String(token.to_string()),
            );
        }
    }
    if map.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::Object(map)
    }
}

fn strip_frontmatter_text(raw: &str) -> String {
    let s = raw.trim();
    if !s.starts_with("---") {
        return s.to_string();
    }
    let after = &s[3..];
    if let Some(pos) = after.find("\n---") {
        after[pos + 4..].trim().to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"---
name: demo
description: demo graph
start: a
nodes:
  a:
    prompt: "do {topic}"
    next: [b, c]
  b:
    prompt: "branch b"
    next: d
  c:
    prompt: "branch c"
    next: d
  d:
    kind: router
    prompt: "choose"
    branches: [e, f]
  e:
    prompt: "e"
    end: true
  f:
    prompt: "f"
    end: true
---
body text
"#;

    #[test]
    fn parses_frontmatter_and_body() {
        let def = parse_graph(VALID, Path::new("/tmp/demo.md")).unwrap();
        assert_eq!(def.name, "demo");
        assert_eq!(def.body, "body text");
        assert_eq!(def.nodes.len(), 6);
        assert_eq!(def.nodes["a"].next, Next::Many(vec!["b".into(), "c".into()]));
        assert_eq!(def.nodes["d"].kind, NodeKind::Router);
        assert_eq!(
            def.nodes["d"].branches,
            vec!["e".to_string(), "f".to_string()]
        );
        assert!(def.nodes["e"].end);
        assert_eq!(def.start_node(), Some("a"));
    }

    #[test]
    fn start_defaults_to_first_node() {
        let raw = "---\nnodes:\n  first:\n    prompt: x\n  second:\n    prompt: y\n---\n";
        let def = parse_graph(raw, Path::new("/tmp/x.md")).unwrap();
        assert_eq!(def.name, "x");
        assert_eq!(def.start_node(), Some("first"));
    }

    #[test]
    fn rejects_dangling_successor() {
        let raw = "---\nnodes:\n  a:\n    prompt: x\n    next: ghost\n---\n";
        let err = parse_graph(raw, Path::new("/tmp/x.md")).unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn rejects_router_with_next() {
        let raw = "---\nnodes:\n  a:\n    kind: router\n    prompt: x\n    next: b\n    branches: [b]\n  b:\n    prompt: y\n---\n";
        let err = parse_graph(raw, Path::new("/tmp/x.md")).unwrap_err();
        assert!(err.to_string().contains("branches"), "{err}");
    }

    #[test]
    fn rejects_router_without_branches() {
        let raw = "---\nnodes:\n  a:\n    kind: router\n    prompt: x\n---\n";
        let err = parse_graph(raw, Path::new("/tmp/x.md")).unwrap_err();
        assert!(err.to_string().contains("no branches"), "{err}");
    }

    #[test]
    fn name_falls_back_to_stem() {
        let raw = "---\nnodes:\n  a:\n    prompt: x\n---\n";
        let def = parse_graph(raw, Path::new("/tmp/pipeline.md")).unwrap();
        assert_eq!(def.name, "pipeline");
    }

    #[test]
    fn load_graph_checked_rejects_unsafe_names() {
        let err = load_graph_checked(Path::new("/tmp"), "../escape").unwrap_err();
        assert!(err.to_string().contains("invalid agent graph name"));
    }

    #[test]
    fn load_graph_checked_reads_real_file() {
        let dir = std::env::temp_dir().join(format!(
            "nc-graph-load-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let graphs = dir.join(".nonoclaw/graphs");
        std::fs::create_dir_all(&graphs).unwrap();
        std::fs::write(
            graphs.join("demo.md"),
            "---\nname: demo\nstart: a\nnodes:\n  a:\n    prompt: hi\n    end: true\n---\n",
        )
        .unwrap();
        let def = load_graph_checked(&dir, "demo").expect("graph loads from disk");
        assert_eq!(def.name, "demo");
        assert_eq!(def.start_node(), Some("a"));
        assert!(def.nodes["a"].end);
        let missing = load_graph_checked(&dir, "nope").unwrap_err();
        assert!(missing.to_string().contains("was not found"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_args_handles_json_and_pairs() {
        use serde_json::{json, Value};
        // JSON object passthrough.
        assert_eq!(parse_args("{\"topic\":\"AI\"}"), json!({"topic": "AI"}));
        // key=value pairs.
        assert_eq!(parse_args("topic=AI depth=2"), json!({"topic": "AI", "depth": "2"}));
        // Bare token becomes `input`.
        assert_eq!(parse_args("hello"), json!({"input": "hello"}));
        // Empty → null.
        assert_eq!(parse_args("   "), Value::Null);
    }
}
