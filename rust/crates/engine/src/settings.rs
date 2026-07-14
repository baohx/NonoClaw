//! Settings file loading and merging. Mirrors the layered settings system in
//! `src/utils/settings/` (`settings.ts`, `types.ts`). Priority (low→high):
//!
//!   user   `~/.nonoclaw/settings.json`           (or `$NONOCLAW_HOME/settings.json`)
//!   → project `<cwd>/.nonoclaw/settings.json`
//!   → local  `<cwd>/.nonoclaw/settings.local.json`  (gitignored)
//!   → flag   `--settings <path>`                 (explicit file)
//!
//! Arrays (e.g. `permissions.allow`) are concatenated and deduplicated. Objects
//! are deep-merged (later source wins on scalar keys). This matches lodash
//! `mergeWith` with the `settingsMergeCustomizer` from the TS reference.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nonoclaw_api::ThinkingConfig;
use nonoclaw_core::PermissionMode;
use nonoclaw_tools::McpServerConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::EngineOptions;

/// A single settings source file's content (all fields optional + passthrough).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SettingsFile {
    pub model: Option<String>,
    #[serde(rename = "maxTurns")]
    pub max_turns: Option<u32>,
    #[serde(rename = "maxTokens")]
    pub max_tokens: Option<u32>,
    #[serde(rename = "autoCompact")]
    pub auto_compact: Option<bool>,
    #[serde(rename = "compactThreshold")]
    pub compact_threshold: Option<usize>,
    /// Model context-window size in tokens. When set, drives the auto-compact
    /// threshold (window − maxTokens − safety margin) unless `compactThreshold`
    /// is given explicitly. Use this for models whose window differs from the
    /// default assumption (e.g. deepseek-chat ~64k/128k vs Claude 200k).
    #[serde(rename = "contextWindow")]
    pub context_window: Option<usize>,
    pub thinking: Option<Value>,
    pub permissions: Option<PermissionsSection>,
    #[serde(default)]
    pub hooks: Option<Value>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: Option<HashMap<String, McpServerConfig>>,
    /// Pre-defined model profiles for multi-model switching. Each profile
    /// carries its own base_url + api_key so different providers can be used.
    #[serde(default)]
    pub models: Option<Vec<ModelProfile>>,
    /// Document processing model for file attachment extraction.
    #[serde(rename = "docModel", default)]
    pub doc_model: Option<DocModelConfig>,
    // Passthrough: preserve unknown fields.
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// A model profile: name + endpoint + credentials, for multi-model switching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfile {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    #[serde(rename = "apiKey")]
    pub api_key: String,
    #[serde(default)]
    pub default: bool,
}

/// Document processing model config. When set, uploaded files (PDF, DOCX, images)
/// are routed through a multimodal model for content extraction instead of using
/// traditional OCR/text-extraction libraries.
///
/// Configured in settings.json under `docModel`:
/// ```json
/// { "docModel": { "provider": "mistral_ocr", "model": "mistral-ocr-latest",
///   "baseUrl": "https://api.mistral.ai", "apiKey": "$MISTRAL_API_KEY" } }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocModelConfig {
    /// Provider backend: "mistral_ocr", "gemini", "generic_vision", or "none".
    pub provider: String,
    /// Model id (e.g. "mistral-ocr-latest", "gemini-3.5-flash", "gpt-4o").
    pub model: String,
    /// API base URL for the provider.
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    /// API key (supports $ENV_VAR substitution).
    #[serde(rename = "apiKey")]
    pub api_key: String,
}

impl DocModelConfig {
    /// Resolve `$VAR` references in api_key against the process environment.
    pub fn resolved_api_key(&self) -> String {
        resolve_env_var(&self.api_key)
    }

    /// Is document processing enabled?
    pub fn is_enabled(&self) -> bool {
        !self.provider.is_empty()
            && self.provider != "none"
            && !self.api_key.is_empty()
            && !self.base_url.is_empty()
    }
}

