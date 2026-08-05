//! System-prompt assembly. The full Claude Code system prompt is built from many
//! fragments across `src/` and is not present verbatim in this extraction; this
//! module assembles a faithful *functional* equivalent: identity + environment +
//! tool guidance + NONOCLAW.md + memory.

use std::sync::{Arc, RwLock};

use nonoclaw_api::SystemBlock;
use nonoclaw_core::CacheControl;

use crate::context::{SystemContext, UserContext};
use crate::skills::SkillsManager;

/// Which sections of the system prompt to include. `Full` reproduces the
/// pre-refactor `BASE` prompt byte-for-byte (verified by test). `Minimal`
/// keeps only identity + safety + task-completion, dropping ~60% of the
/// prompt for cost-sensitive or compact models. `Custom` lets callers pick
/// an explicit set of section names (see `SystemPromptSections::NAMES`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptProfile {
    Full,
    Minimal,
    Custom(std::collections::HashSet<String>),
}

impl Default for PromptProfile {
    fn default() -> Self {
        Self::Full
    }
}

impl PromptProfile {
    /// Parse from a settings string ("full" | "minimal"). Unknown values
    /// fall back to `Full` and log nothing — settings validation happens
    /// upstream in `settings.rs`.
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "minimal" => Self::Minimal,
            _ => Self::Full,
        }
    }

    fn includes(&self, section: &str) -> bool {
        match self {
            Self::Full => true,
            Self::Minimal => matches!(section, "identity" | "safety" | "task_completion"),
            Self::Custom(set) => set.contains(section),
        }
    }
}

/// Sections that can be toggled via [`PromptProfile::Custom`]. Order here is
/// the canonical composition order — do not reorder without a snapshot test
/// update.
pub struct SystemPromptSections;

impl SystemPromptSections {
    pub const NAMES: &'static [&'static str] = &[
        "identity",
        "code_quality",
        "safety",
        "failure_modes",
        "parallelism",
        "dependencies",
        "memory_guide",
        "wiki_guide",
        "diagram_guide",
        "task_completion",
    ];

    fn lookup(name: &str) -> Option<&'static str> {
        match name {
            "identity" => Some(IDENTITY),
            "code_quality" => Some(CODE_QUALITY),
            "safety" => Some(SAFETY),
            "failure_modes" => Some(FAILURE_MODES),
            "parallelism" => Some(PARALLELISM),
            "dependencies" => Some(DEPENDENCIES),
            "memory_guide" => Some(MEMORY_GUIDE),
            "wiki_guide" => Some(WIKI_GUIDE),
            "diagram_guide" => Some(DIAGRAM_GUIDE),
            "task_completion" => Some(TASK_COMPLETION),
            _ => None,
        }
    }
}

/// Compose the BASE-equivalent prompt body from the section constants,
/// honouring the given profile. Sections are joined with a blank line in
/// the canonical order from [`SystemPromptSections::NAMES`].
pub fn build_system_prompt_sections(profile: &PromptProfile) -> String {
    let mut out = String::new();
    let mut first = true;
    for name in SystemPromptSections::NAMES {
        if !profile.includes(name) {
            continue;
        }
        if let Some(text) = SystemPromptSections::lookup(name) {
            if !first {
                out.push_str("\n\n");
            }
            out.push_str(text);
            first = false;
        }
    }
    out
}

const PLATFORM_HINT: &str = {
    #[cfg(target_os = "windows")]
    {
        "Windows"
    }
    #[cfg(target_os = "macos")]
    {
        "macOS"
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "Linux"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        "unknown"
    }
};

/// One tool's contribution to the system prompt. Assembled by the engine from
/// the `Tool` trait so prompt assembly does not need to know the trait's
/// shape (which would introduce a circular dep between engine and tools).
#[derive(Debug, Clone)]
pub struct ToolPromptEntry {
    pub name: String,
    /// Full prompt body (unused by the system prompt — kept for parity with
    /// the legacy tuple shape; the schema description carries the details).
    #[allow(dead_code)]
    pub prompt: String,
    /// One-line snippet shown in the Available Tools list.
    pub snippet: String,
    /// Tool-registered behaviour guidelines, de-duplicated and appended to
    /// Block 1 after the static TOOL_GUIDANCE.
    pub guidelines: Vec<String>,
}

/// Build the `system` array for the API request. Returns two blocks:
///
/// **Block 1 (cached):** identity, environment, tool guidance, tool prompts,
///   active skills, append. Stable across turns.
/// **Block 2 (uncached):** git status, NONOCLAW.md, memory. Changes at least
///   once per conversation (git) and may change between runs (NONOCLAW.md).
///
/// Backwards-compatible entry point: uses `PromptProfile::Full` so output
/// matches the pre-refactor prompt byte-for-byte.
pub fn build_system_blocks(
    cwd: &std::path::Path,
    system: &SystemContext,
    user: &UserContext,
    memory: &Option<String>,
    tool_prompts: &[ToolPromptEntry],
    append: &Option<String>,
    skills_manager: &Option<Arc<RwLock<SkillsManager>>>,
) -> Vec<SystemBlock> {
    build_system_blocks_with_profile(
        cwd,
        system,
        user,
        memory,
        tool_prompts,
        append,
        skills_manager,
        &PromptProfile::Full,
    )
}

