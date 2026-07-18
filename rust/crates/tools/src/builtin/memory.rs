//! Memory tool — search/create/update/forget facts and manage beads.
//! Part of the Mneme three-layer cross-session memory system.

use crate::tool::{Tool, ToolCtx, ToolResult};
use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde_json::{json, Value};
use std::path::Path;
use tokio_util::sync::CancellationToken;

const PROMPT: &str = "Memory tool for the Mneme cross-session memory system. Use this to search, save, update, and forget facts, and to manage task beads.\n\nActions:\n- `search`: search facts by query string. Returns ranked results.\n- `save`: create or update a fact. Requires name, title, content, type, importance, confidence, tags.\n- `forget`: mark a fact as superseded. Requires name and superseded_by reason.\n- `beads`: list active (non-done) beads.\n- `bead_save`: create or update a bead. Requires title, status, priority, content.\n- `bead_done`: mark a bead as done. Requires id.";

pub struct MemoryTool;

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        "Memory"
    }
    fn prompt(&self) -> &'static str {
        PROMPT
    }
    fn description(&self) -> &'static str {
        "Search, save, update or forget facts in the cross-session memory system."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action: search, save, forget, beads, bead_save, bead_done",
                    "enum": ["search", "save", "forget", "beads", "bead_save", "bead_done"]
                },
                "query": { "type": "string", "description": "Search query (for action: search)" },
                "name": { "type": "string", "description": "Fact name/slug (for actions: save, forget)" },
                "title": { "type": "string", "description": "Fact or bead title" },
                "content": { "type": "string", "description": "Fact or bead body content" },
                "type": {
                    "type": "string",
                    "description": "Fact type: preference, convention, decision, architecture, bug, general",
                    "enum": ["preference", "convention", "decision", "architecture", "bug", "general"]
                },
                "importance": { "type": "number", "description": "Fact importance 0.0-1.0" },
                "confidence": { "type": "number", "description": "Fact confidence 0.0-1.0" },
                "tags": { "type": "array", "items": { "type": "string" }, "description": "Tags" },
                "status": {
                    "type": "string",
                    "description": "Bead status",
                    "enum": ["todo", "in_progress", "blocked", "done"]
                },
                "priority": { "type": "integer", "description": "Bead priority 0-10" },
                "id": { "type": "string", "description": "Bead UUID (for bead_done)" },
                "superseded_by": { "type": "string", "description": "Name of fact that replaces this one (for action: forget)" }
            },
            "required": ["action"]
        })
    }
    fn is_read_only(&self, input: &Value) -> bool {
        matches!(
            input["action"].as_str(),
            Some("search") | Some("beads")
        )
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool { true }
    async fn check_permissions(&self, _: &Value, _: &ToolCtx<'_>) -> PermissionResult {
        PermissionResult::allow()
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        _cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let action = input["action"]
            .as_str()
            .ok_or_else(|| Error::Tool {
                tool: "Memory".into(),
                message: "missing required 'action' field".into(),
            })?;

        match action {
            "search" => search_facts(ctx.cwd, &input),
            "save" => save_fact_impl(ctx.cwd, &input),
            "forget" => forget_fact(ctx.cwd, &input),
            "beads" => list_beads(ctx.cwd),
            "bead_save" => save_bead_impl(ctx.cwd, &input),
            "bead_done" => mark_bead_done(ctx.cwd, &input),
            _ => Err(Error::Tool {
                tool: "Memory".into(),
                message: format!("unknown action: {action}"),
            }),
        }
    }
}

fn search_facts(cwd: &Path, input: &Value) -> Result<ToolResult> {
    let query = input["query"].as_str().unwrap_or("");
    let limit = input["limit"].as_u64().unwrap_or(10).min(20) as usize;
    let facts = crate::memory::load_facts(cwd);
    let results = crate::memory::search_facts(&facts, query, limit);
    if results.is_empty() {
        return Ok(ToolResult::ok("No matching facts found."));
    }
    let mut out = String::new();
    for f in &results {
        out.push_str(&format!(
            "## {name} ({t:?}, importance: {imp})\n{body}\n\n",
            name = f.name,
            t = f.fact_type,
            imp = f.importance,
            body = f.content,
        ));
    }
    Ok(ToolResult::ok(out))
}