fn resolve_env_var(raw: &str) -> String {
    if raw.starts_with('$') {
        let var = &raw[1..];
        std::env::var(var).unwrap_or_else(|_| raw.to_string())
    } else {
        raw.to_string()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionsSection {
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    #[serde(default)]
    pub deny: Option<Vec<String>>,
    #[serde(rename = "defaultMode")]
    pub default_mode: Option<String>,
}

/// Resolve `$NONOCLAW_HOME` or `~/.nonoclaw`.
pub fn nonoclaw_config_dir() -> Option<PathBuf> {
    nonoclaw_core::nonoclaw_data_dir()
}

/// Load user-level `settings.json` if it exists.
pub fn load_user_settings() -> Option<SettingsFile> {
    let path = nonoclaw_config_dir()?.join("settings.json");
    read_settings_file(&path)
}

/// Load project-level `settings.json` if it exists.
pub fn load_project_settings(cwd: &Path) -> Option<SettingsFile> {
    read_settings_file(&cwd.join(".nonoclaw").join("settings.json"))
}

/// Load project-local `settings.local.json` if it exists.
pub fn load_local_settings(cwd: &Path) -> Option<SettingsFile> {
    read_settings_file(&cwd.join(".nonoclaw").join("settings.local.json"))
}

/// Load an arbitrary settings file (from `--settings` flag).
pub fn load_flag_settings(path: &Path) -> Option<SettingsFile> {
    read_settings_file(path)
}

fn read_settings_file(path: &Path) -> Option<SettingsFile> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<SettingsFile>(&text).ok()
}

/// Merge `source` into `base` in-place (deep merge; arrays concatenate).
/// Mirrors the `settingsMergeCustomizer` + lodash `mergeWith` behaviour.
pub fn merge_settings(base: &mut SettingsFile, overlay: &SettingsFile) {
    if overlay.model.is_some() {
        base.model = overlay.model.clone();
    }
    if overlay.max_turns.is_some() {
        base.max_turns = overlay.max_turns;
    }
    if overlay.max_tokens.is_some() {
        base.max_tokens = overlay.max_tokens;
    }
    if overlay.auto_compact.is_some() {
        base.auto_compact = overlay.auto_compact;
    }
    if overlay.compact_threshold.is_some() {
        base.compact_threshold = overlay.compact_threshold;
    }
    if overlay.context_window.is_some() {
        base.context_window = overlay.context_window;
    }
    if overlay.thinking.is_some() {
        base.thinking = overlay.thinking.clone();
    }
    if let Some(hooks) = &overlay.hooks {
        base.hooks = Some(deep_merge_values(base.hooks.as_ref(), hooks));
    }
    // Permissions: merge allow/deny arrays (concatenate + dedup) and
    // defaultMode scalar.
    if let Some(perms) = &overlay.permissions {
        let b = base.permissions.get_or_insert_with(Default::default);
        if let Some(ref a) = perms.allow {
            let mut merged = b.allow.take().unwrap_or_default();
            merged.extend(a.clone());
            merged.sort();
            merged.dedup();
            b.allow = Some(merged);
        }
        if let Some(ref d) = perms.deny {
            let mut merged = b.deny.take().unwrap_or_default();
            merged.extend(d.clone());
            merged.sort();
            merged.dedup();
            b.deny = Some(merged);
        }
        if perms.default_mode.is_some() {
            b.default_mode = perms.default_mode.clone();
        }
    }
    // env: shallow merge (later wins per-key).
    if let Some(ref env) = overlay.env {
        let b = base.env.get_or_insert_with(Default::default);
        for (k, v) in env {
            b.insert(k.clone(), v.clone());
        }
    }
    // mcpServers: merge per-key (later wins).
    if let Some(ref mcp) = overlay.mcp_servers {
        let b = base.mcp_servers.get_or_insert_with(Default::default);
        for (k, v) in mcp {
            b.insert(k.clone(), v.clone());
        }
    }
    // models: later overlay replaces the entire array.
    if overlay.models.is_some() {
        base.models = overlay.models.clone();
    }
    // docModel: later overlay replaces.
    if overlay.doc_model.is_some() {
        base.doc_model = overlay.doc_model.clone();
    }
    // passthrough extras: overwrite matching keys.
    for (k, v) in &overlay.extra {
        base.extra.insert(k.clone(), v.clone());
    }
}

fn deep_merge_values(base: Option<&Value>, overlay: &Value) -> Value {
    let Some(base) = base else {
        return overlay.clone();
    };
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            let mut m = b.clone();
            for (k, v) in o {
                m.insert(k.clone(), v.clone());
            }
            Value::Object(m)
        }
        _ => overlay.clone(),
    }
}

/// Load and merge settings from all layers. Returns the merged [`SettingsFile`].
pub fn load_settings(cwd: &Path, flag_path: Option<&Path>) -> SettingsFile {
    let mut merged = SettingsFile::default();
    if let Some(u) = load_user_settings() {
        merge_settings(&mut merged, &u);
    }
    if let Some(p) = load_project_settings(cwd) {
        merge_settings(&mut merged, &p);
    }
    if let Some(l) = load_local_settings(cwd) {
        merge_settings(&mut merged, &l);
    }
    if let Some(path) = flag_path {
        if let Some(f) = load_flag_settings(path) {
            merge_settings(&mut merged, &f);
        }
    }
    // Standalone .mcp.json in cwd (last, so it overrides settings mcpServers per-key).
    if let Some(mcp) = load_mcp_json(cwd) {
        let overlay = SettingsFile {
            mcp_servers: Some(mcp),
            ..Default::default()
        };
        merge_settings(&mut merged, &overlay);
    }
    merged
}