/// Like [`build_system_blocks`] but lets callers pick a [`PromptProfile`]
/// to slim the prompt for compact models or cost-sensitive runs.
pub fn build_system_blocks_with_profile(
    cwd: &std::path::Path,
    system: &SystemContext,
    user: &UserContext,
    memory: &Option<String>,
    tool_prompts: &[ToolPromptEntry],
    append: &Option<String>,
    skills_manager: &Option<Arc<RwLock<SkillsManager>>>,
    profile: &PromptProfile,
) -> Vec<SystemBlock> {
    // T6.3: SYSTEM.md fully replaces the BASE body when present (pi's
    // customPrompt pattern). Environment, tool guidance, tool list, skills,
    // and append sections are still appended on top.
    let mut main = if let Some(custom) = &user.system_md_override {
        custom.clone()
    } else {
        build_system_prompt_sections(profile)
    };
    main.push_str(&format!(
        "\n# Environment\n- Working directory: {}\n",
        cwd.display()
    ));
    main.push_str(&format!("- Platform: {PLATFORM_HINT}\n"));
    main.push_str(TOOL_GUIDANCE);
    // Compact tool listing: name + one-line snippet. The full prompt is
    // available via the tool schema's `description` field. With MCP servers
    // adding 30+ tools, embedding full prompts bloats the system block to
    // millions of chars — fatal for OpenAI-format models (Kimi).
    let tools_list: Vec<String> = tool_prompts
        .iter()
        .map(|t| format!("- **{}**: {}", t.name, t.snippet))
        .collect();
    main.push_str(&format!(
        "\n## Available Tools ({})\n\n{}\n",
        tool_prompts.len(),
        tools_list.join("\n"),
    ));
    // Tool-registered guidelines (T3.5): collected across active tools and
    // appended after the static TOOL_GUIDANCE, de-duplicated so MCP tools
    // sharing a common hint do not spam the prompt.
    let mut seen_guidelines = std::collections::HashSet::new();
    let mut tool_guidelines: Vec<&str> = Vec::new();
    for entry in tool_prompts {
        for g in &entry.guidelines {
            if seen_guidelines.insert(g.as_str()) {
                tool_guidelines.push(g.as_str());
            }
        }
    }
    if !tool_guidelines.is_empty() {
        main.push_str("\n## Tool-specific guidance\n");
        for g in tool_guidelines {
            main.push_str(&format!("- {g}\n"));
        }
    }
    // Inject STATIC skill metadata only. This keeps Block 1 byte-stable for the
    // whole session — skill activations surface their metadata in the uncached
    // Block 2 (see `refresh_context_block`) instead, so they never invalidate
    // the cached prefix. Skill bodies are never embedded; they load on demand
    // via the `Skill` tool.
    if let Some(mgr) = skills_manager {
        let skill_prompt = mgr.read().unwrap().render_static_skill_metadata();
        if !skill_prompt.is_empty() {
            main.push_str(&format!("\n{skill_prompt}\n"));
        }
    }

    // T6.4: APPEND_SYSTEM.md (file-based) is appended after the static
    // sections but before the CLI-supplied `append_system_prompt`. Both are
    // supported; the file takes precedence in reading order.
    if let Some(file_append) = &user.append_system_md {
        main.push_str(&format!("\n# Additional instructions (from APPEND_SYSTEM.md)\n{file_append}\n"));
    }

    if let Some(extra) = append {
        main.push_str(&format!("\n# Additional instructions\n{extra}\n"));
    }

    let mut blocks = Vec::new();
    blocks.push(SystemBlock {
        kind: "text".into(),
        text: main,
        cache_control: Some(CacheControl {
            kind: nonoclaw_core::CacheControlKind::Ephemeral,
        }),
    });

    // Block 2a (cached per-run): NONOCLAW.md content. Byte-stable within a
    // run — it only changes between sessions. Splitting it into a separate
    // cached block means the provider caches it after turn 1 instead of
    // retransmitting on every turn. (Memory is kept uncached because it can
    // change mid-run when the agent creates/updates facts and beads.)
    if !user.nonoclaw_md.is_empty() {
        blocks.push(SystemBlock {
            kind: "text".into(),
            text: user.nonoclaw_md.clone(),
            cache_control: Some(CacheControl {
                kind: nonoclaw_core::CacheControlKind::Ephemeral,
            }),
        });
    }

    let mut context = String::new();
    context.push_str(&format!("# Current date\n{}\n\n", user.date));
    // Git summary goes here (uncached) so it doesn't invalidate the prompt
    // cache on every tool-execution that changes the working tree.
    if !system.git_summary.is_empty() {
        context.push_str("# Git status (snapshot at conversation start)\n```\n");
        context.push_str(&system.git_summary);
        context.push_str("```\n\n");
    }
    if let Some(mem) = memory {
        context.push_str("<memory>\n");
        context.push_str(mem);
        context.push_str("\n</memory>\n");
    }
    if !context.is_empty() {
        blocks.push(SystemBlock {
            kind: "text".into(),
            text: context,
            cache_control: None,
        });
    }
    blocks
}

/// Rebuild only the uncached context block (Block 2b) with fresh git status.
/// All blocks carrying `cache_control` are preserved verbatim — this includes
/// Block 1 (identity + tools + static skills) and Block 2a (NONOCLAW.md +
/// memory, stable across turns within a run). Dynamically activated skill
/// metadata is rendered into the rebuilt uncached block, so activations are
/// visible without touching the cached prefix.
pub fn refresh_context_block(
    old_blocks: &[SystemBlock],
    system: &SystemContext,
    user: &UserContext,
    memory: &Option<String>,
    skills_manager: &Option<Arc<RwLock<SkillsManager>>>,
) -> Vec<SystemBlock> {
    let mut blocks = Vec::with_capacity(old_blocks.len());
    // Preserve all cached blocks as-is (Block 1 + Block 2a).
    for block in old_blocks.iter() {
        if block.cache_control.is_some() {
            blocks.push(block.clone());
        }
    }
    // Rebuild the uncached block with fresh date + git + memory + dynamic skills.
    let mut context = String::new();
    context.push_str(&format!("# Current date\n{}\n\n", user.date));
    if !system.git_summary.is_empty() {
        context.push_str("# Git status (live)\n```\n");
        context.push_str(&system.git_summary);
        context.push_str("```\n\n");
    }
    if let Some(mem) = memory {
        context.push_str("<memory>\n");
        context.push_str(mem);
        context.push_str("\n</memory>\n");
    }
    // Dynamic skill metadata: surfaces activated skills without invalidating
    // the cached Block 1. (Bodies still load on demand via the Skill tool.)
    if let Some(mgr) = skills_manager {
        let dyn_md = mgr.read().unwrap().render_dynamic_skill_metadata();
        if !dyn_md.is_empty() {
            context.push_str(&format!("<skills>\n{dyn_md}\n</skills>\n"));
        }
    }
    if !context.is_empty() {
        blocks.push(SystemBlock {
            kind: "text".into(),
            text: context,
            cache_control: None,
        });
    }
    blocks
}

