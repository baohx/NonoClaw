//! Memory tool — search/create/update/forget facts and manage beads.
//! Part of the Mneme three-layer cross-session memory system.

use crate::tool::{Tool, ToolCtx, ToolResult};
use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionResult, Result};
use serde_json::{json, Value};
use std::path::Path;
use tokio_util::sync::CancellationToken;

const PROMPT: &str = "Memory tool for the Mneme cross-session memory system and LLM Wiki knowledge base.\n\nActions:\n- `search`: search facts by query string. Returns ranked results.\n- `save`: create or update a fact. Requires name, title, content, type, importance, confidence, tags.\n- `forget`: mark a fact as superseded. Requires name and superseded_by reason.\n- `beads`: list active (non-done) beads.\n- `bead_save`: create or update a bead. Requires title, status, priority, content.\n- `bead_done`: mark a bead as done. Requires id.\n- `wiki_search`: search wiki pages by query. Returns ranked results with page name, type, summary.\n- `wiki_ingest`: ingest a raw source file from raw/ into the wiki. Reads the source, creates/updates wiki pages. Requires source_path.\n- `wiki_lint`: list stale or orphan wiki pages. No arguments needed.\n- `goal_create`: create a multi-step goal plan. Requires title, steps[], verification.\n- `goal_update`: update goal status or steps. Requires id, optional status, optional steps.\n- `goal_list`: list all goals (active + completed). No arguments needed.";

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
                    "description": "Action: search, save, forget, beads, bead_save, bead_done, wiki_search, wiki_ingest, wiki_lint",
                    "enum": ["search", "save", "forget", "beads", "bead_save", "bead_done", "wiki_search", "wiki_ingest", "wiki_lint", "goal_create", "goal_update", "goal_list"]
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
            Some("search")
                | Some("beads")
                | Some("wiki_search")
                | Some("wiki_lint")
                | Some("goal_list")
        )
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
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
        let action = input["action"].as_str().ok_or_else(|| Error::Tool {
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
            "wiki_search" => wiki_search(ctx.cwd, &input),
            "wiki_ingest" => wiki_ingest(ctx.cwd, &input),
            "wiki_lint" => wiki_lint(ctx.cwd),
            "goal_create" => goal_create(ctx.cwd, &input),
            "goal_update" => goal_update(ctx.cwd, &input),
            "goal_list" => goal_list(ctx.cwd),
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
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
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
        crate::memory::active_beads(&beads).into_iter().collect();
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
            ctx = b.content.chars().take(200).collect::<String>(),
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
    Ok(ToolResult::ok(format!(
        "Bead `{}` ({}) saved.",
        bead.title, bead.id
    )))
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

fn wiki_search(cwd: &Path, input: &Value) -> Result<ToolResult> {
    let query = input["query"].as_str().unwrap_or("");
    let limit = input["limit"].as_u64().unwrap_or(10).min(20) as usize;
    let pages = crate::memory::load_wiki_pages(cwd);
    let results = crate::memory::search_wiki(&pages, query, limit);
    if results.is_empty() {
        return Ok(ToolResult::ok(
            "No matching wiki pages found. The wiki may be empty — try ingesting a source first.",
        ));
    }
    let mut out = String::new();
    for p in &results {
        out.push_str(&format!(
            "## {title} ({t:?}, {conf:?}, domain: {dom})\n{summary}\n\n",
            title = p.title,
            t = p.page_type,
            conf = p.confidence,
            dom = p.domain,
            summary = p.summary,
        ));
    }
    Ok(ToolResult::ok(out))
}

fn wiki_ingest(_cwd: &Path, input: &Value) -> Result<ToolResult> {
    let source_path = require_str(input, "source_path")?;
    let path = Path::new(source_path);
    if !path.exists() {
        return Err(Error::Tool {
            tool: "Memory".into(),
            message: format!("source file not found: {source_path}. Place source files in .nonoclaw/raw/ and provide the path relative to raw/."),
        });
    }
    // The actual ingestion (reading source, creating wiki pages) is done
    // by the model using Read + Write tools.  We just validate and guide.
    let raw = std::fs::read_to_string(path).map_err(|e| Error::Tool {
        tool: "Memory".into(),
        message: format!("failed to read source: {e}"),
    })?;
    let preview: String = raw.chars().take(2000).collect();
    let mut out = String::from("# Wiki Ingest Guide\n\n");
    out.push_str("Read this source and create/update wiki pages in `.nonoclaw/wiki/`.\n\n");
    out.push_str("## Source content (first 2000 chars)\n\n");
    out.push_str(&preview);
    out.push_str("\n\n## Instructions\n\n");
    out.push_str("1. Read `wiki/WIKI.md` for the schema and writing conventions\n");
    out.push_str("2. Read `wiki/index.md` for the current wiki catalog\n");
    out.push_str(
        "3. Create or update pages in `wiki/concepts/`, `wiki/entities/`, `wiki/sources/`, etc.\n",
    );
    out.push_str("4. Update `wiki/index.md` with new page entries\n");
    out.push_str("5. Append an entry to `wiki/log.md`\n");
    out.push_str("6. Use `[[wikilinks]]` to cross-reference pages\n");
    out.push_str("\nEach page must have YAML frontmatter: title, type, domain, summary, confidence, tags, sources.\n");
    Ok(ToolResult::ok(out))
}

fn wiki_lint(cwd: &Path) -> Result<ToolResult> {
    let pages = crate::memory::load_wiki_pages(cwd);
    if pages.is_empty() {
        return Ok(ToolResult::ok("Wiki is empty — nothing to lint."));
    }
    let mut issues = Vec::new();

    // Check for pages with no sources (except source-type pages).
    for p in &pages {
        if p.page_type != crate::memory::WikiType::Source && p.sources.is_empty() {
            issues.push(format!("No sources: {} ({:?})", p.title, p.page_type));
        }
    }

    // Check for untagged pages.
    for p in &pages {
        if p.tags.is_empty() {
            issues.push(format!("No tags: {}", p.title));
        }
    }

    // Check for low-confidence claims.
    for p in &pages {
        if p.confidence == crate::memory::Confidence::Low {
            issues.push(format!("Low confidence: {}", p.title));
        }
    }

    let total = pages.len();
    if issues.is_empty() {
        Ok(ToolResult::ok(format!(
            "Wiki lint passed. {total} pages, 0 issues."
        )))
    } else {
        let mut out = format!("Wiki lint: {total} pages, {} issues:\n\n", issues.len());
        for (i, issue) in issues.iter().enumerate().take(20) {
            out.push_str(&format!("{}. {issue}\n", i + 1));
        }
        Ok(ToolResult::ok(out))
    }
}

fn goal_create(cwd: &Path, input: &Value) -> Result<ToolResult> {
    let id = uuid::Uuid::new_v4().to_string();
    let title = require_str(input, "title")?.to_string();
    let goal = crate::memory::Goal {
        id: id.clone(),
        title,
        status: crate::memory::GoalStatus::InProgress,
        steps: input["steps"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        verification: input["verification"].as_str().unwrap_or("").to_string(),
        created: chrono_now(),
        updated: chrono_now(),
        content: input["content"].as_str().unwrap_or("").to_string(),
    };
    goal.save(cwd).map_err(|e| Error::Tool {
        tool: "Memory".into(),
        message: format!("failed to save goal: {e}"),
    })?;
    Ok(ToolResult::ok(format!(
        "Goal `{}` created (id: {id}).",
        goal.title
    )))
}

fn goal_update(cwd: &Path, input: &Value) -> Result<ToolResult> {
    let id = require_str(input, "id")?;
    let mut goals = crate::memory::load_goals(cwd);
    if let Some(g) = goals.iter_mut().find(|g| g.id == id) {
        if let Some(s) = input["status"].as_str() {
            g.status = match s {
                "completed" => crate::memory::GoalStatus::Completed,
                "blocked" => crate::memory::GoalStatus::Blocked,
                "abandoned" => crate::memory::GoalStatus::Abandoned,
                _ => crate::memory::GoalStatus::InProgress,
            };
        }
        if let Some(steps) = input["steps"].as_array() {
            g.steps = steps
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
        }
        g.updated = chrono_now();
        g.save(cwd).map_err(|e| Error::Tool {
            tool: "Memory".into(),
            message: format!("failed to save goal: {e}"),
        })?;
        Ok(ToolResult::ok(format!("Goal `{id}` updated.")))
    } else {
        Err(Error::Tool {
            tool: "Memory".into(),
            message: format!("no goal with id `{id}`"),
        })
    }
}

fn goal_list(cwd: &Path) -> Result<ToolResult> {
    let goals = crate::memory::load_goals(cwd);
    if goals.is_empty() {
        return Ok(ToolResult::ok("No goals."));
    }
    let mut out = String::new();
    for g in &goals {
        let icon = match g.status {
            crate::memory::GoalStatus::Completed => "✅",
            crate::memory::GoalStatus::InProgress => "🔄",
            crate::memory::GoalStatus::Blocked => "🚫",
            crate::memory::GoalStatus::Abandoned => "❌",
        };
        out.push_str(&format!(
            "{icon} **{title}** [{status:?}]\n",
            title = g.title,
            status = g.status
        ));
        out.push_str(&format!("  id: {}\n", g.id));
        let done = g.steps.iter().filter(|s| s.starts_with("[x]")).count();
        out.push_str(&format!(
            "  steps: {done}/{total}\n\n",
            total = g.steps.len()
        ));
    }
    Ok(ToolResult::ok(out))
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