/// Load the standalone `.mcp.json` file's `mcpServers` map.
fn load_mcp_json(cwd: &Path) -> Option<HashMap<String, McpServerConfig>> {
    let path = cwd.join(".nonoclaw").join("mcp.json");
    let text = std::fs::read_to_string(&path).ok()?;
    #[derive(Deserialize)]
    struct McpFile {
        #[serde(rename = "mcpServers", default)]
        mcp_servers: HashMap<String, McpServerConfig>,
    }
    serde_json::from_str::<McpFile>(&text)
        .ok()
        .map(|f| f.mcp_servers)
}

/// Inject `settings.env` into the process environment (mirrors TS: settings.ts
/// sets env vars from the merged settings so they're available to child tools).
pub fn apply_env(merged: &SettingsFile) {
    if let Some(env) = &merged.env {
        for (k, v) in env {
            // Don't overwrite already-set env vars (CLI > env > settings).
            if std::env::var_os(k).is_none() {
                std::env::set_var(k, v);
            }
        }
    }
}

/// Apply merged settings to an [`EngineOptions`] in-place. The caller should
/// build options from CLI flags first, then call this so settings fill gaps.
pub fn apply_settings(options: &mut EngineOptions, merged: &SettingsFile) {
    if let Some(model) = &merged.model {
        options.model.clone_from(model);
    }
    if let Some(mt) = merged.max_turns {
        options.max_turns = mt;
    }
    if let Some(mt) = merged.max_tokens {
        options.max_tokens = mt;
    }
    if let Some(ac) = merged.auto_compact {
        options.auto_compact = ac;
    }
    if let Some(ct) = merged.compact_threshold {
        options.compact_threshold_tokens = ct;
    }
    if let Some(think) = &merged.thinking {
        if let Ok(cfg) = serde_json::from_value::<ThinkingConfig>(think.clone()) {
            options.thinking = Some(cfg);
        }
    }
    if let Some(perms) = &merged.permissions {
        if let Some(ref mode_str) = perms.default_mode {
            if let Some(m) = PermissionMode::from_kebab(mode_str) {
                options.permission_mode = m;
            }
        }
        if let Some(ref a) = perms.allow {
            options.allowed_tools.clone_from(a);
        }
        if let Some(ref d) = perms.deny {
            options.disallowed_tools.clone_from(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_overwrites_scalar() {
        let mut base = SettingsFile {
            model: Some("sonnet".into()),
            ..Default::default()
        };
        let overlay = SettingsFile {
            model: Some("opus".into()),
            ..Default::default()
        };
        merge_settings(&mut base, &overlay);
        assert_eq!(base.model.as_deref(), Some("opus"));
    }

    #[test]
    fn merge_concatenates_arrays() {
        let mut base = SettingsFile {
            permissions: Some(PermissionsSection {
                allow: Some(vec!["Read".into()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let overlay = SettingsFile {
            permissions: Some(PermissionsSection {
                allow: Some(vec!["Read".into(), "Bash".into()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        merge_settings(&mut base, &overlay);
        let p = base.permissions.unwrap();
        let allow = p.allow.unwrap();
        assert!(allow.contains(&"Read".to_string()));
        assert!(allow.contains(&"Bash".to_string()));
        assert_eq!(allow.len(), 2); // deduped
    }

    #[test]
    fn load_and_merge_chain() {
        let dir = tempdir();
        let user = dir.join("home");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(
            user.join("settings.json"),
            r#"{"model":"sonnet","maxTurns":5}"#,
        )
        .unwrap();
        let cwd = dir.join("proj");
        std::fs::create_dir_all(cwd.join(".nonoclaw")).unwrap();
        std::fs::write(
            cwd.join(".nonoclaw/settings.json"),
            r#"{"maxTurns":10,"maxTokens":4096}"#,
        )
        .unwrap();
        std::fs::write(
            cwd.join(".nonoclaw/settings.local.json"),
            r#"{"maxTokens":8192}"#,
        )
        .unwrap();

        std::env::set_var("NONOCLAW_HOME", &user);
        let merged = load_settings(&cwd, None);
        let mut opts = EngineOptions::default();
        apply_settings(&mut opts, &merged);
        // model from user
        assert_eq!(merged.model.as_deref(), Some("sonnet"));
        assert_eq!(opts.model, "sonnet");
        // maxTurns from project (overrides user)
        assert_eq!(opts.max_turns, 10);
        // maxTokens from local (overrides project)
        assert_eq!(opts.max_tokens, 8192);
        std::env::remove_var("NONOCLAW_HOME");
    }

    fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("nonoclaw-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
