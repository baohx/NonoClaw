//! Prompt templates: lightweight slash-command expansion backed by
//! `.nonoclaw/prompts/*.md` files.
//!
//! A template file `pr.md` becomes the slash command `/pr`. When the user
//! types `/pr 123 456`, the template body is expanded with positional
//! parameters and returned as the run's user prompt.
//!
//! # Template syntax
//!
//! - `$1`, `$2`, ... — positional parameter (1-based).
//! - `$@` or `$ARGUMENTS` — all parameters joined by a single space.
//! - `${N:-default}` — positional parameter with a default when missing.
//! - `${@:N}` — bash-style slice: parameters starting at position N.
//!
//! # File discovery
//!
//! Templates are loaded from (in order, project wins over user):
//!   1. `<cwd>/.nonoclaw/prompts/*.md`
//!   2. `~/.nonoclaw/prompts/*.md`
//!
//! An optional `argument-hint:` frontmatter line is preserved as template
//! metadata; the rest of the file is the template body.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A single loaded template.
#[derive(Debug, Clone)]
pub struct PromptTemplate {
    /// Template name (the slash command without the leading `/`).
    pub name: String,
    /// Optional hint shown in help / completion.
    pub argument_hint: Option<String>,
    /// Raw template body with `$` placeholders intact.
    pub body: String,
    /// Source path the template was loaded from.
    pub source: PathBuf,
}

/// Registry of templates for the current project + user.
#[derive(Debug, Default)]
pub struct PromptTemplateRegistry {
    templates: HashMap<String, PromptTemplate>,
}

impl PromptTemplateRegistry {
    /// Discover templates for the given cwd, merging project-local and
    /// user-global directories. Project templates shadow user templates
    /// with the same name.
    pub fn discover(cwd: &Path) -> Self {
        let mut templates: HashMap<String, PromptTemplate> = HashMap::new();
        // User-global first so project templates override.
        if let Some(home) = nonoclaw_core::nonoclaw_data_dir() {
            Self::load_dir(&PathBuf::from(home).join(".nonoclaw/prompts"), &mut templates);
        }
        Self::load_dir(&cwd.join(".nonoclaw/prompts"), &mut templates);
        Self { templates }
    }

    fn load_dir(dir: &Path, out: &mut HashMap<String, PromptTemplate>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (argument_hint, body) = parse_frontmatter(&raw);
            out.insert(
                name.to_string(),
                PromptTemplate {
                    name: name.to_string(),
                    argument_hint,
                    body,
                    source: path.clone(),
                },
            );
        }
    }

    /// Look up a template by slash-command name.
    pub fn get(&self, name: &str) -> Option<&PromptTemplate> {
        self.templates.get(name)
    }

    /// Iterate over all templates (sorted by name for stable display).
    pub fn list(&self) -> Vec<&PromptTemplate> {
        let mut v: Vec<&PromptTemplate> = self.templates.values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Expand a `/name arg1 arg2 ...` invocation. Returns `None` when the
    /// name is unknown so the caller can fall back to its default
    /// slash-command handling.
    pub fn expand(&self, name: &str, args: &str) -> Option<String> {
        let template = self.get(name)?;
        Some(expand_body(&template.body, args))
    }
}

/// Parse an optional `argument-hint: ...` frontmatter line. The remainder of
/// the file (after the frontmatter block, if any) is returned as the body.
fn parse_frontmatter(raw: &str) -> (Option<String>, String) {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        return (None, raw.to_string());
    }
    let after_open = &trimmed[3..];
    let Some(close_pos) = after_open.find("\n---") else {
        return (None, raw.to_string());
    };
    let fm = &after_open[..close_pos];
    let body = after_open[close_pos + 4..].trim_start_matches(['\n', ' ']).to_string();
    let mut hint = None;
    for line in fm.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("argument-hint:") {
            hint = Some(rest.trim().trim_matches('"').to_string());
        }
    }
    (hint, body)
}

