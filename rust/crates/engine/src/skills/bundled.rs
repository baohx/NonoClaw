//! Bundled (built-in) skills embedded at compile time via `include_str!()`.
//! Mirrors CC's `src/skills/bundled/`. Each skill lives in a sibling `.md` file
//! with YAML frontmatter compatible with [`super::parse_skill_str`].

use super::{parse_skill_str, Skill};

/// Register all bundled skills into `static_skills`. Called from
/// [`SkillsManager::new`].
pub fn register_bundled(skills: &mut Vec<Skill>) {
    // Each entry: (file_content, fallback_name).
    let defs: &[(&str, &str)] = &[
        (include_str!("bundled/verify.md"), "verify"),
        (include_str!("bundled/simplify.md"), "simplify"),
        (include_str!("bundled/debug.md"), "debug"),
        (include_str!("bundled/remember.md"), "remember"),
        (include_str!("bundled/loop.md"), "loop"),
        (include_str!("bundled/update-config.md"), "update-config"),
        (
            include_str!("bundled/keybindings-help.md"),
            "keybindings-help",
        ),
        (include_str!("bundled/claude-api.md"), "claude-api"),
        (include_str!("bundled/code-review.md"), "code-review"),
        (include_str!("bundled/init.md"), "init"),
        (include_str!("bundled/review.md"), "review"),
        (
            include_str!("bundled/security-review.md"),
            "security-review",
        ),
    ];

    for (content, fallback) in defs {
        let source = format!("bundled:{fallback}");
        if let Some(skill) = parse_skill_str(content, Some(fallback), &source) {
            skills.push(skill);
        }
    }
}
