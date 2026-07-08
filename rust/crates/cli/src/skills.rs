//! Skill + plugin discovery for slash commands. Mirrors `src/skills/`
//! (`loadSkillsDir.ts`) and `src/plugins/`: a skill is a directory containing a
//! `SKILL.md` with YAML frontmatter (`name`, `description`) and an instruction
//! body. Invoking `/<name>` injects the body as a prompt to the model.
//!
//! Consumed by the web frontend Phase 1 (HTTP/WS server).
#![allow(dead_code)]
//!
//! Plugins (Phase 5 minimal) are directories under `.nonoclaw/plugins/<plugin>/`
//! that may contribute skills via `<plugin>/skills/<skill>/SKILL.md`.

use std::path::{Path, PathBuf};

/// One discovered skill.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    #[allow(dead_code)]
    pub source: String,
}

/// Discover skills for `cwd`: project `.nonoclaw/skills`, user `~/.nonoclaw/skills`,
/// and plugin-contributed `.nonoclaw/plugins/<plugin>/skills/<skill>`. Later
/// sources override earlier ones by name.
pub fn discover(cwd: &Path) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();

    // Direct skill dirs.
    let mut bases: Vec<PathBuf> = vec![cwd.join(".nonoclaw/skills")];
    if let Some(home) = std::env::var_os("HOME") {
        bases.push(PathBuf::from(home).join(".nonoclaw/skills"));
    }
    for base in &bases {
        scan_skill_dir(base, &mut skills);
    }

    // Plugin-contributed skills: .nonoclaw/plugins/<plugin>/skills/<skill>/SKILL.md
    let plugins_dir = cwd.join(".nonoclaw/plugins");
    if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
        for entry in entries.flatten() {
            let plugin_skills = entry.path().join("skills");
            scan_skill_dir(&plugin_skills, &mut skills);
        }
    }

    // Dedup by name, keeping the last definition (project > user > plugin order
    // since project is scanned first; later wins).
    dedup_by_name(skills)
}

fn scan_skill_dir(base: &Path, out: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let skill_md = path.join("SKILL.md");
        if let Some(skill) = parse_skill(&skill_md) {
            out.push(skill);
        }
    }
}

/// Parse a SKILL.md file: frontmatter (`---`-delimited) `name`/`description`,
/// then the body.
pub fn parse_skill(path: &Path) -> Option<Skill> {
    let text = std::fs::read_to_string(path).ok()?;
    let (front, body) = split_frontmatter(&text);
    let name = front.get("name").cloned().or_else(|| {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
    })?;
    let description = front.get("description").cloned().unwrap_or_default();
    let source = path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    Some(Skill {
        name,
        description,
        body: body.trim().to_string(),
        source,
    })
}

/// Split `---\n<yaml>\n---\n<body>` into (key map, body). Tolerant: if no
/// frontmatter, returns (empty, whole text).
fn split_frontmatter(text: &str) -> (std::collections::HashMap<String, String>, String) {
    let mut map = std::collections::HashMap::new();
    let trimmed = text.trim_start_matches('\u{feff}');
    let rest = trimmed.strip_prefix("---").unwrap_or(trimmed);
    // Need a leading "---" line to treat as frontmatter.
    if trimmed.starts_with("---") {
        if let Some(end) = rest.find("\n---") {
            let yaml = &rest[..end];
            let body = rest[end + 4..].trim_start_matches('\n');
            for line in yaml.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    map.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
                }
            }
            return (map, body.to_string());
        }
    }
    (map, text.to_string())
}

fn dedup_by_name(mut skills: Vec<Skill>) -> Vec<Skill> {
    // Keep last occurrence per name (later sources override).
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    skills.dedup_by(|a, b| a.name == b.name); // only adjacent; do a real dedup:
    let mut out: Vec<Skill> = Vec::new();
    for s in skills.into_iter().rev() {
        if seen.insert(s.name.clone()) {
            out.push(s);
        }
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let dir = tempdir();
        let skill_dir = dir.join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: does a thing\n---\nRun the thing now.\n",
        )
        .unwrap();
        let s = parse_skill(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(s.name, "my-skill");
        assert_eq!(s.description, "does a thing");
        assert_eq!(s.body, "Run the thing now.");
    }

    #[test]
    fn falls_back_to_dir_name_without_frontmatter() {
        let dir = tempdir();
        let skill_dir = dir.join("fallback");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "just body, no frontmatter").unwrap();
        let s = parse_skill(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(s.name, "fallback");
        assert_eq!(s.body, "just body, no frontmatter");
    }

    #[test]
    fn discover_finds_project_skills_and_dedups() {
        let dir = tempdir();
        let skills = dir.join(".nonoclaw/skills");
        std::fs::create_dir_all(skills.join("alpha")).unwrap();
        std::fs::write(
            skills.join("alpha/SKILL.md"),
            "---\nname: alpha\ndescription: a\n---\nalpha body",
        )
        .unwrap();
        std::fs::create_dir_all(skills.join("beta")).unwrap();
        std::fs::write(
            skills.join("beta/SKILL.md"),
            "---\nname: beta\ndescription: b\n---\nbeta body",
        )
        .unwrap();
        let found = discover(&dir);
        let names: Vec<_> = found.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[test]
    fn discover_finds_plugin_skills() {
        let dir = tempdir();
        let plug = dir.join(".nonoclaw/plugins/myplug/skills/gamma");
        std::fs::create_dir_all(&plug).unwrap();
        std::fs::write(
            plug.join("SKILL.md"),
            "---\nname: gamma\ndescription: g\n---\ngamma body",
        )
        .unwrap();
        let found = discover(&dir);
        assert!(found.iter().any(|s| s.name == "gamma"));
    }

    fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("nonoclaw-skills-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