/// Expand `$` placeholders in `body` against the whitespace-split args.
pub fn expand_body(body: &str, args: &str) -> String {
    let args_vec: Vec<&str> = args.split_whitespace().collect();
    let mut out = String::with_capacity(body.len() + args.len());
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c != '$' {
            out.push(c);
            i += 1;
            continue;
        }
        // $$ → literal $
        if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
            out.push('$');
            i += 2;
            continue;
        }
        // $@ / $ARGUMENTS → all args
        if body[i..].starts_with("$ARGUMENTS") {
            out.push_str(&args_vec.join(" "));
            i += "$ARGUMENTS".len();
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1] == b'@' {
            out.push_str(&args_vec.join(" "));
            i += 2;
            continue;
        }
        // ${N:-default}
        if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(close) = body[i..].find('}') {
                let inner = &body[i + 2..i + close];
                // ${@:N} — slice args from position N (1-based)
                if let Some(rest) = inner.strip_prefix("@:") {
                    if let Ok(n) = rest.trim().parse::<usize>() {
                        let start = n.saturating_sub(1).min(args_vec.len());
                        out.push_str(&args_vec[start..].join(" "));
                        i += close + 1;
                        continue;
                    }
                }
                // ${N:-default}
                if let Some((num_str, default)) = inner.split_once(":-") {
                    if let Ok(n) = num_str.trim().parse::<usize>() {
                        let v = args_vec.get(n.saturating_sub(1)).copied().unwrap_or(default);
                        out.push_str(v);
                        i += close + 1;
                        continue;
                    }
                }
                // ${N} — same as $N but brace form
                if let Ok(n) = inner.trim().parse::<usize>() {
                    if let Some(v) = args_vec.get(n.saturating_sub(1)) {
                        out.push_str(v);
                    }
                    i += close + 1;
                    continue;
                }
            }
        }
        // $N — positional
        if i + 1 < bytes.len() && (bytes[i + 1] as char).is_ascii_digit() {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] as char).is_ascii_digit() {
                j += 1;
            }
            if let Ok(n) = body[i + 1..j].parse::<usize>() {
                if let Some(v) = args_vec.get(n.saturating_sub(1)) {
                    out.push_str(v);
                }
                i = j;
                continue;
            }
        }
        // Lone $ — keep as-is
        out.push('$');
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_positional_params() {
        assert_eq!(expand_body("Review $1 and $2", "123 456"), "Review 123 and 456");
        assert_eq!(expand_body("Review $1", "only"), "Review only");
        // Missing positional → empty.
        assert_eq!(expand_body("Review $2", "only"), "Review ");
    }

    #[test]
    fn expand_at_sign_joins_all_args() {
        assert_eq!(expand_body("Focus on $@.", "a b c"), "Focus on a b c.");
        assert_eq!(expand_body("Focus on $ARGUMENTS.", "a b c"), "Focus on a b c.");
    }

    #[test]
    fn expand_default_value() {
        assert_eq!(
            expand_body("Env: ${2:-dev}", "deploy"),
            "Env: dev"
        );
        assert_eq!(
            expand_body("Env: ${2:-dev}", "deploy prod"),
            "Env: prod"
        );
    }

    #[test]
    fn expand_slice_syntax() {
        assert_eq!(
            expand_body("Rest: ${@:2}", "a b c d"),
            "Rest: b c d"
        );
        assert_eq!(
            expand_body("Rest: ${@:3}", "a b c d"),
            "Rest: c d"
        );
    }

    #[test]
    fn expand_dollar_dollar_is_literal() {
        assert_eq!(expand_body("Cost: $$5", ""), "Cost: $5");
    }

    #[test]
    fn frontmatter_argument_hint_extracted() {
        let raw = "---\nargument-hint: \"issue numbers\"\n---\nReview $@.\n";
        let (hint, body) = parse_frontmatter(raw);
        assert_eq!(hint.as_deref(), Some("issue numbers"));
        assert_eq!(body, "Review $@.\n");
    }

    #[test]
    fn no_frontmatter_returns_raw() {
        let raw = "Just a plain template $1.";
        let (hint, body) = parse_frontmatter(raw);
        assert!(hint.is_none());
        assert_eq!(body, raw);
    }

    #[test]
    fn registry_discovers_and_expands() {
        use std::io::Write;
        let tmp = std::env::temp_dir().join(format!("nc_tpl_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".nonoclaw/prompts")).unwrap();
        let mut f = std::fs::File::create(tmp.join(".nonoclaw/prompts/pr.md")).unwrap();
        writeln!(f, "Review PR #${{1}}").unwrap();
        drop(f);

        let reg = PromptTemplateRegistry::discover(&tmp);
        let t = reg.get("pr").expect("template must be discovered");
        assert_eq!(t.name, "pr");
        let expanded = reg.expand("pr", "123").expect("expand must succeed");
        assert!(expanded.contains("Review PR #123"), "got: {expanded}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn unknown_template_returns_none() {
        let reg = PromptTemplateRegistry::default();
        assert!(reg.expand("nope", "args").is_none());
    }
}
