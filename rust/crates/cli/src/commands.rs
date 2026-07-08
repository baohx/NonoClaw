//! Slash-command metadata + help text. Mirrors the role of `src/commands/` +
//! `src/commands.ts` (built-in local commands).
//!
//! Consumed by the web frontend Phase 1 (HTTP/WS server).
#![allow(dead_code)]

use crate::skills::Skill;

/// Built-in slash commands: (name, one-line description).
pub const BUILTINS: &[(&str, &str)] = &[
    ("clear", "Clear conversation history (frees context)"),
    (
        "compact",
        "Summarize the older transcript into one message now",
    ),
    ("cost", "Show token usage so far"),
    ("tools", "List available tools"),
    ("sessions", "List stored sessions for this directory"),
    ("help", "Show this help"),
    ("quit", "Exit NonoClaw"),
];

/// Render the `/help` text: builtins + discovered skills.
pub fn help_text(skills: &[Skill]) -> String {
    let mut out = String::from("Slash commands:\n");
    for (name, desc) in BUILTINS {
        out.push_str(&format!("  /{name:<10} {desc}\n"));
    }
    if !skills.is_empty() {
        out.push_str("\nSkills (type /<name> to inject the skill's instructions):\n");
        for s in skills {
            out.push_str(&format!("  /{:<10} {}\n", s.name, s.description));
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_lists_builtins() {
        let h = help_text(&[]);
        assert!(h.contains("/clear"));
        assert!(h.contains("/compact"));
        assert!(h.contains("/help"));
    }

    #[test]
    fn help_lists_skills() {
        let s = vec![Skill {
            name: "deploy".into(),
            description: "deploys the app".into(),
            body: String::new(),
            source: String::new(),
        }];
        let h = help_text(&s);
        assert!(h.contains("/deploy"));
        assert!(h.contains("deploys the app"));
    }
}