fn save_fact_impl(cwd: &Path, input: &Value) -> Result<ToolResult> {
    let name = require_str(input, "name")?.to_string();
    let fact = crate::memory::Fact {
        name: name.clone(),
        title: input["title"].as_str().unwrap_or(&name).to_string(),
        content: input["content"].as_str().unwrap_or("").to_string(),
        fact_type: parse_fact_type(input["type"].as_str().unwrap_or("general")),
        importance: input["importance"].as_f64().unwrap_or(0.5).clamp(0.0, 1.0),
        confidence: input["confidence"].as_f64().unwrap_or(0.8).clamp(0.0, 1.0),
        created: chrono_now(),
        updated: chrono_now(),
        sources: vec![],
        supersedes: None,
        tags: input["tags"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default(),
    };
    fact.save(cwd).map_err(|e| Error::Tool {
        tool: "Memory".into(),
        message: format!("failed to save fact: {e}"),
    })?;
    Ok(ToolResult::ok(format!("Fact `{name}` saved.")))
}

fn forget_fact(cwd: &Path, input: &Value) -> Result<ToolResult> {
    let name = require_str(input, "name")?;
    let by = input["superseded_by"].as_str().unwrap_or("manual");
    crate::memory::supersede_fact(cwd, name, by).map_err(|e| Error::Tool {
        tool: "Memory".into(),
        message: format!("failed to supersede fact: {e}"),
    })?;
    Ok(ToolResult::ok(format!("Fact `{name}` superseded by {by}.")))
}

fn list_beads(cwd: &Path) -> Result<ToolResult> {
    let beads = crate::memory::load_beads(cwd);
    let active: Vec<&crate::memory::Bead> =
        crate::memory::active_beads(&beads)
            .into_iter()
            .collect();
    if active.is_empty() {
        return Ok(ToolResult::ok("No active beads."));
    }
    let mut out = String::new();
    for b in &active {
        let icon = match b.status {
            crate::memory::BeadStatus::Todo => "○",
            crate::memory::BeadStatus::InProgress => "◌",
            crate::memory::BeadStatus::Blocked => "⊘",
            crate::memory::BeadStatus::Done => "✓",
        };
        out.push_str(&format!(
            "{icon} **{title}** [priority {prio}]\n  id: {id}\n  {ctx}\n\n",
            title = b.title,
            prio = b.priority,
            id = b.id,
            ctx = &b.content.chars().take(200).collect::<String>(),
        ));
    }
    Ok(ToolResult::ok(out))
}

fn save_bead_impl(cwd: &Path, input: &Value) -> Result<ToolResult> {
    let id = input["id"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let bead = crate::memory::Bead {
        id: id.clone(),
        title: input["title"].as_str().unwrap_or("Untitled").to_string(),
        status: parse_bead_status(input["status"].as_str().unwrap_or("in_progress")),
        priority: input["priority"].as_u64().unwrap_or(5).min(10) as u8,
        created: chrono_now(),
        updated: chrono_now(),
        session: String::new(),
        content: input["content"].as_str().unwrap_or("").to_string(),
    };
    bead.save(cwd).map_err(|e| Error::Tool {
        tool: "Memory".into(),
        message: format!("failed to save bead: {e}"),
    })?;
    Ok(ToolResult::ok(format!("Bead `{}` ({}) saved.", bead.title, bead.id)))
}

fn mark_bead_done(cwd: &Path, input: &Value) -> Result<ToolResult> {
    let id = require_str(input, "id")?;
    let mut beads = crate::memory::load_beads(cwd);
    if let Some(b) = beads.iter_mut().find(|b| b.id == id) {
        b.status = crate::memory::BeadStatus::Done;
        b.updated = chrono_now();
        b.save(cwd).map_err(|e| Error::Tool {
            tool: "Memory".into(),
            message: format!("failed to save bead: {e}"),
        })?;
        Ok(ToolResult::ok(format!("Bead `{id}` marked done.")))
    } else {
        Err(Error::Tool {
            tool: "Memory".into(),
            message: format!("no bead found with id `{id}`"),
        })
    }
}

fn require_str<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key].as_str().ok_or_else(|| Error::Tool {
        tool: "Memory".into(),
        message: format!("missing required field `{key}`"),
    })
}

fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86400;
    let time = secs % 86400;
    let hour = time / 3600;
    let min = (time % 3600) / 60;
    let sec = time % 60;
    let (y, m, d) = days_to_ymd(days as i64);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn parse_fact_type(s: &str) -> crate::memory::FactType {
    use crate::memory::FactType;
    match s {
        "preference" => FactType::Preference,
        "convention" => FactType::Convention,
        "decision" => FactType::Decision,
        "architecture" => FactType::Architecture,
        "bug" => FactType::Bug,
        _ => FactType::General,
    }
}

fn parse_bead_status(s: &str) -> crate::memory::BeadStatus {
    use crate::memory::BeadStatus;
    match s {
        "todo" => BeadStatus::Todo,
        "in_progress" => BeadStatus::InProgress,
        "blocked" => BeadStatus::Blocked,
        "done" => BeadStatus::Done,
        _ => BeadStatus::InProgress,
    }
}
