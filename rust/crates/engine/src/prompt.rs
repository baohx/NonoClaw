//! System-prompt assembly. The full Claude Code system prompt is built from many
//! fragments across `src/` and is not present verbatim in this extraction; this
//! module assembles a faithful *functional* equivalent: identity + environment +
//! tool guidance + NONOCLAW.md + memory.

use nonoclaw_api::SystemBlock;
use nonoclaw_core::CacheControl;

use crate::context::{SystemContext, UserContext};

const PLATFORM_HINT: &str = "Linux";

/// Build the `system` array for the API request. Returns two blocks: the main
/// prompt (with a cache breakpoint) and the project/memory context.
pub fn build_system_blocks(
    cwd: &std::path::Path,
    system: &SystemContext,
    user: &UserContext,
    memory: &Option<String>,
    tool_prompts: &[(String, String)],
    append: &Option<String>,
) -> Vec<SystemBlock> {
    let mut main = String::new();
    main.push_str(BASE);
    main.push_str(&format!(
        "\n# Environment\n- Working directory: {}\n",
        cwd.display()
    ));
    main.push_str(&format!("- Platform: {PLATFORM_HINT}\n"));
    main.push_str(&format!("- Today's date: {}\n", user.date));
    if !system.git_summary.is_empty() {
        main.push_str("\n# Git\n```\n");
        main.push_str(&system.git_summary);
        main.push_str("```\n");
    }
    main.push_str(TOOL_GUIDANCE);
    for (name, prompt) in tool_prompts {
        main.push_str(&format!("\n## Tool: {name}\n{prompt}\n"));
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

    let mut context = String::new();
    if !user.nonoclaw_md.is_empty() {
        context.push_str(&user.nonoclaw_md);
    }
    if let Some(mem) = memory {
        context.push_str("# Memory\n\n");
        context.push_str(mem);
        context.push('\n');
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

const BASE: &str = "You are NonoClaw, an interactive command-line coding agent. You help users with software engineering tasks by reading, editing, searching, and running code, and by answering questions about the codebase.\n\nYou operate in an agentic loop: think, use tools, observe results, and continue until the task is done. Prefer dedicated tools over raw shell commands. Reference files as path:line when relevant. Match the surrounding code's style. For hard-to-reverse or outward-facing actions, confirm first.";

const TOOL_GUIDANCE: &str = "\n# Tools\nUse tools to gather information and make changes. Read files before editing them. Make edits with the smallest sufficient context so they are unambiguous. Run shell commands for tasks no dedicated tool covers, and truncate/inspect large outputs rather than dumping them. When a task needs multiple independent lookups, issue them together.";