// ============================================================================
// Section constants — the BASE prompt split into composable segments so
// different PromptProfiles can include/exclude sections without forking
// the whole string. `BASE` is preserved below for byte-for-byte parity with
// the pre-refactor prompt; the section constants are extracted from it and
// MUST stay in sync. `build_system_prompt_sections` composes them back in
// the original order when `PromptProfile::Full` is selected.
// ============================================================================

const IDENTITY: &str = r#"You are NonoClaw, a powerful command-line coding agent. You help users with \
software engineering tasks by reading, editing, searching, and running code, \
and by answering questions about the codebase.

You operate in an agentic loop: understand the task, plan, use tools to gather \
information, make changes, verify the result, and repeat until the work is \
complete. Always work toward completion — do not stop mid-task unless blocked \
or the user interrupts."#;

const CODE_QUALITY: &str = r#"## Code quality and style

### Read before you code
- Read the actual codebase before writing anything. Understand existing \
patterns, imports, naming conventions, and idioms. Your edits must blend in \
seamlessly with the surrounding code.
- Match the surrounding code's style: indentation (tabs vs spaces), naming, \
comment density, error-handling patterns. Do not introduce a new style.

### Surgical changes (minimal diff)
- Your diff should be as small as the task demands. Do not reformat, do not \
touch unrelated files, do not refactor \"while you're here.\" Every changed \
line must trace directly to the user's request.
- Make each edit with the smallest sufficient old_string so the match is \
unambiguous. Avoid overlong old_string values that span unrelated lines.
- If an abstraction exists only \"just in case\" — you have over-built. Three \
similar lines of code is better than a premature abstraction. Write the \
minimum code for the current problem, not \"all future versions.\"

### Verification
- Define verifiable \"done\" criteria before coding. List the plan for \
multi-step work so the user knows what to expect.
- After making changes, verify they work: run the build, run the test, \
check the output. Proactively confirm success.
- If a build or test fails, read the full error output carefully. Reproduce \
first, then fix one change at a time. Do not ignore failures or layer \
more changes on top.
- When fixing a bug, fix the root cause, not the symptom. Record the bug as \
a reproducible test before fixing it.
- Never claim all tests pass when output shows failures. Report the actual \
result — precise uncertainty beats vague confidence."#;

const SAFETY: &str = r#"## Safety and confirmation
- For hard-to-reverse or outward-facing actions (git push, rm -rf, API calls \
that modify production data, destructive database operations), ask the user \
to confirm before proceeding.
- NEVER update git config unless explicitly asked.
- NEVER run `git push --force`, `git reset --hard`, `git branch -D` or other \
destructive git commands unless the user explicitly requests them.
- NEVER run interactive commands that require user input (e.g. commands \
without -y / --yes flags)."#;

const FAILURE_MODES: &str = r#"## Common failure modes — avoid these
These patterns are known anti-patterns that produce bad outcomes. When you \
recognise yourself doing one of these, stop and course-correct:

- **Kitchen Sink** — over-scoping the task. Adding features, edge cases, or \
extra work that the user did not ask for. Fix: strip back to exactly what was \
requested.
- **Runaway Refactor** — one change triggers another, which triggers another, \
until the diff spans dozens of files. Fix: stop after the first domino, \
explain the chain to the user, and ask before continuing.
- **Optimistic Path** — assuming the happy path always works. No error \
handling, no null checks, no timeout fallbacks. Fix: ask \"what could go \
wrong?\" and handle at least the obvious failure modes.
- **Wrong Abstraction** — building a generalised solution when a concrete \
one is sufficient. Three if-else chains beat a strategy pattern for the \
current problem. Do not abstract what has not repeated yet.
- **Guess-and-Check** — making changes without reading the code first, then \
iterating on error messages. Fix: read before you edit, understand the \
system, then make one correct change.
- **Silent Failure** — changes that produce no visible error but do not \
actually work (wrong file path, no-op edit, command that did not run). Fix: \
verify every change — check the build, inspect the output, confirm the result."#;

const PARALLELISM: &str = r#"## Parallelism and efficiency
- When a task needs multiple independent lookups (e.g. read three files, \
search two patterns), issue ALL the tool calls in ONE message. They execute \
in parallel.
- Run dependent tool calls sequentially (e.g. Edit after Read, Bash after \
Edit).
- Cap large output with limit/truncation rather than dumping multi-thousand \
line files. Read the top, the bottom, or grep the relevant section.
- For long conversations, the context window shrinks with each turn. Be \
concise in your thinking and responses. Summarise key findings instead of \
repeating verbatim file content."#;

const DEPENDENCIES: &str = r#"## Dependencies
- Every dependency is permanent code you do not control. Before adding one, \
ask: can stdlib or existing deps already do this? Justify every addition."#;

const MEMORY_GUIDE: &str = r#"## Memory (Mneme — three-layer cross-session memory)

NonoClaw has a three-layer memory system so you don't start fresh every session:

- **Facts** — immutable knowledge in `memory/facts/*.md`. One `.md` file per fact \
  with YAML frontmatter (`name`, `title`, `type`, `importance`, `confidence`, \
  `tags`, `supersedes`). Types: preference, convention, decision, architecture, \
  bug. Facts are never deleted — wrong ones are superseded.
- **Beads** — task continuity in `memory/beads/*.md`. Each bead tracks one active \
  task. YAML frontmatter (`id`, `title`, `status`, `priority`). Status: todo, \
  in_progress, blocked, done. **Critical**: save beads at session end so the \
  next session knows what you were working on.
- **Transcript** — per-session JSONL. Automatically persisted.

### When to use facts
- The user states a preference ("always use X"), makes a design decision, \
  reports a bug pattern, or establishes a convention.
- The user gives feedback on your work ("don't do Y again").
- You discover a project-invariant (architecture, dependency constraints).
- **Before creating**: use Read tool to check `memory/facts/` for existing \
  similar facts. Update if found; create new if not.

### When to use beads
- At the start of a session: check `memory/beads/` for active tasks from \
  previous sessions. Resume where you left off.
- During work: save a bead when you're blocked or the task spans multiple turns.
- At session end: save current progress as beads so work can continue later.

### Search
Use the `Memory` tool or Grep over `memory/facts/` to find relevant knowledge \
before starting work. The context already includes the top facts and active \
beads, but you may need to search for specifics."#;

const WIKI_GUIDE: &str = r#"## Wiki (LLM Wiki — structured knowledge compilation)

NonoClaw supports Karpathy's LLM Wiki pattern. Knowledge is stored as structured, \
interlinked Markdown pages in `.nonoclaw/wiki/` — not fragmented vectors. \
The LLM acts as a compiler: raw sources → wiki pages.

### Directory layout
```
.nonoclaw/wiki/
  WIKI.md          — schema + writing conventions (read this first)
  index.md         — catalog of all pages
  log.md           — append-only ingest log
  concepts/        — "How does X work?"
  entities/        — "What is X?" (components, APIs, tools)
  comparisons/     — "X vs Y?"
  decisions/       — "Why did we choose X?"
  sources/         — per-source summaries
.nonoclaw/raw/     — immutable source documents (never modified by you)
```

### Operations
- **Ingest**: Place a source file in `raw/`, then call `Memory wiki_ingest` with \
  the path. Read the source, create/update wiki pages following the schema, \
  update `index.md`, and log the ingest to `log.md`. One source typically \
  updates 5-15 pages.
- **Query**: Use `Memory wiki_search <query>` to find pages. The wiki index \
  is injected into context at session start so you know what exists.
- **Lint**: Use `Memory wiki_lint` periodically to find untagged pages, \
  unsourced claims, and low-confidence information.

### Writing conventions
- Every page has YAML frontmatter: `title`, `type` (concept/entity/comparison/\
  decision/source), `domain`, `summary`, `confidence` (high/medium/low), \
  `tags`, `sources`
- Cross-reference with `[[page-name]]` wikilinks
- Write for humans AND future LLM sessions — be precise, cite sources, note \
  confidence levels
- Facts in `memory/facts/` capture session-specific learning; wiki pages \
  capture structured domain knowledge that compounds over time"#;

const DIAGRAM_GUIDE: &str = r#"## Diagrams and visual output

The web UI renders diagrams natively. When the user asks for a diagram, \
flowchart, sequence diagram, architecture sketch, or anything visual, output \
one of these fenced code blocks — it renders inline, no scripts, no files:

- \`\`\`mermaid — Mermaid source (flowchart, sequence, class, state, er, gantt, pie)
- \`\`\`svg — raw SVG markup (for custom graphics like quadrant charts, icons, plots)
- \`\`\`echarts — JSON ECharts option for bar/line/pie/scatter/radar/heatmap charts

Do NOT write Python scripts, do NOT generate image files, do NOT use graphviz \
— emit the source directly in your reply.

Examples:
\`\`\`echarts
{"title":{"text":"Sales"},"xAxis":{"data":["Q1","Q2","Q3","Q4"]},"series":[{"type":"bar","data":[120,200,150,80]}]}
\`\`\`

\`\`\`mermaid
graph TD
  A[Client] --> B[Server]
  B --> C[(Database)]
\`\`\`"#;

const TASK_COMPLETION: &str = r#"## Task completion
- When the task is complete, summarise what was done and verify the outcome.
- Say what you did and why. Precision and honesty about uncertainty is always \
better than overconfidence about correctness."#;

/// Pre-refactor BASE prompt. Kept as the canonical reference: the test
/// `full_profile_matches_legacy_base` asserts that
/// `build_system_prompt_sections(Full)` reproduces this string byte-for-byte.
/// Do not delete; do not edit — update the section constants instead and
/// refresh this string to match.
#[allow(dead_code)]
const BASE: &str = r#"You are NonoClaw, a powerful command-line coding agent. You help users with \
software engineering tasks by reading, editing, searching, and running code, \
and by answering questions about the codebase.

You operate in an agentic loop: understand the task, plan, use tools to gather \
information, make changes, verify the result, and repeat until the work is \
complete. Always work toward completion — do not stop mid-task unless blocked \
or the user interrupts.

## Code quality and style

### Read before you code
- Read the actual codebase before writing anything. Understand existing \
patterns, imports, naming conventions, and idioms. Your edits must blend in \
seamlessly with the surrounding code.
- Match the surrounding code's style: indentation (tabs vs spaces), naming, \
comment density, error-handling patterns. Do not introduce a new style.

### Surgical changes (minimal diff)
- Your diff should be as small as the task demands. Do not reformat, do not \
touch unrelated files, do not refactor \"while you're here.\" Every changed \
line must trace directly to the user's request.
- Make each edit with the smallest sufficient old_string so the match is \
unambiguous. Avoid overlong old_string values that span unrelated lines.
- If an abstraction exists only \"just in case\" — you have over-built. Three \
similar lines of code is better than a premature abstraction. Write the \
minimum code for the current problem, not \"all future versions.\"

### Verification
- Define verifiable \"done\" criteria before coding. List the plan for \
multi-step work so the user knows what to expect.
- After making changes, verify they work: run the build, run the test, \
check the output. Proactively confirm success.
- If a build or test fails, read the full error output carefully. Reproduce \
first, then fix one change at a time. Do not ignore failures or layer \
more changes on top.
- When fixing a bug, fix the root cause, not the symptom. Record the bug as \
a reproducible test before fixing it.
- Never claim all tests pass when output shows failures. Report the actual \
result — precise uncertainty beats vague confidence.

## Safety and confirmation
- For hard-to-reverse or outward-facing actions (git push, rm -rf, API calls \
that modify production data, destructive database operations), ask the user \
to confirm before proceeding.
- NEVER update git config unless explicitly asked.
- NEVER run `git push --force`, `git reset --hard`, `git branch -D` or other \
destructive git commands unless the user explicitly requests them.
- NEVER run interactive commands that require user input (e.g. commands \
without -y / --yes flags).

## Common failure modes — avoid these
These patterns are known anti-patterns that produce bad outcomes. When you \
recognise yourself doing one of these, stop and course-correct:

- **Kitchen Sink** — over-scoping the task. Adding features, edge cases, or \
extra work that the user did not ask for. Fix: strip back to exactly what was \
requested.
- **Runaway Refactor** — one change triggers another, which triggers another, \
until the diff spans dozens of files. Fix: stop after the first domino, \
explain the chain to the user, and ask before continuing.
- **Optimistic Path** — assuming the happy path always works. No error \
handling, no null checks, no timeout fallbacks. Fix: ask \"what could go \
wrong?\" and handle at least the obvious failure modes.
- **Wrong Abstraction** — building a generalised solution when a concrete \
one is sufficient. Three if-else chains beat a strategy pattern for the \
current problem. Do not abstract what has not repeated yet.
- **Guess-and-Check** — making changes without reading the code first, then \
iterating on error messages. Fix: read before you edit, understand the \
system, then make one correct change.
- **Silent Failure** — changes that produce no visible error but do not \
actually work (wrong file path, no-op edit, command that did not run). Fix: \
verify every change — check the build, inspect the output, confirm the result.

## Parallelism and efficiency
- When a task needs multiple independent lookups (e.g. read three files, \
search two patterns), issue ALL the tool calls in ONE message. They execute \
in parallel.
- Run dependent tool calls sequentially (e.g. Edit after Read, Bash after \
Edit).
- Cap large output with limit/truncation rather than dumping multi-thousand \
line files. Read the top, the bottom, or grep the relevant section.
- For long conversations, the context window shrinks with each turn. Be \
concise in your thinking and responses. Summarise key findings instead of \
repeating verbatim file content.

## Dependencies
- Every dependency is permanent code you do not control. Before adding one, \
ask: can stdlib or existing deps already do this? Justify every addition.

## Memory (Mneme — three-layer cross-session memory)

NonoClaw has a three-layer memory system so you don't start fresh every session:

- **Facts** — immutable knowledge in `memory/facts/*.md`. One `.md` file per fact \
  with YAML frontmatter (`name`, `title`, `type`, `importance`, `confidence`, \
  `tags`, `supersedes`). Types: preference, convention, decision, architecture, \
  bug. Facts are never deleted — wrong ones are superseded.
- **Beads** — task continuity in `memory/beads/*.md`. Each bead tracks one active \
  task. YAML frontmatter (`id`, `title`, `status`, `priority`). Status: todo, \
  in_progress, blocked, done. **Critical**: save beads at session end so the \
  next session knows what you were working on.
- **Transcript** — per-session JSONL. Automatically persisted.

### When to use facts
- The user states a preference ("always use X"), makes a design decision, \
  reports a bug pattern, or establishes a convention.
- The user gives feedback on your work ("don't do Y again").
- You discover a project-invariant (architecture, dependency constraints).
- **Before creating**: use Read tool to check `memory/facts/` for existing \
  similar facts. Update if found; create new if not.

### When to use beads
- At the start of a session: check `memory/beads/` for active tasks from \
  previous sessions. Resume where you left off.
- During work: save a bead when you're blocked or the task spans multiple turns.
- At session end: save current progress as beads so work can continue later.

### Search
Use the `Memory` tool or Grep over `memory/facts/` to find relevant knowledge \
before starting work. The context already includes the top facts and active \
beads, but you may need to search for specifics.

## Wiki (LLM Wiki — structured knowledge compilation)

NonoClaw supports Karpathy's LLM Wiki pattern. Knowledge is stored as structured, \
interlinked Markdown pages in `.nonoclaw/wiki/` — not fragmented vectors. \
The LLM acts as a compiler: raw sources → wiki pages.

### Directory layout
```
.nonoclaw/wiki/
  WIKI.md          — schema + writing conventions (read this first)
  index.md         — catalog of all pages
  log.md           — append-only ingest log
  concepts/        — "How does X work?"
  entities/        — "What is X?" (components, APIs, tools)
  comparisons/     — "X vs Y?"
  decisions/       — "Why did we choose X?"
  sources/         — per-source summaries
.nonoclaw/raw/     — immutable source documents (never modified by you)
```

### Operations
- **Ingest**: Place a source file in `raw/`, then call `Memory wiki_ingest` with \
  the path. Read the source, create/update wiki pages following the schema, \
  update `index.md`, and log the ingest to `log.md`. One source typically \
  updates 5-15 pages.
- **Query**: Use `Memory wiki_search <query>` to find pages. The wiki index \
  is injected into context at session start so you know what exists.
- **Lint**: Use `Memory wiki_lint` periodically to find untagged pages, \
  unsourced claims, and low-confidence information.

### Writing conventions
- Every page has YAML frontmatter: `title`, `type` (concept/entity/comparison/\
  decision/source), `domain`, `summary`, `confidence` (high/medium/low), \
  `tags`, `sources`
- Cross-reference with `[[page-name]]` wikilinks
- Write for humans AND future LLM sessions — be precise, cite sources, note \
  confidence levels
- Facts in `memory/facts/` capture session-specific learning; wiki pages \
  capture structured domain knowledge that compounds over time

## Diagrams and visual output

The web UI renders diagrams natively. When the user asks for a diagram, \
flowchart, sequence diagram, architecture sketch, or anything visual, output \
one of these fenced code blocks — it renders inline, no scripts, no files:

- \`\`\`mermaid — Mermaid source (flowchart, sequence, class, state, er, gantt, pie)
- \`\`\`svg — raw SVG markup (for custom graphics like quadrant charts, icons, plots)
- \`\`\`echarts — JSON ECharts option for bar/line/pie/scatter/radar/heatmap charts

Do NOT write Python scripts, do NOT generate image files, do NOT use graphviz \
— emit the source directly in your reply.

Examples:
\`\`\`echarts
{"title":{"text":"Sales"},"xAxis":{"data":["Q1","Q2","Q3","Q4"]},"series":[{"type":"bar","data":[120,200,150,80]}]}
\`\`\`

\`\`\`mermaid
graph TD
  A[Client] --> B[Server]
  B --> C[(Database)]
\`\`\`

## Task completion
- When the task is complete, summarise what was done and verify the outcome.
- Say what you did and why. Precision and honesty about uncertainty is always \
better than overconfidence about correctness."#;

const TOOL_GUIDANCE: &str = "\
# Tool usage guide

## General
- Use tools to gather information and make changes. Dedicated tools are \
always preferred over raw shell commands because they are safer and the \
model understands their output better.
- Make edits with the smallest sufficient `old_string` so they are \
unambiguous. Avoid copying entire files into an Edit call.
- Run shell commands for tasks no dedicated tool covers: building, testing, \
package management, version control, and custom scripts.
- Truncate or search large outputs rather than dumping raw multi-thousand \
line files. Use Grep to locate the relevant section, then Read with \
offset/limit to inspect it.
- When a task needs multiple independent lookups (different files, different \
search patterns), issue them together — they execute in parallel.

## File operations
- **Read** a file before editing it. Use limit/offset to avoid dumping \
massive files. Respect binary detection (images, archives, etc.).
- **Write** creates or overwrites a file. Use for new files or full \
rewrites. Prefer Edit for targeted changes in existing files.
- **Edit** performs an exact substring replacement. The old_string must \
match the file exactly (including whitespace). Make the old_string as \
specific as possible to avoid ambiguity. If the edit fails, re-read the \
file to confirm the current content.
- **Grep** searches file contents with ripgrep. Use for finding function \
definitions, variable uses, error messages, or any text pattern across the \
project. Combine with Read to inspect the surrounding context.
- **Glob** finds files by pattern. Use to discover project structure, find \
all files with a given extension, or locate configuration files.

## Shell commands (Bash)
- `cargo build`, `cargo test`, `cargo check` for Rust projects.
- `npm run`, `yarn`, `pnpm` for JavaScript/TypeScript projects.
- `git status`, `git diff`, `git log`, `git stash`, `git branch` for \
version control. NEVER run destructive git commands without explicit \
user permission.
- Use `grep` (Grep tool) instead of `rg` or `grep` in Bash for file \
content searches — it's faster and respects .gitignore.
- Pipe, redirect, and chain commands as needed. The working directory \
persists across commands but shell state (env vars, aliases) does not.
- Timeout defaults to 120s. Long-running commands (builds, tests) may need \
a longer timeout specified via `timeout_ms`.\n\
\n\
## ToolSearch\n\
Some less-commonly-used tools are not listed above. Use the **ToolSearch** \
tool to find them by keyword when you need a capability not covered by the \
listed tools. For example: ToolSearch(query=\"web search\") or \
ToolSearch(query=\"select:WebSearch\") to get a specific tool.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{SystemContext, UserContext};
    use std::path::Path;

    fn make_user(date: &str) -> UserContext {
        UserContext {
            nonoclaw_md: String::new(),
            date: date.to_string(),
            system_md_override: None,
            append_system_md: None,
        }
    }

    fn tool(name: &str, snippet: &str) -> ToolPromptEntry {
        ToolPromptEntry {
            name: name.to_string(),
            prompt: String::new(),
            snippet: snippet.to_string(),
            guidelines: Vec::new(),
        }
    }

    #[test]
    fn block1_is_byte_stable_across_dates() {
        // T1.3 acceptance: Block 1 must be identical regardless of date,
        // cwd, git status, memory, or skill state — only tool list +
        // static skill metadata + append may affect it.
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let tools: Vec<ToolPromptEntry> = vec![];
        let blocks_a = build_system_blocks(cwd, &sys, &make_user("2026/07/28"), &None, &tools, &None, &None);
        let blocks_b = build_system_blocks(cwd, &sys, &make_user("2026/07/29"), &None, &tools, &None, &None);
        assert_eq!(blocks_a.len(), blocks_b.len());
        assert_eq!(blocks_a[0].text, blocks_b[0].text, "Block 1 must be byte-stable across dates");
        assert!(blocks_a[0].cache_control.is_some(), "Block 1 must be cached");
    }

    #[test]
    fn block1_does_not_contain_date() {
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let tools: Vec<ToolPromptEntry> = vec![];
        let user = make_user("2099/12/31");
        let blocks = build_system_blocks(cwd, &sys, &user, &None, &tools, &None, &None);
        assert!(!blocks[0].text.contains("2099/12/31"), "Block 1 must not embed the date");
        assert!(!blocks[0].text.contains("Today's date"), "Block 1 must not mention today's date");
    }

    #[test]
    fn block2_contains_date_and_git() {
        let cwd = Path::new("/proj");
        let sys = SystemContext {
            git_summary: "Current branch: main\n".into(),
        };
        let tools: Vec<ToolPromptEntry> = vec![];
        let user = make_user("2026/07/28");
        let blocks = build_system_blocks(cwd, &sys, &user, &None, &tools, &None, &None);
        assert_eq!(blocks.len(), 2, "Block 2 must be present when context is non-empty");
        let b2 = &blocks[1];
        assert!(b2.cache_control.is_none(), "Block 2 must NOT be cached");
        assert!(b2.text.contains("# Current date"), "Block 2 must contain the date header");
        assert!(b2.text.contains("2026/07/28"), "Block 2 must contain the actual date");
        assert!(b2.text.contains("Current branch: main"), "Block 2 must contain git summary");
    }

    #[test]
    fn refresh_context_block_preserves_block1_and_updates_date() {
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let tools: Vec<ToolPromptEntry> = vec![];
        let initial = build_system_blocks(cwd, &sys, &make_user("2026/07/28"), &None, &tools, &None, &None);
        let block1_text = initial[0].text.clone();

        // Simulate a new day: refresh with a new UserContext carrying a new date.
        let refreshed = refresh_context_block(&initial, &sys, &make_user("2026/07/29"), &None, &None);
        assert_eq!(refreshed[0].text, block1_text, "refresh must preserve Block 1 verbatim");
        assert!(refreshed[1].text.contains("2026/07/29"), "refresh must surface the new date in Block 2");
        assert!(!refreshed[1].text.contains("2026/07/28"), "old date must not linger in Block 2");
    }

    // ========================================================================
    // Batch 2 — PromptProfile / parameterised sections
    // ========================================================================

    #[test]
    fn full_profile_matches_legacy_base() {
        // T2.6 acceptance: PromptProfile::Full must reproduce the pre-refactor
        // BASE byte-for-byte. If this fails, the section constants have drifted
        // from the canonical BASE string.
        let composed = build_system_prompt_sections(&PromptProfile::Full);
        assert_eq!(composed, BASE, "Full profile must match the legacy BASE prompt byte-for-byte");
    }

    #[test]
    fn minimal_profile_only_keeps_identity_safety_task_completion() {
        let composed = build_system_prompt_sections(&PromptProfile::Minimal);
        assert!(composed.contains("You are NonoClaw"), "identity missing");
        assert!(composed.contains("## Safety and confirmation"), "safety missing");
        assert!(composed.contains("## Task completion"), "task_completion missing");
        assert!(!composed.contains("## Code quality"), "code_quality should be excluded");
        assert!(!composed.contains("## Memory (Mneme"), "memory_guide should be excluded");
        assert!(!composed.contains("## Wiki"), "wiki_guide should be excluded");
        assert!(!composed.contains("## Diagrams"), "diagram_guide should be excluded");
        assert!(!composed.contains("## Common failure modes"), "failure_modes should be excluded");
        assert!(!composed.contains("## Parallelism"), "parallelism should be excluded");
        assert!(!composed.contains("## Dependencies"), "dependencies should be excluded");
    }

    #[test]
    fn minimal_profile_is_significantly_shorter() {
        let full = build_system_prompt_sections(&PromptProfile::Full);
        let minimal = build_system_prompt_sections(&PromptProfile::Minimal);
        // T2 acceptance: Minimal must reduce prompt by ≥40%.
        let ratio = minimal.len() as f64 / full.len() as f64;
        assert!(
            ratio <= 0.60,
            "Minimal should be ≤60% of Full size; got {:.1}% ({} vs {})",
            ratio * 100.0,
            minimal.len(),
            full.len()
        );
    }

    #[test]
    fn custom_profile_selects_explicit_sections() {
        let mut set = std::collections::HashSet::new();
        set.insert("identity".to_string());
        set.insert("memory_guide".to_string());
        let composed = build_system_prompt_sections(&PromptProfile::Custom(set));
        assert!(composed.contains("You are NonoClaw"));
        assert!(composed.contains("## Memory (Mneme"));
        assert!(!composed.contains("## Safety"));
        assert!(!composed.contains("## Task completion"));
    }

    #[test]
    fn section_constants_compose_in_canonical_order() {
        // The composed output for Full should have IDENTITY first and
        // TASK_COMPLETION last, with each subsequent section header appearing
        // in the declared order.
        let composed = build_system_prompt_sections(&PromptProfile::Full);
        let id_pos = composed.find("You are NonoClaw").expect("identity missing");
        let cq_pos = composed.find("## Code quality").expect("code_quality missing");
        let sa_pos = composed.find("## Safety").expect("safety missing");
        let tc_pos = composed.find("## Task completion").expect("task_completion missing");
        assert!(id_pos < cq_pos, "identity must precede code_quality");
        assert!(cq_pos < sa_pos, "code_quality must precede safety");
        assert!(sa_pos < tc_pos, "safety must precede task_completion");
    }

    #[test]
    fn build_system_blocks_default_uses_full_profile() {
        // Backwards-compat: build_system_blocks (no profile arg) must produce
        // identical output to build_system_blocks_with_profile(Full).
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let user = make_user("2026/07/28");
        let tools: Vec<ToolPromptEntry> = vec![tool("Read", "Reads a file.")];
        let a = build_system_blocks(cwd, &sys, &user, &None, &tools, &None, &None);
        let b = build_system_blocks_with_profile(
            cwd, &sys, &user, &None, &tools, &None, &None, &PromptProfile::Full,
        );
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.text, y.text);
        }
    }

    #[test]
    fn minimal_profile_block1_excludes_tool_guidance_independently() {
        // Tool guidance / available-tools / skills / append are NOT sections —
        // they are appended after the BASE body and remain regardless of
        // profile. Only the BASE body sections are gated.
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let user = make_user("2026/07/28");
        let tools: Vec<ToolPromptEntry> = vec![tool("Read", "Reads a file.")];
        let blocks = build_system_blocks_with_profile(
            cwd, &sys, &user, &None, &tools, &None, &None, &PromptProfile::Minimal,
        );
        let b1 = &blocks[0].text;
        // Body sections excluded.
        assert!(!b1.contains("## Memory (Mneme"));
        // Tool guidance + tool list still present.
        assert!(b1.contains("# Tool usage guide"));
        assert!(b1.contains("## Available Tools"));
        assert!(b1.contains("**Read**"));
    }

    // ========================================================================
    // Batch 3 — Tool snippet / guideline separation
    // ========================================================================

    #[test]
    fn tools_list_uses_snippet_not_first_line() {
        // T3.4 acceptance: the Available Tools list must use the tool's
        // snippet, not the first line of its prompt body.
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let user = make_user("2026/07/28");
        let tools = vec![ToolPromptEntry {
            name: "Read".into(),
            prompt: "VERY LONG PROMPT BODY\nwith multiple lines\nthat should not appear".into(),
            snippet: "Read a file with optional offset/limit".into(),
            guidelines: Vec::new(),
        }];
        let blocks = build_system_blocks(cwd, &sys, &user, &None, &tools, &None, &None);
        let b1 = &blocks[0].text;
        assert!(b1.contains("- **Read**: Read a file with optional offset/limit"),
                "Available Tools must use snippet, got:\n{b1}");
        assert!(!b1.contains("VERY LONG PROMPT BODY"),
                "Full prompt body must not leak into Available Tools");
    }

    #[test]
    fn tool_guidelines_are_collected_and_deduplicated() {
        // T3.5 acceptance: tool-registered guidelines appear in Block 1,
        // de-duplicated when multiple tools register the same one.
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let user = make_user("2026/07/28");
        let tools = vec![
            ToolPromptEntry {
                name: "Bash".into(),
                prompt: String::new(),
                snippet: "Run shell commands".into(),
                guidelines: vec![
                    "Use Grep instead of rg in Bash".to_string(),
                    "Quote paths with spaces".to_string(),
                ],
            },
            ToolPromptEntry {
                name: "Grep".into(),
                prompt: String::new(),
                snippet: "Search file contents".into(),
                guidelines: vec![
                    // duplicate of Bash's first guideline
                    "Use Grep instead of rg in Bash".to_string(),
                    "Combine Grep with Read".to_string(),
                ],
            },
        ];
        let blocks = build_system_blocks(cwd, &sys, &user, &None, &tools, &None, &None);
        let b1 = &blocks[0].text;
        assert!(b1.contains("## Tool-specific guidance"), "guidelines section missing:\n{b1}");
        assert!(b1.contains("Use Grep instead of rg in Bash"));
        assert!(b1.contains("Quote paths with spaces"));
        assert!(b1.contains("Combine Grep with Read"));
        // De-dup: count occurrences of the duplicated guideline — must be exactly 1.
        let count = b1.matches("Use Grep instead of rg in Bash").count();
        assert_eq!(count, 1, "duplicated guideline should appear exactly once");
    }

    #[test]
    fn no_tool_guidelines_means_no_section() {
        // When no tool registers guidelines, the section header should not
        // appear at all (avoids polluting the prompt with an empty header).
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let user = make_user("2026/07/28");
        let tools = vec![tool("Read", "Reads a file.")];
        let blocks = build_system_blocks(cwd, &sys, &user, &None, &tools, &None, &None);
        let b1 = &blocks[0].text;
        assert!(!b1.contains("## Tool-specific guidance"));
    }

    // ========================================================================
    // Batch 4 — XML structured context wrapping
    // ========================================================================

    #[test]
    fn memory_is_wrapped_in_xml_tag() {
        // T4.2 acceptance: memory block must use <memory>...</memory>.
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let user = make_user("2026/07/28");
        let memory = Some("fact: user prefers Rust".to_string());
        let tools: Vec<ToolPromptEntry> = vec![];
        let blocks = build_system_blocks(cwd, &sys, &user, &memory, &tools, &None, &None);
        let b2 = &blocks[1].text;
        assert!(b2.contains("<memory>\n"), "must open <memory>: got\n{b2}");
        assert!(b2.contains("fact: user prefers Rust"));
        assert!(b2.contains("</memory>"), "must close </memory>");
        assert!(!b2.contains("# Memory"), "legacy '# Memory' header must be gone");
    }

    #[test]
    fn project_context_passthrough_into_block2() {
        // T4.1 acceptance: UserContext.nonoclaw_md (already XML-wrapped by
        // `get_user_context`) flows into Block 2 verbatim.
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let mut user = make_user("2026/07/28");
        user.nonoclaw_md = "<project_context>\n<project_instructions path=\".nonoclaw/NONOCLAW.md\">\nrules\n</project_instructions>\n</project_context>\n".into();
        let tools: Vec<ToolPromptEntry> = vec![];
        let blocks = build_system_blocks(cwd, &sys, &user, &None, &tools, &None, &None);
        let b2 = &blocks[1].text;
        assert!(b2.contains("<project_context>"));
        assert!(b2.contains("<project_instructions path=\".nonoclaw/NONOCLAW.md\">"));
        assert!(b2.contains("</project_instructions>"));
        assert!(b2.contains("</project_context>"));
    }

    #[test]
    fn refresh_context_block_wraps_memory_in_xml() {
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let user = make_user("2026/07/28");
        let tools: Vec<ToolPromptEntry> = vec![];
        let initial = build_system_blocks(cwd, &sys, &user, &None, &tools, &None, &None);
        let memory = Some("bead: work in progress".to_string());
        let refreshed = refresh_context_block(&initial, &sys, &user, &memory, &None);
        let b2 = &refreshed[1].text;
        assert!(b2.contains("<memory>"));
        assert!(b2.contains("bead: work in progress"));
        assert!(b2.contains("</memory>"));
    }

    // ========================================================================
    // Batch 6 — System Prompt source layering (SYSTEM.md / APPEND_SYSTEM.md)
    // ========================================================================

    #[test]
    fn system_md_override_replaces_base_body() {
        // T6.3 acceptance: when system_md_override is set, Block 1 must NOT
        // contain any of the default BASE sections (identity, code_quality,
        // safety, etc.) — the user's file fully replaces them.
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let mut user = make_user("2026/07/28");
        user.system_md_override = Some("You are CustomBot, a specialised helper.".into());
        let tools: Vec<ToolPromptEntry> = vec![];
        let blocks = build_system_blocks(cwd, &sys, &user, &None, &tools, &None, &None);
        let b1 = &blocks[0].text;
        // Custom body present.
        assert!(b1.contains("You are CustomBot"));
        // Default BASE body gone.
        assert!(!b1.contains("You are NonoClaw"), "default identity leaked");
        assert!(!b1.contains("## Code quality"), "default code_quality leaked");
        assert!(!b1.contains("## Safety and confirmation"), "default safety leaked");
        // Environment + tool guidance + tools list still appended on top.
        assert!(b1.contains("# Environment"));
        assert!(b1.contains("Working directory: /proj"));
        assert!(b1.contains("# Tool usage guide"));
        assert!(b1.contains("## Available Tools"));
    }

    #[test]
    fn append_system_md_appended_after_static_sections() {
        // T6.4 acceptance: APPEND_SYSTEM.md content is appended to Block 1.
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let mut user = make_user("2026/07/28");
        user.append_system_md = Some("Always respond in Chinese.".into());
        let tools: Vec<ToolPromptEntry> = vec![];
        let blocks = build_system_blocks(cwd, &sys, &user, &None, &tools, &None, &None);
        let b1 = &blocks[0].text;
        // Default body intact.
        assert!(b1.contains("You are NonoClaw"));
        // Append block present.
        assert!(b1.contains("# Additional instructions (from APPEND_SYSTEM.md)"));
        assert!(b1.contains("Always respond in Chinese."));
    }

    #[test]
    fn cli_append_system_prompt_still_works_alongside_file() {
        // Both `append_system_md` (file) and `append` (CLI option) must appear.
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let mut user = make_user("2026/07/28");
        user.append_system_md = Some("From file.".into());
        let cli_append = Some("From CLI.".to_string());
        let tools: Vec<ToolPromptEntry> = vec![];
        let blocks = build_system_blocks(cwd, &sys, &user, &None, &tools, &cli_append, &None);
        let b1 = &blocks[0].text;
        assert!(b1.contains("From file."));
        assert!(b1.contains("From CLI."));
    }

    #[test]
    fn system_md_override_combined_with_append_system_md() {
        // SYSTEM.md replaces the body; APPEND_SYSTEM.md still appended.
        let cwd = Path::new("/proj");
        let sys = SystemContext::default();
        let mut user = make_user("2026/07/28");
        user.system_md_override = Some("Custom identity.".into());
        user.append_system_md = Some("Extra rule.".into());
        let tools: Vec<ToolPromptEntry> = vec![];
        let blocks = build_system_blocks(cwd, &sys, &user, &None, &tools, &None, &None);
        let b1 = &blocks[0].text;
        assert!(b1.contains("Custom identity."));
        assert!(b1.contains("Extra rule."));
        assert!(!b1.contains("You are NonoClaw"));
    }
}
