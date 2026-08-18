//! Canonical configuration loading, merging, validation, and provenance.
//!
//! File I/O is confined to [`load_resolved_config`]. The merge itself is the
//! pure [`resolve_layers`] function, making precedence and collection rules
//! deterministic and directly testable.
//!
//! Precedence (low to high): user settings, project settings, project-local
//! settings, explicit `--settings`, standalone project MCP, explicit
//! `--mcp-config`, then per-run CLI/Web overrides.
//!
//! Merge rules:
//! - scalar values: highest-precedence present value wins;
//! - `permissions.allow` / `permissions.deny`: concatenate, sort, and dedupe;
//! - `models`: highest-precedence complete array replaces the prior array;
//! - `hooks`: recursively merge objects, concatenate/dedupe arrays, replace scalars;
//! - `mcpServers`: merge by server name, replacing a complete server on conflict;
//! - `env`: merge by variable name; process environment wins over file values.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nonoclaw_api::{
    ApiFormat, Client, ClientConfig, ClientFactory, ClientPurpose, ThinkingConfig, DEFAULT_BASE_URL,
};
use nonoclaw_core::{display_path, PermissionMode, RunEvent, TechnicalStatus};
use nonoclaw_tools::McpServerConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agents::AgentProfile;
use crate::budget::{ContextBudget, ContextBudgetSettings, TokenMode};
use crate::EngineOptions;

pub const DEFAULT_MODEL: &str = "claude-sonnet-4-5-20250929";
pub const DEFAULT_MAX_TURNS: u32 = 200;
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
pub const DEFAULT_COMPACT_THRESHOLD: usize = 150_000;
const COMPACT_SAFETY_MARGIN: usize = 2048;

/// Shared settings metadata used by diagnostics, the Web Insight reference,
/// and user-facing documentation. Values and credentials are never included.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ConfigFieldReference {
    pub name: &'static str,
    pub description: &'static str,
}

pub const CONFIG_REFERENCE: &[ConfigFieldReference] = &[
    ConfigFieldReference {
        name: "model",
        description: "Default conversation model name.",
    },
    ConfigFieldReference {
        name: "maxTurns",
        description: "Maximum agent turns per run.",
    },
    ConfigFieldReference {
        name: "maxTokens",
        description: "Maximum model output tokens per turn.",
    },
    ConfigFieldReference {
        name: "autoCompact",
        description: "Enable automatic transcript compaction.",
    },
    ConfigFieldReference {
        name: "autoSelectMcp",
        description: "Narrow advertised MCP tools to a keyword-relevant subset per run.",
    },
    ConfigFieldReference {
        name: "autoSelectMcpTopK",
        description: "Legacy MCP relevance cap; toolAutoSelectTopK takes precedence.",
    },
    ConfigFieldReference {
        name: "toolAutoSelectTopK",
        description: "Maximum intent-selected non-core tool schemas. Default 5.",
    },
    ConfigFieldReference {
        name: "coreTools",
        description: "Tool schemas that remain visible on every model request.",
    },
    ConfigFieldReference {
        name: "mcpNoMatchPolicy",
        description: "MCP fallback when no tool matches: none (default), safe, or all.",
    },
    ConfigFieldReference {
        name: "mcpSafeTools",
        description: "Exact MCP tool names exposed by the safe no-match policy.",
    },
    ConfigFieldReference {
        name: "compactThreshold",
        description: "Estimated-token threshold for automatic compaction.",
    },
    ConfigFieldReference {
        name: "contextWindow",
        description: "Global model context-window size in tokens.",
    },
    ConfigFieldReference {
        name: "thinking",
        description: "Provider thinking configuration.",
    },
    ConfigFieldReference {
        name: "permissions",
        description: "Tool allow/deny rules and defaultMode.",
    },
    ConfigFieldReference {
        name: "hooks",
        description: "Hook configuration merged with discovered hook files.",
    },
    ConfigFieldReference {
        name: "env",
        description: "Environment inputs; existing process values take precedence.",
    },
    ConfigFieldReference {
        name: "mcpServers",
        description: "Named MCP stdio server configurations.",
    },
    ConfigFieldReference {
        name: "models",
        description: "Named model profiles and role assignments.",
    },
    ConfigFieldReference {
        name: "compactModel",
        description: "Model profile used for transcript compaction.",
    },
    ConfigFieldReference {
        name: "compactMaxTokens",
        description: "Cap on the compaction summarizer's output length (tokens). Default 4096.",
    },
    ConfigFieldReference {
        name: "tokenMode",
        description: "Payload preset: standard (default) or ultra. Explicit settings override preset defaults.",
    },
    ConfigFieldReference {
        name: "contextBudget",
        description: "Per-partition token caps for system prompt, schemas, skills, project rules, memory (with independent beads/facts/wiki/index sub-partitions), git, tool results, history, and attachments.",
    },
    ConfigFieldReference {
        name: "promptProfile",
        description: "System-prompt section profile: \"full\", \"minimal\", or \"ultra\".",
    },
    ConfigFieldReference {
        name: "skillDisclosure",
        description: "Static skill prompt policy: \"all\", \"index\" (default), or \"search\".",
    },
    ConfigFieldReference {
        name: "skillIndexMaxTokens",
        description: "Maximum estimated tokens for the static skill index. Default 500.",
    },
    ConfigFieldReference {
        name: "elevenlabsApiKey",
        description: "Server-side speech-to-text credential or environment reference.",
    },
    ConfigFieldReference {
        name: "charsPerToken",
        description: "Global token-estimation divisor.",
    },
    ConfigFieldReference {
        name: "executables",
        description: "Explicit Rust, Node.js, and Python executable paths and versions.",
    },
    ConfigFieldReference {
        name: "docModel",
        description: "Document/OCR model name or inline configuration.",
    },
    ConfigFieldReference {
        name: "attachmentConverter",
        description: "Document conversion strategy for file uploads: \"auto\", \"markitdown\", or \"legacy\".",
    },
    ConfigFieldReference {
        name: "providerBilling",
        description: "Provider API balance-check endpoints: { providers: { name: { balanceUrl, apiKey } } }.",
    },
];

pub fn config_reference() -> &'static [ConfigFieldReference] {
    CONFIG_REFERENCE
}

/// A single settings source file's content. Unknown top-level fields are kept
/// in `extra` so they can be diagnosed without discarding otherwise-valid data.
#[derive(Clone, Serialize, Deserialize)]
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
    /// Narrow advertised MCP tools to a keyword-relevant subset per run.
    /// Defaults to true when unset.
    #[serde(rename = "autoSelectMcp", default)]
    pub auto_select_mcp: Option<bool>,
    /// Cap on advertised MCP tools once narrowing applies.
    #[serde(rename = "autoSelectMcpTopK", default)]
    pub auto_select_mcp_top_k: Option<usize>,
    /// Cap on all intent-selected non-core tool schemas.
    #[serde(rename = "toolAutoSelectTopK", default)]
    pub tool_auto_select_top_k: Option<usize>,
    /// Tool schemas that are always advertised. ToolSearch is force-retained.
    #[serde(rename = "coreTools", default)]
    pub core_tools: Option<Vec<String>>,
    /// MCP fallback when relevance selection finds no MCP match.
    #[serde(rename = "mcpNoMatchPolicy", default)]
    pub mcp_no_match_policy: Option<String>,
    /// Exact MCP names allowed by the `safe` fallback.
    #[serde(rename = "mcpSafeTools", default)]
    pub mcp_safe_tools: Option<Vec<String>>,
    /// High-level payload preset: standard (default) or ultra.
    #[serde(rename = "tokenMode", default)]
    pub token_mode: Option<String>,
    /// Per-partition token caps. Partial objects merge across settings layers.
    #[serde(rename = "contextBudget", default)]
    pub context_budget: Option<ContextBudgetSettings>,
    /// System-prompt section profile: full, minimal, or ultra.
    #[serde(rename = "promptProfile", default)]
    pub prompt_profile: Option<String>,
    /// Static skill prompt policy: all (legacy), index (default), or search.
    #[serde(rename = "skillDisclosure", default)]
    pub skill_disclosure: Option<String>,
    /// Hard cap for the static skill index, in estimated tokens.
    #[serde(rename = "skillIndexMaxTokens", default)]
    pub skill_index_max_tokens: Option<usize>,
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
    #[serde(default)]
    pub models: Option<Vec<ModelProfile>>,
    #[serde(rename = "compactModel", default)]
    pub compact_model: Option<String>,
    /// Cap on the compaction summarizer's output length (tokens). Default
    /// 4096. Increase (e.g. 8192) for long, dense conversations.
    #[serde(rename = "compactMaxTokens", default)]
    pub compact_max_tokens: Option<u32>,
    /// Enable the AutoDream idle background memory consolidation run.
    /// Default true.
    #[serde(rename = "dreamEnabled", default)]
    pub dream_enabled: Option<bool>,
    /// Idle minutes (no client activity) before a dream run may start.
    /// Default 10.
    #[serde(rename = "dreamIdleMinutes", default)]
    pub dream_idle_minutes: Option<u64>,
    #[serde(rename = "elevenlabsApiKey", default)]
    pub elevenlabs_api_key: Option<String>,
    #[serde(rename = "charsPerToken", default = "default_chars_per_token")]
    pub chars_per_token: usize,
    #[serde(rename = "docModel", default)]
    pub doc_model: Option<DocModelSetting>,
    /// Document conversion strategy for file uploads.
    /// - `"auto"` (default): probe for MarkItDown CLI → use if available,
    ///   fall back to legacy `docModel` OCR otherwise.
    /// - `"markitdown"`: require MarkItDown; uploads fail with a
    ///   configuration error when it's not installed.
    /// - `"legacy"`: always use the `docModel` OCR pipeline, never try
    ///   MarkItDown even if installed.
    #[serde(rename = "attachmentConverter", default)]
    pub attachment_converter: Option<String>,
    #[serde(default)]
    pub executables: Option<ExecutableSettings>,
    #[serde(rename = "providerBilling", default)]
    pub provider_billing: Option<ProviderBilling>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutableEntrySetting {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RustExecutableSettings {
    #[serde(default)]
    pub rustc: Option<ExecutableEntrySetting>,
    #[serde(default)]
    pub cargo: Option<ExecutableEntrySetting>,
    #[serde(default)]
    pub rustup: Option<ExecutableEntrySetting>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeExecutableSettings {
    #[serde(default)]
    pub node: Option<ExecutableEntrySetting>,
    #[serde(default)]
    pub npm: Option<ExecutableEntrySetting>,
    #[serde(default)]
    pub npx: Option<ExecutableEntrySetting>,
    #[serde(default)]
    pub corepack: Option<ExecutableEntrySetting>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PythonExecutableSettings {
    #[serde(default)]
    pub python: Option<ExecutableEntrySetting>,
    #[serde(default)]
    pub pip: Option<ExecutableEntrySetting>,
    #[serde(rename = "requireVirtualEnv", default)]
    pub require_virtual_env: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutableSettings {
    #[serde(default)]
    pub rust: Option<RustExecutableSettings>,
    #[serde(default)]
    pub node: Option<NodeExecutableSettings>,
    #[serde(default)]
    pub python: Option<PythonExecutableSettings>,
}

impl ExecutableSettings {
    pub fn entries(&self) -> Vec<(&'static str, &ExecutableEntrySetting)> {
        let mut entries = Vec::new();
        if let Some(group) = &self.rust {
            if let Some(entry) = &group.rustc {
                entries.push(("rust.rustc", entry));
            }
            if let Some(entry) = &group.cargo {
                entries.push(("rust.cargo", entry));
            }
            if let Some(entry) = &group.rustup {
                entries.push(("rust.rustup", entry));
            }
        }
        if let Some(group) = &self.node {
            if let Some(entry) = &group.node {
                entries.push(("node.node", entry));
            }
            if let Some(entry) = &group.npm {
                entries.push(("node.npm", entry));
            }
            if let Some(entry) = &group.npx {
                entries.push(("node.npx", entry));
            }
            if let Some(entry) = &group.corepack {
                entries.push(("node.corepack", entry));
            }
        }
        if let Some(group) = &self.python {
            if let Some(entry) = &group.python {
                entries.push(("python.python", entry));
            }
            if let Some(entry) = &group.pip {
                entries.push(("python.pip", entry));
            }
        }
        entries
    }
}

impl fmt::Debug for SettingsFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SettingsFile")
            .field("model", &self.model)
            .field("max_turns", &self.max_turns)
            .field("max_tokens", &self.max_tokens)
            .field("auto_compact", &self.auto_compact)
            .field("compact_threshold", &self.compact_threshold)
            .field("auto_select_mcp", &self.auto_select_mcp)
            .field("auto_select_mcp_top_k", &self.auto_select_mcp_top_k)
            .field("tool_auto_select_top_k", &self.tool_auto_select_top_k)
            .field("core_tools", &self.core_tools)
            .field("mcp_no_match_policy", &self.mcp_no_match_policy)
            .field("mcp_safe_tools", &self.mcp_safe_tools)
            .field("token_mode", &self.token_mode)
            .field("context_budget", &self.context_budget)
            .field("prompt_profile", &self.prompt_profile)
            .field("skill_disclosure", &self.skill_disclosure)
            .field("skill_index_max_tokens", &self.skill_index_max_tokens)
            .field("context_window", &self.context_window)
            .field("thinking", &self.thinking.as_ref().map(|_| "[configured]"))
            .field("permissions", &self.permissions)
            .field("hooks", &self.hooks.as_ref().map(|_| "[configured]"))
            .field(
                "env_keys",
                &self
                    .env
                    .as_ref()
                    .map(|values| values.keys().collect::<Vec<_>>()),
            )
            .field(
                "mcp_server_names",
                &self
                    .mcp_servers
                    .as_ref()
                    .map(|values| values.keys().collect::<Vec<_>>()),
            )
            .field(
                "model_names",
                &self.models.as_ref().map(|models| {
                    models
                        .iter()
                        .map(|model| model.name.as_str())
                        .collect::<Vec<_>>()
                }),
            )
            .field("compact_model", &self.compact_model)
            .field(
                "elevenlabs_api_key",
                &self.elevenlabs_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field("chars_per_token", &self.chars_per_token)
            .field(
                "doc_model",
                &self.doc_model.as_ref().map(|_| "[configured]"),
            )
            .field(
                "executables",
                &self.executables.as_ref().map(|_| "[configured]"),
            )
            .field(
                "provider_billing",
                &self
                    .provider_billing
                    .as_ref()
                    .map(|b| b.providers.as_ref().map(|p| p.keys().collect::<Vec<_>>())),
            )
            .finish()
    }
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            model: None,
            max_turns: None,
            max_tokens: None,
            auto_compact: None,
            compact_threshold: None,
            auto_select_mcp: None,
            auto_select_mcp_top_k: None,
            tool_auto_select_top_k: None,
            core_tools: None,
            mcp_no_match_policy: None,
            mcp_safe_tools: None,
            token_mode: None,
            context_budget: None,
            prompt_profile: None,
            skill_disclosure: None,
            skill_index_max_tokens: None,
            context_window: None,
            thinking: None,
            permissions: None,
            hooks: None,
            env: None,
            mcp_servers: None,
            models: None,
            compact_model: None,
            compact_max_tokens: None,
            dream_enabled: None,
            dream_idle_minutes: None,
            elevenlabs_api_key: None,
            chars_per_token: default_chars_per_token(),
            doc_model: None,
            attachment_converter: None,
            executables: None,
            provider_billing: None,
            extra: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DocModelSetting {
    Full(DocModelConfig),
    Name(String),
}

#[derive(Clone, Serialize, Deserialize)]
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
    #[serde(default, deserialize_with = "deserialize_roles")]
    pub role: Vec<String>,
    #[serde(rename = "contextWindow", default)]
    pub context_window: Option<usize>,
    #[serde(rename = "maxTokens", default)]
    pub max_tokens: Option<u32>,
    #[serde(rename = "charsPerToken", default)]
    pub chars_per_token: Option<usize>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(rename = "apiFormat", default)]
    pub api_format: Option<String>,
    /// Explicit billing provider key (from `providerBilling.providers`) for
    /// balance display. Overrides `base_url` inference.
    #[serde(rename = "billingProvider", default)]
    pub billing_provider: Option<String>,
}

impl fmt::Debug for ModelProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelProfile")
            .field("name", &self.name)
            .field("label", &self.label)
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("default", &self.default)
            .field("role", &self.role)
            .field("context_window", &self.context_window)
            .field("max_tokens", &self.max_tokens)
            .field("chars_per_token", &self.chars_per_token)
            .field("profile", &self.profile)
            .field("api_format", &self.api_format)
            .field("billing_provider", &self.billing_provider)
            .finish()
    }
}

fn parse_model_api_format(value: &str) -> Option<ApiFormat> {
    let normalized = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>();
    match normalized.as_str() {
        "anthropic" | "anthropiccompatible" => Some(ApiFormat::Anthropic),
        "openai" | "openaicompatible" => Some(ApiFormat::OpenAI),
        _ => None,
    }
}

impl ModelProfile {
    pub fn api_format(&self) -> ApiFormat {
        self.api_format
            .as_deref()
            .and_then(parse_model_api_format)
            .unwrap_or(ApiFormat::Anthropic)
    }

    pub fn is_conversation_model(&self) -> bool {
        self.role.is_empty() || self.role.iter().any(|role| role == "main")
    }

    pub fn has_role(&self, role: &str) -> bool {
        self.role.iter().any(|candidate| candidate == role)
    }

    pub fn infer_doc_provider(&self) -> &str {
        let name = self.name.to_lowercase();
        if name.contains("baidu") && (name.contains("unlimited") || name.contains("ocr")) {
            "baidu_unlimited"
        } else if name.contains("mistral") {
            "mistral_ocr"
        } else if name.contains("deepseek") && name.contains("ocr") {
            "deepseek_ocr"
        } else if name.contains("gemini") {
            "gemini"
        } else {
            "generic_vision"
        }
    }
}

fn deserialize_roles<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    use serde::de;
    struct RoleVisitor;
    impl<'de> de::Visitor<'de> for RoleVisitor {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a string or array of strings")
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<Vec<String>, E> {
            Ok(vec![value.to_string()])
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element::<String>()? {
                values.push(value);
            }
            Ok(values)
        }
    }
    d.deserialize_any(RoleVisitor)
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DocModelConfig {
    pub provider: String,
    pub model: String,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    #[serde(rename = "apiKey")]
    pub api_key: String,
    /// Optional second credential. Required by `baidu_unlimited` (Baidu
    /// Cloud Unlimited-OCR) which authenticates with an API Key + Secret Key
    /// pair via OAuth client_credentials. Ignored by other providers.
    #[serde(rename = "apiSecret", default)]
    pub api_secret: Option<String>,
}

impl fmt::Debug for DocModelConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DocModelConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("api_secret", &"[REDACTED]")
            .finish()
    }
}

impl DocModelConfig {
    /// Compatibility helper. Canonical callers use `ResolvedConfig::doc_model`
    /// so environment references are resolved from the captured input snapshot.
    pub fn resolved_api_key(&self) -> String {
        resolve_process_env_ref(&self.api_key)
    }

    /// Resolve the optional secret key (environment references supported).
    pub fn resolved_api_secret(&self) -> Option<String> {
        self.api_secret
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(resolve_process_env_ref)
    }

    pub fn is_enabled(&self) -> bool {
        if self.provider.is_empty() || self.provider == "none" || self.base_url.is_empty() {
            return false;
        }
        // Baidu Cloud Unlimited-OCR needs both keys for OAuth.
        if self.provider == "baidu_unlimited" {
            return !self.api_key.is_empty() && self.resolved_api_secret().is_some();
        }
        !self.api_key.is_empty()
    }
}

/// Configuration for querying API balances from LLM providers.
/// Each entry maps a provider key to its balance-check endpoint + credentials.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderBilling {
    /// Named provider billing entries (e.g. "kimi", "deepseek", "glm", "jiekou").
    pub providers: Option<HashMap<String, ProviderBillingEntry>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderBillingEntry {
    /// URL for the provider's balance/usage query endpoint.
    #[serde(rename = "balanceUrl")]
    pub balance_url: String,
    /// API key (or `$ENV_VAR` reference).
    #[serde(rename = "apiKey")]
    pub api_key: String,
}

/// Result of a single provider balance query. Safe to display in the UI
/// (no secrets, no raw API responses).
#[derive(Debug, Clone, Serialize)]
pub struct ProviderBalance {
    /// Provider key from settings (e.g. "kimi", "deepseek").
    pub provider: String,
    /// Human-readable balance summary, e.g. "¥123.45" or "CNY 50.00".
    pub summary: String,
    /// Whether the query succeeded.
    pub ok: bool,
    /// Error message when `ok` is false.
    pub error: Option<String>,
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

impl SettingsFile {
    pub fn resolved_doc_model(&self) -> Option<&DocModelConfig> {
        match &self.doc_model {
            Some(DocModelSetting::Full(config)) => Some(config),
            _ => None,
        }
    }

    pub fn conversation_models(&self) -> Vec<ModelProfile> {
        self.models
            .as_ref()
            .map(|models| {
                models
                    .iter()
                    .filter(|model| model.is_conversation_model())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn all_models(&self) -> Vec<ModelProfile> {
        self.models.clone().unwrap_or_default()
    }
}

fn default_chars_per_token() -> usize {
    4
}

fn resolve_process_env_ref(raw: &str) -> String {
    raw.strip_prefix('$')
        .and_then(|name| std::env::var(name).ok())
        .unwrap_or_else(|| raw.to_string())
}

/// The origin of a resolved configuration value. Paths identify the exact
/// source without including any source value (and therefore no secret).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigSource {
    BuiltIn,
    User { path: PathBuf },
    Project { path: PathBuf },
    Local { path: PathBuf },
    ExplicitSettings { path: PathBuf },
    StandaloneMcp { path: PathBuf },
    ExplicitMcp { path: PathBuf },
    Environment { variable: String },
    CommandLine { field: String },
    RemoteRequest { field: String },
    WebRequest { field: String },
    WebSession { field: String },
}

impl ConfigSource {
    pub fn label(&self) -> String {
        match self {
            Self::BuiltIn => "built-in default".into(),
            Self::User { path } => format!("user {}", display_path(path)),
            Self::Project { path } => format!("project {}", display_path(path)),
            Self::Local { path } => format!("local {}", display_path(path)),
            Self::ExplicitSettings { path } => format!("--settings {}", display_path(path)),
            Self::StandaloneMcp { path } => format!("project MCP {}", display_path(path)),
            Self::ExplicitMcp { path } => format!("--mcp-config {}", display_path(path)),
            Self::Environment { variable } => format!("environment ${variable}"),
            Self::CommandLine { field } => format!("CLI {field}"),
            Self::RemoteRequest { field } => format!("remote request {field}"),
            Self::WebRequest { field } => format!("Web request {field}"),
            Self::WebSession { field } => format!("Web session {field}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// Safe-to-display diagnostic. It contains names and paths but never resolved
/// environment values, API keys, or complete configuration payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigDiagnostic {
    pub severity: DiagnosticSeverity,
    pub code: String,
    pub message: String,
    pub field: Option<String>,
    pub source: Option<ConfigSource>,
    pub related_source: Option<ConfigSource>,
    pub suggestion: String,
}

impl ConfigDiagnostic {
    fn warning(
        code: &str,
        message: impl Into<String>,
        field: impl Into<Option<String>>,
        source: impl Into<Option<ConfigSource>>,
        suggestion: impl Into<String>,
    ) -> Self {
        Self {
            severity: DiagnosticSeverity::Warning,
            code: code.into(),
            message: message.into(),
            field: field.into(),
            source: source.into(),
            related_source: None,
            suggestion: suggestion.into(),
        }
    }

    fn error(
        code: &str,
        message: impl Into<String>,
        field: impl Into<Option<String>>,
        source: impl Into<Option<ConfigSource>>,
        suggestion: impl Into<String>,
    ) -> Self {
        Self {
            severity: DiagnosticSeverity::Error,
            code: code.into(),
            message: message.into(),
            field: field.into(),
            source: source.into(),
            related_source: None,
            suggestion: suggestion.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<T> {
    pub value: T,
    pub source: ConfigSource,
}

/// A parsed layer plus exact JSON field presence. Presence is tracked
/// separately because serde defaults cannot distinguish an absent field from
/// an explicitly configured default such as `charsPerToken: 4`.
#[derive(Debug, Clone)]
pub struct ConfigLayer {
    pub source: ConfigSource,
    pub settings: SettingsFile,
    present_fields: BTreeSet<String>,
}

impl ConfigLayer {
    pub fn from_json(source: ConfigSource, value: Value) -> Result<Self, serde_json::Error> {
        let present_fields = collect_present_fields(&value);
        let settings = serde_json::from_value(value)?;
        Ok(Self {
            source,
            settings,
            present_fields,
        })
    }

    fn has(&self, field: &str) -> bool {
        self.present_fields.contains(field)
    }
}

/// Captured environment is an explicit merge input rather than hidden process
/// state. It is intentionally not serializable to prevent accidental exposure.
#[derive(Clone, Default)]
pub struct ConfigEnvironment {
    values: HashMap<String, String>,
}

impl fmt::Debug for ConfigEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut keys = self.values.keys().collect::<Vec<_>>();
        keys.sort();
        formatter
            .debug_struct("ConfigEnvironment")
            .field("keys", &keys)
            .field("values", &"[REDACTED]")
            .finish()
    }
}

impl ConfigEnvironment {
    pub fn capture() -> Self {
        Self {
            values: std::env::vars().collect(),
        }
    }

    pub fn from_values(values: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            values: values.into_iter().collect(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedMcpServer {
    pub name: String,
    pub config: McpServerConfig,
    pub source: ConfigSource,
}

/// Backward-compatible name for the factory's redacted client input.
pub type ResolvedClientConfig = ClientConfig;

#[derive(Clone)]
pub struct ResolvedConfig {
    settings: SettingsFile,
    environment: ConfigEnvironment,
    cwd: PathBuf,
    settings_path: Option<PathBuf>,
    explicit_mcp_path: Option<PathBuf>,
    pub active_model: Resolved<String>,
    pub mcp_servers: Vec<ResolvedMcpServer>,
    pub diagnostics: Vec<ConfigDiagnostic>,
    /// Field path -> all contributing sources. Scalars contain only the final
    /// source; merged arrays/objects retain every contributing source.
    pub field_sources: BTreeMap<String, Vec<ConfigSource>>,
    agent_profiles: HashMap<String, AgentProfile>,
    client_factory: Arc<ClientFactory>,
}

#[derive(Debug, Clone)]
pub struct RunConfigOverrides {
    pub source: ConfigSource,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u32>,
    pub context_window: Option<usize>,
    pub compact_threshold: Option<usize>,
    pub auto_compact: Option<bool>,
    pub permission_mode: Option<PermissionMode>,
    pub allowed_tools: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,
    pub append_system_prompt: Option<String>,
    pub add_dirs: Vec<PathBuf>,
    pub arguments: Option<String>,
    pub is_non_interactive: bool,
}

impl Default for RunConfigOverrides {
    fn default() -> Self {
        Self {
            source: ConfigSource::BuiltIn,
            model: None,
            max_turns: None,
            max_tokens: None,
            context_window: None,
            compact_threshold: None,
            auto_compact: None,
            permission_mode: None,
            allowed_tools: None,
            disallowed_tools: None,
            append_system_prompt: None,
            add_dirs: Vec::new(),
            arguments: None,
            is_non_interactive: true,
        }
    }
}

#[derive(Clone)]
pub struct ResolvedRunConfig {
    pub options: EngineOptions,
    pub field_sources: BTreeMap<String, Vec<ConfigSource>>,
}

impl fmt::Debug for ResolvedConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedConfig")
            .field("cwd", &self.cwd)
            .field("active_model", &self.active_model.value)
            .field(
                "models",
                &self
                    .all_models()
                    .iter()
                    .map(|model| model.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field(
                "mcp_servers",
                &self
                    .mcp_servers
                    .iter()
                    .map(|server| server.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("diagnostics", &self.diagnostics)
            .field("environment", &self.environment)
            .finish_non_exhaustive()
    }
}

impl ResolvedConfig {
    pub fn settings(&self) -> &SettingsFile {
        &self.settings
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn executable_settings(&self) -> Option<&ExecutableSettings> {
        self.settings.executables.as_ref()
    }

    pub fn executable_source_label(&self, field: &str) -> String {
        self.source_for(field)
            .last()
            .map(ConfigSource::label)
            .unwrap_or_else(|| "PATH discovery".into())
    }

    pub fn executable_fingerprint(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_string(&self.settings.executables)
            .unwrap_or_default()
            .hash(&mut hasher);
        self.cwd.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    pub fn all_models(&self) -> &[ModelProfile] {
        self.settings.models.as_deref().unwrap_or_default()
    }

    pub fn conversation_models(&self) -> Vec<ModelProfile> {
        self.settings.conversation_models()
    }

    pub fn source_for(&self, field: &str) -> &[ConfigSource] {
        self.field_sources
            .get(field)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn environment_value(&self, key: &str) -> Option<&str> {
        self.environment.get(key)
    }

    pub fn mcp_configs(&self) -> Vec<(String, McpServerConfig)> {
        self.mcp_servers
            .iter()
            .map(|server| (server.name.clone(), server.config.clone()))
            .collect()
    }

    pub fn mcp_source_labels(&self) -> HashMap<String, String> {
        self.mcp_servers
            .iter()
            .map(|server| (server.name.clone(), server.source.label()))
            .collect()
    }

    pub fn compact_model(&self) -> Option<&str> {
        self.settings.compact_model.as_deref().filter(|name| {
            self.all_models()
                .iter()
                .any(|profile| profile.name == *name)
        })
    }

    pub fn doc_model(&self) -> Option<DocModelConfig> {
        let config = match &self.settings.doc_model {
            Some(DocModelSetting::Full(config)) => config.clone(),
            Some(DocModelSetting::Name(name)) => {
                let profile = self
                    .all_models()
                    .iter()
                    .find(|profile| &profile.name == name)?;
                DocModelConfig {
                    provider: profile.infer_doc_provider().into(),
                    model: profile.name.clone(),
                    base_url: profile.base_url.clone(),
                    api_key: profile.api_key.clone(),
                    api_secret: None,
                }
            }
            None => return None,
        };
        Some(DocModelConfig {
            provider: config.provider,
            model: config.model,
            base_url: self.resolve_reference(&config.base_url),
            api_key: self.resolve_reference(&config.api_key),
            api_secret: config.api_secret.clone(),
        })
    }

    /// Document conversion strategy. `"auto"` (unset or explicit) probes for
    /// MarkItDown and falls back to `docModel`; `"markitdown"` requires it;
    /// `"legacy"` uses `docModel` exclusively.
    pub fn attachment_converter(&self) -> &str {
        self.settings
            .attachment_converter
            .as_deref()
            .filter(|v| !v.is_empty())
            .unwrap_or("auto")
    }

    /// Resolved provider billing entries with environment references expanded.
    pub fn provider_billing_entries(&self) -> Vec<(String, ProviderBillingEntry)> {
        let Some(billing) = &self.settings.provider_billing else {
            return vec![];
        };
        let Some(providers) = &billing.providers else {
            return vec![];
        };
        providers
            .iter()
            .map(|(name, entry)| {
                (
                    name.clone(),
                    ProviderBillingEntry {
                        balance_url: self.resolve_reference(&entry.balance_url),
                        api_key: self.resolve_reference(&entry.api_key),
                    },
                )
            })
            .collect()
    }

    pub fn elevenlabs_api_key(&self) -> Option<String> {
        self.settings
            .elevenlabs_api_key
            .as_deref()
            .map(|value| self.resolve_reference(value))
    }

    pub fn client_config(&self, model: Option<&str>) -> ResolvedClientConfig {
        let model = model.unwrap_or(&self.active_model.value).to_string();
        if let Some(profile) = self
            .all_models()
            .iter()
            .find(|profile| profile.name == model)
        {
            return ResolvedClientConfig {
                model,
                base_url: self.resolve_reference(&profile.base_url),
                api_key: nonempty(self.resolve_reference(&profile.api_key)),
                auth_token: None,
                api_format: profile.api_format(),
            };
        }
        ResolvedClientConfig {
            model,
            base_url: self
                .environment
                .get("ANTHROPIC_BASE_URL")
                .unwrap_or(DEFAULT_BASE_URL)
                .to_string(),
            api_key: self
                .environment
                .get("ANTHROPIC_API_KEY")
                .map(ToOwned::to_owned),
            auth_token: self
                .environment
                .get("ANTHROPIC_AUTH_TOKEN")
                .map(ToOwned::to_owned),
            api_format: ApiFormat::Anthropic,
        }
    }

    pub fn model_for(&self, purpose: ClientPurpose, requested_model: Option<&str>) -> String {
        let fallback = requested_model.unwrap_or(&self.active_model.value);
        match purpose {
            ClientPurpose::Conversation => fallback.to_string(),
            ClientPurpose::Compact => self
                .compact_model()
                .map(ToOwned::to_owned)
                .or_else(|| {
                    self.all_models()
                        .iter()
                        .find(|profile| profile.has_role("compact"))
                        .map(|profile| profile.name.clone())
                })
                .unwrap_or_else(|| fallback.to_string()),
            ClientPurpose::Subagent => self
                .all_models()
                .iter()
                .find(|profile| profile.has_role("subagent"))
                .map(|profile| profile.name.clone())
                .unwrap_or_else(|| fallback.to_string()),
            ClientPurpose::Document => self
                .doc_model()
                .map(|config| config.model)
                .or_else(|| {
                    self.all_models()
                        .iter()
                        .find(|profile| profile.has_role("doc"))
                        .map(|profile| profile.name.clone())
                })
                .unwrap_or_else(|| fallback.to_string()),
        }
    }

    pub fn client_for(
        &self,
        purpose: ClientPurpose,
        requested_model: Option<&str>,
    ) -> nonoclaw_core::Result<Arc<Client>> {
        if purpose == ClientPurpose::Document {
            if let Some(document) = self.doc_model() {
                let api_format = self
                    .all_models()
                    .iter()
                    .find(|profile| profile.name == document.model)
                    .map(ModelProfile::api_format)
                    .unwrap_or_else(|| match document.provider.as_str() {
                        "deepseek_ocr" | "generic_vision" | "gemini" => ApiFormat::OpenAI,
                        _ => ApiFormat::Anthropic,
                    });
                return self.client_factory.client(
                    purpose,
                    ClientConfig {
                        model: document.model,
                        base_url: document.base_url,
                        api_key: nonempty(document.api_key),
                        auth_token: None,
                        api_format,
                    },
                );
            }
        }
        let model = self.model_for(purpose, requested_model);
        self.client_factory
            .client(purpose, self.client_config(Some(&model)))
    }

    pub fn client_factory(&self) -> Arc<ClientFactory> {
        Arc::clone(&self.client_factory)
    }

    pub fn model_budget(&self, model: &str) -> (Option<usize>, usize) {
        let profile = self
            .all_models()
            .iter()
            .find(|profile| profile.name == model);
        let max_tokens = profile
            .and_then(|profile| profile.max_tokens)
            .or(self.settings.max_tokens)
            .unwrap_or(DEFAULT_MAX_TOKENS);
        let context_window = profile
            .and_then(|profile| profile.context_window)
            .or(self.settings.context_window);
        let threshold = self
            .settings
            .compact_threshold
            .or_else(|| {
                context_window.map(|window| {
                    window.saturating_sub(max_tokens as usize + COMPACT_SAFETY_MARGIN)
                })
            })
            .unwrap_or(DEFAULT_COMPACT_THRESHOLD);
        (context_window, threshold)
    }

    /// Derive all run modes from one resolved snapshot. Runtime-only handles
    /// (permission/question resolvers, skills, background registry) are attached
    /// by adapters after this call and do not re-parse configuration.
    pub fn resolve_run(&self, overrides: RunConfigOverrides) -> ResolvedRunConfig {
        let mut sources = self.field_sources.clone();
        let model = overrides
            .model
            .clone()
            .unwrap_or_else(|| self.active_model.value.clone());
        if overrides.model.is_some() {
            set_scalar_source(&mut sources, "model", overrides.source.clone());
        }
        let profile = self
            .all_models()
            .iter()
            .find(|profile| profile.name == model);
        let max_tokens = overrides
            .max_tokens
            .or_else(|| profile.and_then(|profile| profile.max_tokens))
            .or(self.settings.max_tokens)
            .unwrap_or(DEFAULT_MAX_TOKENS);
        if overrides.max_tokens.is_some() {
            set_scalar_source(&mut sources, "maxTokens", overrides.source.clone());
        } else if profile.and_then(|profile| profile.max_tokens).is_some() {
            copy_source(
                &mut sources,
                &format!("models.{model}.maxTokens"),
                "maxTokens",
            );
        }
        let context_window = overrides
            .context_window
            .or_else(|| profile.and_then(|profile| profile.context_window))
            .or(self.settings.context_window);
        if overrides.context_window.is_some() {
            set_scalar_source(&mut sources, "contextWindow", overrides.source.clone());
        } else if profile.and_then(|profile| profile.context_window).is_some() {
            copy_source(
                &mut sources,
                &format!("models.{model}.contextWindow"),
                "contextWindow",
            );
        }
        let compact_threshold_tokens = overrides
            .compact_threshold
            .or(self.settings.compact_threshold)
            .or_else(|| {
                context_window.map(|window| {
                    window.saturating_sub(max_tokens as usize + COMPACT_SAFETY_MARGIN)
                })
            })
            .unwrap_or(DEFAULT_COMPACT_THRESHOLD);
        if overrides.compact_threshold.is_some() {
            set_scalar_source(&mut sources, "compactThreshold", overrides.source.clone());
        } else if self.settings.compact_threshold.is_none() && context_window.is_some() {
            copy_source(&mut sources, "contextWindow", "compactThreshold");
        }
        let chars_per_token = profile
            .and_then(|profile| profile.chars_per_token)
            .unwrap_or(self.settings.chars_per_token)
            .max(1);
        if profile
            .and_then(|profile| profile.chars_per_token)
            .is_some()
        {
            copy_source(
                &mut sources,
                &format!("models.{model}.charsPerToken"),
                "charsPerToken",
            );
        }
        let permission_mode = overrides.permission_mode.unwrap_or_else(|| {
            self.settings
                .permissions
                .as_ref()
                .and_then(|permissions| permissions.default_mode.as_deref())
                .and_then(PermissionMode::from_kebab)
                .unwrap_or_default()
        });
        if overrides.permission_mode.is_some() {
            set_scalar_source(
                &mut sources,
                "permissions.defaultMode",
                overrides.source.clone(),
            );
        }
        let configured_allow = self
            .settings
            .permissions
            .as_ref()
            .and_then(|permissions| permissions.allow.clone())
            .unwrap_or_default();
        let configured_deny = self
            .settings
            .permissions
            .as_ref()
            .and_then(|permissions| permissions.deny.clone())
            .unwrap_or_default();
        let allowed_tools = overrides.allowed_tools.clone().unwrap_or(configured_allow);
        let disallowed_tools = overrides
            .disallowed_tools
            .clone()
            .unwrap_or(configured_deny);
        if overrides.allowed_tools.is_some() {
            set_scalar_source(&mut sources, "permissions.allow", overrides.source.clone());
        }
        if overrides.disallowed_tools.is_some() {
            set_scalar_source(&mut sources, "permissions.deny", overrides.source.clone());
        }
        if overrides.append_system_prompt.is_some() {
            set_scalar_source(&mut sources, "appendSystemPrompt", overrides.source.clone());
        }
        if !overrides.add_dirs.is_empty() {
            set_scalar_source(&mut sources, "addDirs", overrides.source.clone());
        }
        if overrides.arguments.is_some() {
            set_scalar_source(&mut sources, "arguments", overrides.source.clone());
        }
        let thinking = self.settings.thinking.as_ref().and_then(parse_thinking);
        let token_mode = self
            .settings
            .token_mode
            .as_deref()
            .map(TokenMode::parse)
            .unwrap_or_default();
        let context_budget = ContextBudget::resolve(
            token_mode,
            self.settings.context_budget.as_ref(),
            self.settings.skill_index_max_tokens,
        );
        let mut options = EngineOptions {
            model: model.clone(),
            max_tokens,
            permission_mode,
            allowed_tools,
            disallowed_tools,
            auto_select_mcp: self
                .settings
                .auto_select_mcp
                .unwrap_or(true),
            auto_select_mcp_top_k: self
                .settings
                .auto_select_mcp_top_k
                .unwrap_or(crate::tool_selector::DEFAULT_TOP_K),
            tool_auto_select_top_k: self
                .settings
                .tool_auto_select_top_k
                .or(self.settings.auto_select_mcp_top_k)
                .unwrap_or(if token_mode.is_ultra() {
                    3
                } else {
                    crate::tool_selector::DEFAULT_TOP_K
                }),
            core_tools: self.settings.core_tools.clone().unwrap_or_else(|| {
                if token_mode.is_ultra() {
                    crate::tool_selector::ultra_core_tools()
                } else {
                    crate::tool_selector::default_core_tools()
                }
            }),
            mcp_no_match_policy: self
                .settings
                .mcp_no_match_policy
                .as_deref()
                .map(crate::tool_selector::McpNoMatchPolicy::parse)
                .unwrap_or_default(),
            mcp_safe_tools: self.settings.mcp_safe_tools.clone().unwrap_or_default(),
            add_dirs: overrides.add_dirs,
            max_turns: overrides
                .max_turns
                .or(self.settings.max_turns)
                .unwrap_or(DEFAULT_MAX_TURNS),
            finalize_on_max_turns: false,
            append_system_prompt: overrides.append_system_prompt,
            skills_manager: None,
            arguments: overrides.arguments,
            background_registry: None,
            thinking,
            is_non_interactive: overrides.is_non_interactive,
            permission_resolver: None,
            question_resolver: None,
            auto_compact: overrides
                .auto_compact
                .or(self.settings.auto_compact)
                .unwrap_or(true),
            compact_threshold_tokens,
            compact_model: Some(self.model_for(ClientPurpose::Compact, Some(&model))),
            compact_max_tokens: self
                .settings
                .compact_max_tokens
                .unwrap_or(crate::compact::DEFAULT_MAX_SUMMARY_TOKENS),
            compact_client: self.client_for(ClientPurpose::Compact, Some(&model)).ok(),
            subagent_client: self.client_for(ClientPurpose::Subagent, Some(&model)).ok(),
            chars_per_token,
            context_window,
            max_budget_usd: None,
            token_mode,
            context_budget,
            prompt_profile: self
                .settings
                .prompt_profile
                .as_deref()
                .map(crate::prompt::PromptProfile::parse)
                .unwrap_or(if token_mode.is_ultra() {
                    crate::prompt::PromptProfile::Ultra
                } else {
                    crate::prompt::PromptProfile::Full
                }),
            skill_disclosure: self
                .settings
                .skill_disclosure
                .as_deref()
                .map(crate::skills::SkillDisclosure::parse)
                .unwrap_or(if token_mode.is_ultra() {
                    crate::skills::SkillDisclosure::Search
                } else {
                    crate::skills::SkillDisclosure::default()
                }),
            skill_index_max_tokens: context_budget.skill_index_tokens,
            startup_events: self
                .diagnostics
                .iter()
                .map(|diagnostic| RunEvent::ConfigDiagnostic {
                    severity: format!("{:?}", diagnostic.severity).to_lowercase(),
                    code: diagnostic.code.clone(),
                    field: diagnostic.field.clone(),
                    source: diagnostic.source.as_ref().map(ConfigSource::label),
                    message: diagnostic.message.clone(),
                    suggestion: diagnostic.suggestion.clone(),
                })
                .chain(self.mcp_servers.iter().map(|server| RunEvent::McpDiagnostic {
                    server: server.name.clone(),
                    status: TechnicalStatus::Pending,
                    source: Some(server.source.label()),
                    detail: "MCP server configured; connection is isolated from the core runtime".into(),
                }))
                .collect(),
        };
        if overrides.max_turns.is_some() {
            set_scalar_source(&mut sources, "maxTurns", overrides.source.clone());
        }
        if overrides.auto_compact.is_some() {
            set_scalar_source(&mut sources, "autoCompact", overrides.source.clone());
        }
        if let Some(profile_name) = profile.and_then(|profile| profile.profile.as_deref()) {
            if let Some(agent_profile) = self.agent_profiles.get(profile_name) {
                crate::agents::apply_profile(&mut options, agent_profile);
                let profile_field = format!("models.{model}.profile");
                if agent_profile.system_prompt_append.is_some() {
                    copy_source(&mut sources, &profile_field, "appendSystemPrompt");
                }
                if !agent_profile.tools_allow.is_empty() {
                    copy_source(&mut sources, &profile_field, "permissions.allow");
                }
                if !agent_profile.tools_deny.is_empty() {
                    copy_source(&mut sources, &profile_field, "permissions.deny");
                }
                if agent_profile.permission_mode.is_some() {
                    copy_source(&mut sources, &profile_field, "permissions.defaultMode");
                }
            }
        }
        ResolvedRunConfig {
            options,
            field_sources: sources,
        }
    }

    pub fn reload(&self) -> Self {
        load_resolved_config(
            &self.cwd,
            self.settings_path.as_deref(),
            self.explicit_mcp_path.as_deref(),
        )
    }

    pub fn log_diagnostics(&self) {
        for diagnostic in &self.diagnostics {
            let source = diagnostic
                .source
                .as_ref()
                .map(ConfigSource::label)
                .unwrap_or_else(|| "configuration".into());
            match diagnostic.severity {
                DiagnosticSeverity::Info => tracing::info!(
                    code = %diagnostic.code,
                    field = ?diagnostic.field,
                    %source,
                    "{}; {}",
                    diagnostic.message,
                    diagnostic.suggestion
                ),
                DiagnosticSeverity::Warning => tracing::warn!(
                    code = %diagnostic.code,
                    field = ?diagnostic.field,
                    %source,
                    "{}; {}",
                    diagnostic.message,
                    diagnostic.suggestion
                ),
                DiagnosticSeverity::Error => tracing::error!(
                    code = %diagnostic.code,
                    field = ?diagnostic.field,
                    %source,
                    "{}; {}",
                    diagnostic.message,
                    diagnostic.suggestion
                ),
            }
        }
    }

    fn resolve_reference(&self, raw: &str) -> String {
        raw.strip_prefix('$')
            .and_then(|name| self.environment.get(name))
            .unwrap_or(raw)
            .to_string()
    }
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn parse_thinking(value: &Value) -> Option<ThinkingConfig> {
    match value {
        Value::Bool(true) => Some(ThinkingConfig::adaptive()),
        Value::Bool(false) | Value::Null => None,
        _ => serde_json::from_value(value.clone()).ok(),
    }
}

/// Resolve `$NONOCLAW_HOME` or `~/.nonoclaw`.
pub fn nonoclaw_config_dir() -> Option<PathBuf> {
    nonoclaw_core::nonoclaw_data_dir()
}

pub fn load_user_settings() -> Option<SettingsFile> {
    let path = nonoclaw_config_dir()?.join("settings.json");
    read_settings_file_compat(&path)
}

pub fn load_project_settings(cwd: &Path) -> Option<SettingsFile> {
    read_settings_file_compat(&cwd.join(".nonoclaw/settings.json"))
}

pub fn load_local_settings(cwd: &Path) -> Option<SettingsFile> {
    read_settings_file_compat(&cwd.join(".nonoclaw/settings.local.json"))
}

pub fn load_flag_settings(path: &Path) -> Option<SettingsFile> {
    read_settings_file_compat(path)
}

fn read_settings_file_compat(path: &Path) -> Option<SettingsFile> {
    let mut layers = Vec::new();
    let mut diagnostics = Vec::new();
    load_settings_layer(
        path,
        ConfigSource::ExplicitSettings {
            path: path.to_path_buf(),
        },
        false,
        &mut layers,
        &mut diagnostics,
    );
    layers.pop().map(|layer| layer.settings)
}

/// Compatibility in-place merge. New production callers use
/// [`resolve_layers`], which applies the same rules and records provenance.
pub fn merge_settings(base: &mut SettingsFile, overlay: &SettingsFile) {
    merge_settings_value(base, overlay, None);
}

fn merge_settings_value(
    base: &mut SettingsFile,
    overlay: &SettingsFile,
    presence: Option<&BTreeSet<String>>,
) {
    let present = |field: &str, fallback: bool| {
        presence
            .map(|fields| fields.contains(field))
            .unwrap_or(fallback)
    };
    if present("model", overlay.model.is_some()) {
        base.model.clone_from(&overlay.model);
    }
    if present("maxTurns", overlay.max_turns.is_some()) {
        base.max_turns = overlay.max_turns;
    }
    if present("maxTokens", overlay.max_tokens.is_some()) {
        base.max_tokens = overlay.max_tokens;
    }
    if present("autoCompact", overlay.auto_compact.is_some()) {
        base.auto_compact = overlay.auto_compact;
    }
    if present("compactThreshold", overlay.compact_threshold.is_some()) {
        base.compact_threshold = overlay.compact_threshold;
    }
    if present("autoSelectMcp", overlay.auto_select_mcp.is_some()) {
        base.auto_select_mcp = overlay.auto_select_mcp;
    }
    if present("autoSelectMcpTopK", overlay.auto_select_mcp_top_k.is_some()) {
        base.auto_select_mcp_top_k = overlay.auto_select_mcp_top_k;
    }
    if present(
        "toolAutoSelectTopK",
        overlay.tool_auto_select_top_k.is_some(),
    ) {
        base.tool_auto_select_top_k = overlay.tool_auto_select_top_k;
    }
    if present("coreTools", overlay.core_tools.is_some()) {
        base.core_tools.clone_from(&overlay.core_tools);
    }
    if present("mcpNoMatchPolicy", overlay.mcp_no_match_policy.is_some()) {
        base.mcp_no_match_policy
            .clone_from(&overlay.mcp_no_match_policy);
    }
    if present("mcpSafeTools", overlay.mcp_safe_tools.is_some()) {
        base.mcp_safe_tools.clone_from(&overlay.mcp_safe_tools);
    }
    if present("tokenMode", overlay.token_mode.is_some()) {
        base.token_mode.clone_from(&overlay.token_mode);
    }
    if present("contextBudget", overlay.context_budget.is_some()) {
        if let Some(overlay_budget) = &overlay.context_budget {
            base.context_budget
                .get_or_insert_with(ContextBudgetSettings::default)
                .merge_from(overlay_budget);
        } else {
            base.context_budget = None;
        }
    }
    if present("promptProfile", overlay.prompt_profile.is_some()) {
        base.prompt_profile = overlay.prompt_profile.clone();
    }
    if present("skillDisclosure", overlay.skill_disclosure.is_some()) {
        base.skill_disclosure = overlay.skill_disclosure.clone();
    }
    if present(
        "skillIndexMaxTokens",
        overlay.skill_index_max_tokens.is_some(),
    ) {
        base.skill_index_max_tokens = overlay.skill_index_max_tokens;
    }
    if present("compactMaxTokens", overlay.compact_max_tokens.is_some()) {
        base.compact_max_tokens = overlay.compact_max_tokens;
    }
    if present("contextWindow", overlay.context_window.is_some()) {
        base.context_window = overlay.context_window;
    }
    if present("thinking", overlay.thinking.is_some()) {
        base.thinking.clone_from(&overlay.thinking);
    }
    if let Some(hooks) = &overlay.hooks {
        base.hooks = Some(deep_merge_values(base.hooks.as_ref(), hooks));
    }
    if let Some(permissions) = &overlay.permissions {
        let merged = base.permissions.get_or_insert_with(Default::default);
        if let Some(allow) = &permissions.allow {
            merged.allow = Some(merge_unique_strings(merged.allow.take(), allow));
        }
        if let Some(deny) = &permissions.deny {
            merged.deny = Some(merge_unique_strings(merged.deny.take(), deny));
        }
        if permissions.default_mode.is_some() {
            merged.default_mode.clone_from(&permissions.default_mode);
        }
    }
    if let Some(environment) = &overlay.env {
        let merged = base.env.get_or_insert_with(Default::default);
        merged.extend(environment.clone());
    }
    if let Some(servers) = &overlay.mcp_servers {
        let merged = base.mcp_servers.get_or_insert_with(Default::default);
        merged.extend(servers.clone());
    }
    if present("models", overlay.models.is_some()) {
        base.models.clone_from(&overlay.models);
    }
    if present("compactModel", overlay.compact_model.is_some()) {
        base.compact_model.clone_from(&overlay.compact_model);
    }
    if present("elevenlabsApiKey", overlay.elevenlabs_api_key.is_some()) {
        base.elevenlabs_api_key
            .clone_from(&overlay.elevenlabs_api_key);
    }
    if present(
        "charsPerToken",
        overlay.chars_per_token != default_chars_per_token(),
    ) {
        base.chars_per_token = overlay.chars_per_token;
    }
    if present("docModel", overlay.doc_model.is_some()) {
        base.doc_model.clone_from(&overlay.doc_model);
    }
    if let Some(executables) = &overlay.executables {
        base.executables = Some(match &base.executables {
            Some(existing) => {
                let existing = serde_json::to_value(existing).unwrap_or_default();
                let overlay =
                    remove_null_values(serde_json::to_value(executables).unwrap_or_default());
                serde_json::from_value(deep_merge_values(Some(&existing), &overlay))
                    .unwrap_or_else(|_| executables.clone())
            }
            None => executables.clone(),
        });
    }
    if present("providerBilling", overlay.provider_billing.is_some()) {
        base.provider_billing.clone_from(&overlay.provider_billing);
    }
    base.extra.extend(overlay.extra.clone());
}

fn merge_unique_strings(existing: Option<Vec<String>>, added: &[String]) -> Vec<String> {
    let mut merged = existing.unwrap_or_default();
    merged.extend_from_slice(added);
    merged.sort();
    merged.dedup();
    merged
}

fn remove_null_values(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key, remove_null_values(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(remove_null_values).collect()),
        other => other,
    }
}

fn deep_merge_values(base: Option<&Value>, overlay: &Value) -> Value {
    let Some(base) = base else {
        return overlay.clone();
    };
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            let mut merged = base.clone();
            for (key, value) in overlay {
                let next = deep_merge_values(merged.get(key), value);
                merged.insert(key.clone(), next);
            }
            Value::Object(merged)
        }
        (Value::Array(base), Value::Array(overlay)) => {
            let mut merged = base.clone();
            for value in overlay {
                if !merged.contains(value) {
                    merged.push(value.clone());
                }
            }
            Value::Array(merged)
        }
        _ => overlay.clone(),
    }
}

/// Pure configuration merge. No files, process environment, or global state
/// are read or modified here.
pub fn resolve_layers(
    layers: &[ConfigLayer],
    environment: &ConfigEnvironment,
    cwd: &Path,
) -> ResolvedConfig {
    let mut settings = SettingsFile::default();
    let mut field_sources = built_in_sources();
    let mut diagnostics = Vec::new();

    for layer in layers {
        diagnose_overrides(&settings, layer, &field_sources, &mut diagnostics);
        merge_settings_value(&mut settings, &layer.settings, Some(&layer.present_fields));
        record_layer_sources(layer, &mut field_sources);
    }

    // File-defined env is a fallback; actual process environment wins.
    let mut effective_environment = settings.env.clone().unwrap_or_default();
    for (key, value) in &environment.values {
        effective_environment.insert(key.clone(), value.clone());
        if settings
            .env
            .as_ref()
            .is_some_and(|configured| configured.contains_key(key))
        {
            set_scalar_source(
                &mut field_sources,
                &format!("env.{key}"),
                ConfigSource::Environment {
                    variable: key.clone(),
                },
            );
        }
    }
    let environment = ConfigEnvironment {
        values: effective_environment,
    };

    validate_settings(&settings, &environment, &field_sources, &mut diagnostics);
    record_environment_reference_sources(&settings, &environment, &mut field_sources);

    let (active_model, active_source) = resolve_active_model(&settings, &field_sources);
    let mut mcp_servers: Vec<ResolvedMcpServer> = settings
        .mcp_servers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|(name, mut config)| {
            config.command = resolve_from_environment(&config.command, &environment);
            config.args = config
                .args
                .into_iter()
                .map(|arg| resolve_from_environment(&arg, &environment))
                .collect();
            config.env = config
                .env
                .into_iter()
                .map(|(key, value)| (key, resolve_from_environment(&value, &environment)))
                .collect();
            let source = field_sources
                .get(&format!("mcpServers.{name}"))
                .and_then(|sources| sources.last())
                .cloned()
                .unwrap_or(ConfigSource::BuiltIn);
            ResolvedMcpServer {
                name,
                config,
                source,
            }
        })
        .collect();
    mcp_servers.sort_by(|a, b| a.name.cmp(&b.name));

    ResolvedConfig {
        settings,
        environment,
        cwd: cwd.to_path_buf(),
        settings_path: None,
        explicit_mcp_path: None,
        active_model: Resolved {
            value: active_model,
            source: active_source,
        },
        mcp_servers,
        diagnostics,
        field_sources,
        agent_profiles: HashMap::new(),
        client_factory: Arc::new(ClientFactory::new()),
    }
}

/// Canonical loader used by CLI, Web, remote server, subagents, compaction,
/// and document-model callers.
pub fn load_resolved_config(
    cwd: &Path,
    explicit_settings: Option<&Path>,
    explicit_mcp: Option<&Path>,
) -> ResolvedConfig {
    let mut diagnostics = Vec::new();
    let mut layers = Vec::new();

    if let Some(home) = nonoclaw_config_dir() {
        let path = home.join("settings.json");
        load_settings_layer(
            &path,
            ConfigSource::User { path: path.clone() },
            false,
            &mut layers,
            &mut diagnostics,
        );
    }
    let project = cwd.join(".nonoclaw/settings.json");
    load_settings_layer(
        &project,
        ConfigSource::Project {
            path: project.clone(),
        },
        false,
        &mut layers,
        &mut diagnostics,
    );
    let local = cwd.join(".nonoclaw/settings.local.json");
    load_settings_layer(
        &local,
        ConfigSource::Local {
            path: local.clone(),
        },
        false,
        &mut layers,
        &mut diagnostics,
    );
    if let Some(path) = explicit_settings {
        load_settings_layer(
            path,
            ConfigSource::ExplicitSettings {
                path: path.to_path_buf(),
            },
            true,
            &mut layers,
            &mut diagnostics,
        );
    }
    let standalone_mcp = cwd.join(".nonoclaw/mcp.json");
    load_mcp_layer(
        &standalone_mcp,
        ConfigSource::StandaloneMcp {
            path: standalone_mcp.clone(),
        },
        false,
        &mut layers,
        &mut diagnostics,
    );
    if let Some(path) = explicit_mcp {
        load_mcp_layer(
            path,
            ConfigSource::ExplicitMcp {
                path: path.to_path_buf(),
            },
            true,
            &mut layers,
            &mut diagnostics,
        );
    }

    let mut resolved = resolve_layers(&layers, &ConfigEnvironment::capture(), cwd);
    diagnostics.append(&mut resolved.diagnostics);
    resolved.diagnostics = diagnostics;
    resolved.settings_path = explicit_settings.map(Path::to_path_buf);
    resolved.explicit_mcp_path = explicit_mcp.map(Path::to_path_buf);
    load_agent_profiles(&mut resolved);
    resolved
}

fn load_settings_layer(
    path: &Path,
    source: ConfigSource,
    required: bool,
    layers: &mut Vec<ConfigLayer>,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => return,
        Err(error) => {
            diagnostics.push(ConfigDiagnostic::error(
                "settings_read_failed",
                format!("cannot read settings file: {error}"),
                None,
                Some(source),
                "Check that the path exists and is readable.",
            ));
            return;
        }
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            diagnostics.push(ConfigDiagnostic::error(
                "settings_json_invalid",
                format!(
                    "invalid JSON at line {}, column {}: {error}",
                    error.line(),
                    error.column()
                ),
                None,
                Some(source),
                "Fix the JSON syntax; this layer was ignored.",
            ));
            return;
        }
    };
    diagnose_unknown_fields(&value, &source, diagnostics);
    match ConfigLayer::from_json(source.clone(), value) {
        Ok(layer) => layers.push(layer),
        Err(error) => diagnostics.push(ConfigDiagnostic::error(
            "settings_value_invalid",
            format!("invalid setting value: {error}"),
            field_from_serde_error(&error),
            Some(source),
            "Use the documented type for this field; this layer was ignored.",
        )),
    }
}

fn load_mcp_layer(
    path: &Path,
    source: ConfigSource,
    required: bool,
    layers: &mut Vec<ConfigLayer>,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => return,
        Err(error) => {
            diagnostics.push(ConfigDiagnostic::error(
                "mcp_read_failed",
                format!("cannot read MCP config: {error}"),
                Some("mcpServers".into()),
                Some(source),
                "Check that the path exists and is readable.",
            ));
            return;
        }
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            diagnostics.push(ConfigDiagnostic::error(
                "mcp_json_invalid",
                format!(
                    "invalid JSON at line {}, column {}: {error}",
                    error.line(),
                    error.column()
                ),
                Some("mcpServers".into()),
                Some(source),
                "Fix the MCP JSON syntax; this source was ignored.",
            ));
            return;
        }
    };
    diagnose_mcp_unknown_fields(&value, &source, diagnostics);
    let wrapped = serde_json::json!({
        "mcpServers": value.get("mcpServers").cloned().unwrap_or_else(|| Value::Object(Default::default()))
    });
    match ConfigLayer::from_json(source.clone(), wrapped) {
        Ok(layer) => layers.push(layer),
        Err(error) => diagnostics.push(ConfigDiagnostic::error(
            "mcp_value_invalid",
            format!("invalid MCP server field: {error}"),
            Some("mcpServers".into()),
            Some(source),
            "Each server requires a string command; type defaults to stdio.",
        )),
    }
}

fn load_agent_profiles(resolved: &mut ResolvedConfig) {
    let references: Vec<(String, ConfigSource)> = resolved
        .all_models()
        .iter()
        .filter_map(|model| {
            model.profile.as_ref().map(|profile| {
                let source = resolved
                    .source_for(&format!("models.{}.profile", model.name))
                    .last()
                    .cloned()
                    .unwrap_or_else(|| resolved.active_model.source.clone());
                (profile.clone(), source)
            })
        })
        .collect();
    for (profile_name, source) in references {
        if resolved.agent_profiles.contains_key(&profile_name) {
            continue;
        }
        match crate::agents::load_profile(&resolved.cwd, &profile_name) {
            Some(profile) => {
                resolved.agent_profiles.insert(profile_name, profile);
            }
            None => resolved.diagnostics.push(ConfigDiagnostic::warning(
                "agent_profile_not_found",
                format!("model references unknown agent profile `{profile_name}`"),
                Some("models.*.profile".to_string()),
                Some(source),
                format!("Create .nonoclaw/agents/{profile_name}.md or remove the reference."),
            )),
        }
    }
}

fn validate_settings(
    settings: &SettingsFile,
    environment: &ConfigEnvironment,
    field_sources: &BTreeMap<String, Vec<ConfigSource>>,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    let source = |field: &str| {
        field_sources
            .get(field)
            .and_then(|sources| sources.last())
            .cloned()
    };
    if settings.chars_per_token == 0 {
        diagnostics.push(ConfigDiagnostic::error(
            "invalid_chars_per_token",
            "charsPerToken must be greater than zero",
            Some("charsPerToken".into()),
            source("charsPerToken"),
            "Set charsPerToken to a positive integer (typically 2-4).",
        ));
    }
    if let Some(mode) = settings.token_mode.as_deref() {
        if !matches!(mode.to_ascii_lowercase().as_str(), "standard" | "ultra") {
            diagnostics.push(ConfigDiagnostic::error(
                "invalid_token_mode",
                format!("unknown token mode `{mode}`"),
                Some("tokenMode".into()),
                source("tokenMode"),
                "Use standard or ultra.",
            ));
        }
    }
    if let Some(profile) = settings.prompt_profile.as_deref() {
        if !matches!(
            profile.to_ascii_lowercase().as_str(),
            "full" | "minimal" | "ultra"
        ) {
            diagnostics.push(ConfigDiagnostic::error(
                "invalid_prompt_profile",
                format!("unknown prompt profile `{profile}`"),
                Some("promptProfile".into()),
                source("promptProfile"),
                "Use full, minimal, or ultra.",
            ));
        }
    }
    if let Some(context_budget) = &settings.context_budget {
        for (field, value) in context_budget.fields() {
            if value == Some(0) {
                let path = format!("contextBudget.{field}");
                diagnostics.push(ConfigDiagnostic::error(
                    "invalid_context_budget",
                    format!("{path} must be greater than zero"),
                    Some(path.clone()),
                    source(&path).or_else(|| source("contextBudget")),
                    "Use a positive token budget.",
                ));
            }
        }
    }
    if let Some(disclosure) = settings.skill_disclosure.as_deref() {
        if !matches!(
            disclosure.to_ascii_lowercase().as_str(),
            "all" | "index" | "search"
        ) {
            diagnostics.push(ConfigDiagnostic::error(
                "invalid_skill_disclosure",
                format!("unknown skill disclosure policy `{disclosure}`"),
                Some("skillDisclosure".into()),
                source("skillDisclosure"),
                "Use all, index, or search.",
            ));
        }
    }
    if settings.skill_index_max_tokens == Some(0) {
        diagnostics.push(ConfigDiagnostic::error(
            "invalid_skill_index_budget",
            "skillIndexMaxTokens must be greater than zero",
            Some("skillIndexMaxTokens".into()),
            source("skillIndexMaxTokens"),
            "Use a positive token budget (typically 250-500).",
        ));
    }
    if settings.tool_auto_select_top_k == Some(0) || settings.auto_select_mcp_top_k == Some(0) {
        diagnostics.push(ConfigDiagnostic::error(
            "invalid_tool_auto_select_top_k",
            "tool selection top-k must be greater than zero",
            Some("toolAutoSelectTopK".into()),
            source("toolAutoSelectTopK"),
            "Use a positive cap (typically 3-5).",
        ));
    }
    if let Some(policy) = settings.mcp_no_match_policy.as_deref() {
        if !matches!(
            policy.to_ascii_lowercase().as_str(),
            "none" | "safe" | "all"
        ) {
            diagnostics.push(ConfigDiagnostic::error(
                "invalid_mcp_no_match_policy",
                format!("unknown MCP no-match policy `{policy}`"),
                Some("mcpNoMatchPolicy".into()),
                source("mcpNoMatchPolicy"),
                "Use none, safe, or all.",
            ));
        }
    }
    if let Some(permissions) = &settings.permissions {
        if let Some(mode) = &permissions.default_mode {
            if PermissionMode::from_kebab(mode).is_none() {
                diagnostics.push(ConfigDiagnostic::error(
                    "invalid_permission_mode",
                    format!("unknown permission mode `{mode}`"),
                    Some("permissions.defaultMode".into()),
                    source("permissions.defaultMode"),
                    "Use default, acceptEdits, auto, bypassPermissions, or plan.",
                ));
            }
        }
        let allow: BTreeSet<_> = permissions
            .allow
            .iter()
            .flatten()
            .map(String::as_str)
            .collect();
        let deny: BTreeSet<_> = permissions
            .deny
            .iter()
            .flatten()
            .map(String::as_str)
            .collect();
        for conflict in allow.intersection(&deny) {
            diagnostics.push(ConfigDiagnostic::warning(
                "permission_conflict",
                format!("tool pattern `{conflict}` appears in both allow and deny"),
                Some("permissions".into()),
                source("permissions.deny"),
                "Remove it from one list; deny takes precedence at runtime.",
            ));
        }
    }

    let models = settings.models.as_deref().unwrap_or_default();
    let mut names = BTreeSet::new();
    let mut defaults = Vec::new();
    for (index, model) in models.iter().enumerate() {
        let base = format!("models[{index}]");
        if !names.insert(model.name.as_str()) {
            diagnostics.push(ConfigDiagnostic::error(
                "duplicate_model",
                format!("duplicate model name `{}`", model.name),
                Some(format!("{base}.name")),
                source("models"),
                "Give every model profile a unique name.",
            ));
        }
        if model.default {
            defaults.push(model.name.as_str());
        }
        for role in &model.role {
            if !matches!(role.as_str(), "main" | "doc" | "compact" | "subagent") {
                diagnostics.push(ConfigDiagnostic::warning(
                    "unknown_model_role",
                    format!("model `{}` has unknown role `{role}`", model.name),
                    Some(format!("{base}.role")),
                    source("models"),
                    "Use main, doc, compact, or subagent.",
                ));
            }
        }
        if let Some(format) = model.api_format.as_deref() {
            if parse_model_api_format(format).is_none() {
                diagnostics.push(ConfigDiagnostic::error(
                    "invalid_api_format",
                    format!("model `{}` has unknown apiFormat `{format}`", model.name),
                    Some(format!("{base}.apiFormat")),
                    source("models"),
                    "Use anthropic, openai, or an explicit *-compatible alias.",
                ));
            }
        }
        if model.chars_per_token == Some(0) {
            diagnostics.push(ConfigDiagnostic::error(
                "invalid_model_chars_per_token",
                format!("model `{}` has charsPerToken 0", model.name),
                Some(format!("{base}.charsPerToken")),
                source("models"),
                "Set a positive integer.",
            ));
        }
        diagnose_missing_env_reference(
            &model.api_key,
            &format!("{base}.apiKey"),
            source("models"),
            environment,
            diagnostics,
        );
        diagnose_missing_env_reference(
            &model.base_url,
            &format!("{base}.baseUrl"),
            source("models"),
            environment,
            diagnostics,
        );
    }
    if defaults.len() > 1 {
        diagnostics.push(ConfigDiagnostic::warning(
            "multiple_default_models",
            format!(
                "multiple models are marked default: {}",
                defaults.join(", ")
            ),
            Some("models.default".into()),
            source("models"),
            "Keep exactly one default model; the first currently wins.",
        ));
    }
    if let Some(model) = settings.model.as_deref() {
        if !models.is_empty() && !models.iter().any(|profile| profile.name == model) {
            diagnostics.push(ConfigDiagnostic::warning(
                "active_model_not_found",
                format!("model references unknown profile `{model}`"),
                Some("model".into()),
                source("model"),
                "Add the profile to models[] or choose an existing model name.",
            ));
        }
    }
    if let Some(name) = settings.compact_model.as_deref() {
        if !models.iter().any(|profile| profile.name == name) {
            diagnostics.push(ConfigDiagnostic::warning(
                "compact_model_not_found",
                format!("compactModel references unknown model `{name}`"),
                Some("compactModel".into()),
                source("compactModel"),
                "Add that model profile or remove compactModel to use the conversation model.",
            ));
        }
    }
    match &settings.doc_model {
        Some(DocModelSetting::Name(name))
            if !models.iter().any(|profile| profile.name == *name) =>
        {
            diagnostics.push(ConfigDiagnostic::warning(
                "doc_model_not_found",
                format!("docModel references unknown model `{name}`"),
                Some("docModel".into()),
                source("docModel"),
                "Add that model profile or configure docModel inline.",
            ));
        }
        Some(DocModelSetting::Full(config)) => {
            diagnose_missing_env_reference(
                &config.base_url,
                "docModel.baseUrl",
                source("docModel"),
                environment,
                diagnostics,
            );
            diagnose_missing_env_reference(
                &config.api_key,
                "docModel.apiKey",
                source("docModel"),
                environment,
                diagnostics,
            );
        }
        _ => {}
    }
    if let Some(key) = &settings.elevenlabs_api_key {
        diagnose_missing_env_reference(
            key,
            "elevenlabsApiKey",
            source("elevenlabsApiKey"),
            environment,
            diagnostics,
        );
    }
    if let Some(servers) = &settings.mcp_servers {
        for (name, server) in servers {
            diagnose_missing_env_reference(
                &server.command,
                &format!("mcpServers.{name}.command"),
                source(&format!("mcpServers.{name}")),
                environment,
                diagnostics,
            );
            for (index, argument) in server.args.iter().enumerate() {
                diagnose_missing_env_reference(
                    argument,
                    &format!("mcpServers.{name}.args[{index}]"),
                    source(&format!("mcpServers.{name}")),
                    environment,
                    diagnostics,
                );
            }
            for (key, value) in &server.env {
                diagnose_missing_env_reference(
                    value,
                    &format!("mcpServers.{name}.env.{key}"),
                    source(&format!("mcpServers.{name}")),
                    environment,
                    diagnostics,
                );
            }
        }
    }
    validate_executable_settings(settings, field_sources, diagnostics);
}

fn validate_executable_settings(
    settings: &SettingsFile,
    sources: &BTreeMap<String, Vec<ConfigSource>>,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    let Some(executables) = &settings.executables else {
        return;
    };
    for (entry_name, entry) in executables.entries() {
        let base = format!("executables.{entry_name}");
        let source = |suffix: &str| {
            sources
                .get(&format!("{base}.{suffix}"))
                .or_else(|| sources.get(&base))
                .and_then(|items| items.last())
                .cloned()
        };
        if entry.path.is_none() && entry.version.is_none() {
            diagnostics.push(ConfigDiagnostic::warning(
                "executable_entry_empty",
                format!("{base} defines neither path nor version"),
                Some(base.clone()),
                source("path"),
                "Set path and/or exact version, or remove the empty entry.",
            ));
        }
        if let Some(path) = entry.path.as_deref() {
            let supported_prefix = path == "${HOME}"
                || path.starts_with("${HOME}/")
                || path == "${WORKSPACE}"
                || path.starts_with("${WORKSPACE}/");
            if path.is_empty()
                || (!Path::new(path).is_absolute() && !supported_prefix)
                || (path.starts_with("${") && !supported_prefix)
            {
                diagnostics.push(ConfigDiagnostic::error(
                    "executable_path_invalid",
                    format!(
                        "{base}.path must be absolute or start with ${{HOME}} or ${{WORKSPACE}}"
                    ),
                    Some(format!("{base}.path")),
                    source("path"),
                    "Use one executable path without shell syntax or extra arguments.",
                ));
            }
        }
        if entry.version.as_deref().is_some_and(str::is_empty) {
            diagnostics.push(ConfigDiagnostic::error(
                "executable_version_empty",
                format!("{base}.version cannot be empty"),
                Some(format!("{base}.version")),
                source("version"),
                "Set an exact version or remove the version field.",
            ));
        }
    }
}

fn diagnose_missing_env_reference(
    raw: &str,
    field: &str,
    source: Option<ConfigSource>,
    environment: &ConfigEnvironment,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    if let Some(variable) = raw.strip_prefix('$') {
        if environment.get(variable).is_none() {
            diagnostics.push(ConfigDiagnostic::error(
                "environment_reference_missing",
                format!("{field} references unset environment variable `${variable}`"),
                Some(field.into()),
                source,
                format!("Set {variable} in the process environment or settings env map."),
            ));
        }
    }
}

fn record_environment_reference_sources(
    settings: &SettingsFile,
    environment: &ConfigEnvironment,
    sources: &mut BTreeMap<String, Vec<ConfigSource>>,
) {
    let record = |raw: &str, field: String, sources: &mut BTreeMap<String, Vec<ConfigSource>>| {
        if let Some(variable) = raw.strip_prefix('$') {
            if environment.get(variable).is_some() {
                set_scalar_source(
                    sources,
                    &field,
                    ConfigSource::Environment {
                        variable: variable.into(),
                    },
                );
            }
        }
    };

    for model in settings.models.as_deref().unwrap_or_default() {
        record(
            &model.base_url,
            format!("models.{}.baseUrl", model.name),
            sources,
        );
        record(
            &model.api_key,
            format!("models.{}.apiKey", model.name),
            sources,
        );
    }
    if let Some(DocModelSetting::Full(config)) = &settings.doc_model {
        record(&config.base_url, "docModel.baseUrl".into(), sources);
        record(&config.api_key, "docModel.apiKey".into(), sources);
    }
    if let Some(key) = &settings.elevenlabs_api_key {
        record(key, "elevenlabsApiKey".into(), sources);
    }
    if let Some(servers) = &settings.mcp_servers {
        for (name, server) in servers {
            record(
                &server.command,
                format!("mcpServers.{name}.command"),
                sources,
            );
            for argument in &server.args {
                if let Some(variable) = argument.strip_prefix('$') {
                    if environment.get(variable).is_some() {
                        push_source(
                            sources,
                            &format!("mcpServers.{name}.args"),
                            ConfigSource::Environment {
                                variable: variable.into(),
                            },
                        );
                    }
                }
            }
            for (key, value) in &server.env {
                record(value, format!("mcpServers.{name}.env.{key}"), sources);
            }
        }
    }
}

fn resolve_active_model(
    settings: &SettingsFile,
    sources: &BTreeMap<String, Vec<ConfigSource>>,
) -> (String, ConfigSource) {
    if let Some(model) = &settings.model {
        return (
            model.clone(),
            sources
                .get("model")
                .and_then(|sources| sources.last())
                .cloned()
                .unwrap_or(ConfigSource::BuiltIn),
        );
    }
    if let Some(model) = settings
        .conversation_models()
        .into_iter()
        .find(|model| model.default)
        .or_else(|| settings.conversation_models().into_iter().next())
    {
        return (
            model.name,
            sources
                .get("models")
                .and_then(|sources| sources.last())
                .cloned()
                .unwrap_or(ConfigSource::BuiltIn),
        );
    }
    (DEFAULT_MODEL.into(), ConfigSource::BuiltIn)
}

fn resolve_from_environment(raw: &str, environment: &ConfigEnvironment) -> String {
    raw.strip_prefix('$')
        .and_then(|variable| environment.get(variable))
        .unwrap_or(raw)
        .to_string()
}

fn built_in_sources() -> BTreeMap<String, Vec<ConfigSource>> {
    [
        "model",
        "maxTurns",
        "maxTokens",
        "autoCompact",
        "compactThreshold",
        "charsPerToken",
        "permissions.defaultMode",
    ]
    .into_iter()
    .map(|field| (field.into(), vec![ConfigSource::BuiltIn]))
    .collect()
}

fn record_layer_sources(layer: &ConfigLayer, sources: &mut BTreeMap<String, Vec<ConfigSource>>) {
    for field in [
        "model",
        "maxTurns",
        "maxTokens",
        "autoCompact",
        "compactThreshold",
        "autoSelectMcp",
        "autoSelectMcpTopK",
        "toolAutoSelectTopK",
        "coreTools",
        "mcpNoMatchPolicy",
        "mcpSafeTools",
        "tokenMode",
        "contextBudget",
        "promptProfile",
        "skillDisclosure",
        "skillIndexMaxTokens",
        "contextWindow",
        "thinking",
        "compactModel",
        "compactMaxTokens",
        "elevenlabsApiKey",
        "charsPerToken",
        "docModel",
        "attachmentConverter",
    ] {
        if layer.has(field) {
            set_scalar_source(sources, field, layer.source.clone());
        }
    }
    if layer.has("models") {
        sources.retain(|field, _| !field.starts_with("models"));
        set_scalar_source(sources, "models", layer.source.clone());
        if let Some(models) = &layer.settings.models {
            for (index, model) in models.iter().enumerate() {
                set_scalar_source(
                    sources,
                    &format!("models.{}", model.name),
                    layer.source.clone(),
                );
                for field in [
                    "name",
                    "label",
                    "baseUrl",
                    "apiKey",
                    "default",
                    "role",
                    "contextWindow",
                    "maxTokens",
                    "charsPerToken",
                    "profile",
                    "apiFormat",
                ] {
                    if layer.has(&format!("models[{index}].{field}")) {
                        set_scalar_source(
                            sources,
                            &format!("models.{}.{field}", model.name),
                            layer.source.clone(),
                        );
                    }
                }
            }
        }
    }
    if let Some(permissions) = &layer.settings.permissions {
        if permissions.allow.is_some() {
            push_source(sources, "permissions.allow", layer.source.clone());
        }
        if permissions.deny.is_some() {
            push_source(sources, "permissions.deny", layer.source.clone());
        }
        if permissions.default_mode.is_some() {
            set_scalar_source(sources, "permissions.defaultMode", layer.source.clone());
        }
    }
    if let Some(hooks) = &layer.settings.hooks {
        for field in collect_present_fields(hooks) {
            let path = if field.is_empty() {
                "hooks".into()
            } else {
                format!("hooks.{field}")
            };
            push_source(sources, &path, layer.source.clone());
        }
        push_source(sources, "hooks", layer.source.clone());
    }
    if let Some(environment) = &layer.settings.env {
        for key in environment.keys() {
            set_scalar_source(sources, &format!("env.{key}"), layer.source.clone());
        }
    }
    if let Some(servers) = &layer.settings.mcp_servers {
        for name in servers.keys() {
            sources.retain(|field, _| !field.starts_with(&format!("mcpServers.{name}.")));
            set_scalar_source(sources, &format!("mcpServers.{name}"), layer.source.clone());
            for field in ["type", "command", "args", "env"] {
                if layer.has(&format!("mcpServers.{name}.{field}")) {
                    set_scalar_source(
                        sources,
                        &format!("mcpServers.{name}.{field}"),
                        layer.source.clone(),
                    );
                }
            }
        }
    }
    for field in layer
        .present_fields
        .iter()
        .filter(|field| field.starts_with("contextBudget."))
    {
        set_scalar_source(sources, field, layer.source.clone());
    }
    for field in layer
        .present_fields
        .iter()
        .filter(|field| field.starts_with("executables."))
    {
        set_scalar_source(sources, field, layer.source.clone());
    }
    for field in layer.settings.extra.keys() {
        set_scalar_source(sources, field, layer.source.clone());
    }
}

fn set_scalar_source(
    sources: &mut BTreeMap<String, Vec<ConfigSource>>,
    field: &str,
    source: ConfigSource,
) {
    sources.insert(field.into(), vec![source]);
}

fn copy_source(sources: &mut BTreeMap<String, Vec<ConfigSource>>, from: &str, to: &str) {
    if let Some(source) = sources.get(from).and_then(|items| items.last()).cloned() {
        set_scalar_source(sources, to, source);
    }
}

fn push_source(
    sources: &mut BTreeMap<String, Vec<ConfigSource>>,
    field: &str,
    source: ConfigSource,
) {
    let entry = sources.entry(field.into()).or_default();
    if !entry.contains(&source) {
        entry.push(source);
    }
}

fn diagnose_overrides(
    current: &SettingsFile,
    layer: &ConfigLayer,
    sources: &BTreeMap<String, Vec<ConfigSource>>,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    let scalar_conflict = |field: &str, differs: bool, diagnostics: &mut Vec<ConfigDiagnostic>| {
        if layer.has(field) && differs {
            diagnostics.push(ConfigDiagnostic {
                severity: DiagnosticSeverity::Info,
                code: "value_overridden".into(),
                message: format!("higher-precedence source overrides `{field}`"),
                field: Some(field.into()),
                source: Some(layer.source.clone()),
                related_source: sources.get(field).and_then(|items| items.last()).cloned(),
                suggestion: "Remove one value if the override is unintended.".into(),
            });
        }
    };
    scalar_conflict(
        "model",
        current.model.is_some() && current.model != layer.settings.model,
        diagnostics,
    );
    scalar_conflict(
        "maxTurns",
        current.max_turns.is_some() && current.max_turns != layer.settings.max_turns,
        diagnostics,
    );
    scalar_conflict(
        "maxTokens",
        current.max_tokens.is_some() && current.max_tokens != layer.settings.max_tokens,
        diagnostics,
    );
    scalar_conflict(
        "autoCompact",
        current.auto_compact.is_some() && current.auto_compact != layer.settings.auto_compact,
        diagnostics,
    );
    scalar_conflict(
        "compactThreshold",
        current.compact_threshold.is_some()
            && current.compact_threshold != layer.settings.compact_threshold,
        diagnostics,
    );
    scalar_conflict(
        "contextWindow",
        current.context_window.is_some() && current.context_window != layer.settings.context_window,
        diagnostics,
    );
    if let (Some(current_servers), Some(new_servers)) =
        (&current.mcp_servers, &layer.settings.mcp_servers)
    {
        for name in new_servers
            .keys()
            .filter(|name| current_servers.contains_key(*name))
        {
            diagnostics.push(ConfigDiagnostic {
                severity: DiagnosticSeverity::Info,
                code: "mcp_server_overridden".into(),
                message: format!("MCP server `{name}` is replaced by a higher-precedence source"),
                field: Some(format!("mcpServers.{name}")),
                source: Some(layer.source.clone()),
                related_source: sources
                    .get(&format!("mcpServers.{name}"))
                    .and_then(|items| items.last())
                    .cloned(),
                suggestion: "Remove the lower-precedence entry if replacement is intended.".into(),
            });
        }
    }
}

fn collect_present_fields(value: &Value) -> BTreeSet<String> {
    fn visit(value: &Value, prefix: &str, fields: &mut BTreeSet<String>) {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    fields.insert(path.clone());
                    visit(child, &path, fields);
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    let path = format!("{prefix}[{index}]");
                    fields.insert(path.clone());
                    visit(child, &path, fields);
                }
            }
            _ => {}
        }
    }
    let mut fields = BTreeSet::new();
    visit(value, "", &mut fields);
    fields
}

fn diagnose_unknown_fields(
    value: &Value,
    source: &ConfigSource,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    let Some(object) = value.as_object() else {
        diagnostics.push(ConfigDiagnostic::error(
            "settings_root_invalid",
            "settings root must be a JSON object",
            None,
            Some(source.clone()),
            "Wrap settings fields in a JSON object.",
        ));
        return;
    };
    for key in object.keys().filter(|key| {
        !CONFIG_REFERENCE
            .iter()
            .any(|field| field.name == key.as_str())
    }) {
        unknown_field(key, source, diagnostics);
    }
    if let Some(context_budget) = object.get("contextBudget").and_then(Value::as_object) {
        const CONTEXT_BUDGET_FIELDS: &[&str] = &[
            "systemPromptTokens",
            "toolSchemaTokens",
            "skillIndexTokens",
            "projectRulesTokens",
            "memoryTokens",
            "memoryBeadsTokens",
            "memoryFactsTokens",
            "memoryWikiTokens",
            "memoryIndexTokens",
            "gitTokens",
            "singleToolResultTokens",
            "historyTokens",
            "attachmentTokens",
        ];
        for key in context_budget
            .keys()
            .filter(|key| !CONTEXT_BUDGET_FIELDS.contains(&key.as_str()))
        {
            unknown_field(&format!("contextBudget.{key}"), source, diagnostics);
        }
    }
    if let Some(permissions) = object.get("permissions").and_then(Value::as_object) {
        for key in permissions
            .keys()
            .filter(|key| !matches!(key.as_str(), "allow" | "deny" | "defaultMode"))
        {
            unknown_field(&format!("permissions.{key}"), source, diagnostics);
        }
    }
    if let Some(models) = object.get("models").and_then(Value::as_array) {
        const MODEL_FIELDS: &[&str] = &[
            "name",
            "label",
            "baseUrl",
            "apiKey",
            "default",
            "role",
            "contextWindow",
            "maxTokens",
            "charsPerToken",
            "profile",
            "apiFormat",
            "billingProvider",
        ];
        for (index, model) in models.iter().filter_map(Value::as_object).enumerate() {
            for key in model
                .keys()
                .filter(|key| !MODEL_FIELDS.contains(&key.as_str()))
            {
                unknown_field(&format!("models[{index}].{key}"), source, diagnostics);
            }
        }
    }
    if let Some(doc_model) = object.get("docModel").and_then(Value::as_object) {
        for key in doc_model.keys().filter(|key| {
            !matches!(
                key.as_str(),
                "provider" | "model" | "baseUrl" | "apiKey" | "apiSecret"
            )
        }) {
            unknown_field(&format!("docModel.{key}"), source, diagnostics);
        }
    }
    diagnose_executable_unknown_fields(value, source, diagnostics);
    diagnose_mcp_unknown_fields(value, source, diagnostics);
}

fn diagnose_executable_unknown_fields(
    value: &Value,
    source: &ConfigSource,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    let Some(groups) = value.get("executables").and_then(Value::as_object) else {
        return;
    };
    for (group, group_value) in groups {
        let allowed: &[&str] = match group.as_str() {
            "rust" => &["rustc", "cargo", "rustup"],
            "node" => &["node", "npm", "npx", "corepack"],
            "python" => &["python", "pip", "requireVirtualEnv"],
            _ => {
                unknown_field(&format!("executables.{group}"), source, diagnostics);
                continue;
            }
        };
        let Some(entries) = group_value.as_object() else {
            continue;
        };
        for (entry, entry_value) in entries {
            if !allowed.contains(&entry.as_str()) {
                unknown_field(&format!("executables.{group}.{entry}"), source, diagnostics);
                continue;
            }
            if group == "python" && entry == "requireVirtualEnv" {
                continue;
            }
            if let Some(fields) = entry_value.as_object() {
                for field in fields
                    .keys()
                    .filter(|field| !matches!(field.as_str(), "path" | "version"))
                {
                    unknown_field(
                        &format!("executables.{group}.{entry}.{field}"),
                        source,
                        diagnostics,
                    );
                }
            }
        }
    }
}

fn diagnose_mcp_unknown_fields(
    value: &Value,
    source: &ConfigSource,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    if let Some(root) = value.as_object() {
        if matches!(
            source,
            ConfigSource::StandaloneMcp { .. } | ConfigSource::ExplicitMcp { .. }
        ) {
            for key in root.keys().filter(|key| key.as_str() != "mcpServers") {
                unknown_field(key, source, diagnostics);
            }
        }
    }
    if let Some(servers) = value.get("mcpServers").and_then(Value::as_object) {
        for (name, server) in servers {
            if let Some(server) = server.as_object() {
                for key in server
                    .keys()
                    .filter(|key| !matches!(key.as_str(), "type" | "command" | "args" | "env"))
                {
                    unknown_field(&format!("mcpServers.{name}.{key}"), source, diagnostics);
                }
            }
        }
    }
}

fn unknown_field(field: &str, source: &ConfigSource, diagnostics: &mut Vec<ConfigDiagnostic>) {
    diagnostics.push(ConfigDiagnostic::warning(
        "unknown_field",
        format!("unknown configuration field `{field}`"),
        Some(field.into()),
        Some(source.clone()),
        "Check the field spelling or remove it; unknown fields are ignored.",
    ));
}

fn field_from_serde_error(error: &serde_json::Error) -> Option<String> {
    let message = error.to_string();
    message
        .split(" at line")
        .next()
        .and_then(|prefix| prefix.split('`').nth(1))
        .map(ToOwned::to_owned)
}

/// Compatibility loader. New callers should retain the complete
/// [`ResolvedConfig`] instead of dropping provenance and diagnostics.
pub fn load_settings(cwd: &Path, flag_path: Option<&Path>) -> SettingsFile {
    load_resolved_config(cwd, flag_path, None).settings
}

/// Compatibility loader for standalone project MCP configuration. Parsing is
/// delegated to the canonical MCP layer loader so schema behavior cannot drift.
pub fn load_mcp_json(cwd: &Path) -> Option<HashMap<String, McpServerConfig>> {
    let path = cwd.join(".nonoclaw/mcp.json");
    let mut layers = Vec::new();
    let mut diagnostics = Vec::new();
    load_mcp_layer(
        &path,
        ConfigSource::StandaloneMcp { path: path.clone() },
        false,
        &mut layers,
        &mut diagnostics,
    );
    layers.pop()?.settings.mcp_servers
}

/// Legacy compatibility only. Canonical callers use the captured environment
/// in [`ResolvedConfig`] and never mutate process state while resolving config.
#[deprecated(note = "use load_resolved_config; configuration resolution is side-effect free")]
pub fn apply_env(settings: &SettingsFile) {
    if let Some(environment) = &settings.env {
        for (key, value) in environment {
            if std::env::var_os(key).is_none() {
                std::env::set_var(key, value);
            }
        }
    }
}

/// Legacy compatibility only. Canonical callers use `ResolvedConfig::resolve_run`.
#[deprecated(note = "use ResolvedConfig::resolve_run")]
pub fn apply_settings(options: &mut EngineOptions, settings: &SettingsFile) {
    if let Some(model) = &settings.model {
        options.model.clone_from(model);
    }
    if let Some(value) = settings.max_turns {
        options.max_turns = value;
    }
    if let Some(value) = settings.max_tokens {
        options.max_tokens = value;
    }
    if let Some(value) = settings.auto_compact {
        options.auto_compact = value;
    }
    if let Some(value) = settings.compact_threshold {
        options.compact_threshold_tokens = value;
    }
    options.compact_model.clone_from(&settings.compact_model);
    options.chars_per_token = settings.chars_per_token.max(1);
    options.thinking = settings.thinking.as_ref().and_then(parse_thinking);
    if let Some(mode) = settings.token_mode.as_deref() {
        options.token_mode = TokenMode::parse(mode);
        if options.token_mode.is_ultra() {
            if settings.tool_auto_select_top_k.is_none() && settings.auto_select_mcp_top_k.is_none()
            {
                options.tool_auto_select_top_k = 3;
            }
            if settings.core_tools.is_none() {
                options.core_tools = crate::tool_selector::ultra_core_tools();
            }
            if settings.prompt_profile.is_none() {
                options.prompt_profile = crate::prompt::PromptProfile::Ultra;
            }
            if settings.skill_disclosure.is_none() {
                options.skill_disclosure = crate::skills::SkillDisclosure::Search;
            }
        }
    }
    if settings.token_mode.is_some()
        || settings.context_budget.is_some()
        || settings.skill_index_max_tokens.is_some()
    {
        options.context_budget = ContextBudget::resolve(
            options.token_mode,
            settings.context_budget.as_ref(),
            settings.skill_index_max_tokens,
        );
        options.skill_index_max_tokens = options.context_budget.skill_index_tokens;
    }
    if let Some(value) = settings.auto_select_mcp {
        options.auto_select_mcp = value;
    }
    if let Some(value) = settings.auto_select_mcp_top_k {
        options.auto_select_mcp_top_k = value;
    }
    if let Some(value) = settings
        .tool_auto_select_top_k
        .or(settings.auto_select_mcp_top_k)
    {
        options.tool_auto_select_top_k = value;
    }
    if let Some(core) = &settings.core_tools {
        options.core_tools.clone_from(core);
    }
    if let Some(policy) = settings.mcp_no_match_policy.as_deref() {
        options.mcp_no_match_policy = crate::tool_selector::McpNoMatchPolicy::parse(policy);
    }
    if let Some(safe) = &settings.mcp_safe_tools {
        options.mcp_safe_tools.clone_from(safe);
    }
    if let Some(profile) = settings.prompt_profile.as_deref() {
        options.prompt_profile = crate::prompt::PromptProfile::parse(profile);
    }
    if let Some(disclosure) = settings.skill_disclosure.as_deref() {
        options.skill_disclosure = crate::skills::SkillDisclosure::parse(disclosure);
    }
    if let Some(limit) = settings.skill_index_max_tokens {
        if settings
            .context_budget
            .as_ref()
            .and_then(|budget| budget.skill_index_tokens)
            .is_none()
        {
            options.skill_index_max_tokens = limit;
            options.context_budget.skill_index_tokens = limit;
        }
    }
    if let Some(permissions) = &settings.permissions {
        if let Some(mode) = permissions
            .default_mode
            .as_deref()
            .and_then(PermissionMode::from_kebab)
        {
            options.permission_mode = mode;
        }
        if let Some(allow) = &permissions.allow {
            options.allowed_tools.clone_from(allow);
        }
        if let Some(deny) = &permissions.deny {
            options.disallowed_tools.clone_from(deny);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(name: &str) -> ConfigSource {
        ConfigSource::ExplicitSettings {
            path: PathBuf::from(name),
        }
    }

    fn layer(name: &str, json: Value) -> ConfigLayer {
        ConfigLayer::from_json(source(name), json).unwrap()
    }

    #[test]
    fn config_reference_and_unknown_field_diagnostics_share_one_field_list() {
        // **Validates: Requirements 2.7, 12.2**
        let names = config_reference()
            .iter()
            .map(|field| field.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), config_reference().len());
        assert!(names.contains("models"));
        assert!(names.contains("mcpServers"));
        assert!(names.contains("docModel"));
        assert!(names.contains("tokenMode"));
        assert!(names.contains("contextBudget"));

        let mut diagnostics = Vec::new();
        diagnose_unknown_fields(
            &serde_json::json!({"models": [], "unknownReferenceField": true}),
            &source("reference"),
            &mut diagnostics,
        );
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.field.as_deref() == Some("unknownReferenceField")));
        assert!(!diagnostics
            .iter()
            .any(|diagnostic| diagnostic.field.as_deref() == Some("models")));
    }

    #[test]
    fn context_budget_unknown_nested_field_is_diagnosed() {
        let mut diagnostics = Vec::new();
        diagnose_unknown_fields(
            &serde_json::json!({
                "contextBudget": {
                    "historyTokens": 12_000,
                    "histroyTokens": 99
                }
            }),
            &source("budget.json"),
            &mut diagnostics,
        );

        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "unknown_field"
                && diagnostic.field.as_deref() == Some("contextBudget.histroyTokens")
        }));
        assert!(!diagnostics.iter().any(|diagnostic| {
            diagnostic.field.as_deref() == Some("contextBudget.historyTokens")
        }));
    }

    #[test]
    fn token_policy_validation_rejects_unknown_modes_and_zero_partitions() {
        let resolved = resolve_layers(
            &[layer(
                "invalid-budget.json",
                serde_json::json!({
                    "tokenMode": "extreme",
                    "promptProfile": "tiny",
                    "contextBudget": {
                        "systemPromptTokens": 0,
                        "toolSchemaTokens": 0,
                        "skillIndexTokens": 0,
                        "projectRulesTokens": 0,
                        "memoryTokens": 0,
                        "gitTokens": 0,
                        "singleToolResultTokens": 0,
                        "historyTokens": 0,
                        "attachmentTokens": 0
                    }
                }),
            )],
            &ConfigEnvironment::default(),
            Path::new("/project"),
        );

        for (code, field) in [
            ("invalid_token_mode", "tokenMode"),
            ("invalid_prompt_profile", "promptProfile"),
            ("invalid_context_budget", "contextBudget.systemPromptTokens"),
            ("invalid_context_budget", "contextBudget.toolSchemaTokens"),
            ("invalid_context_budget", "contextBudget.skillIndexTokens"),
            ("invalid_context_budget", "contextBudget.projectRulesTokens"),
            ("invalid_context_budget", "contextBudget.memoryTokens"),
            ("invalid_context_budget", "contextBudget.gitTokens"),
            (
                "invalid_context_budget",
                "contextBudget.singleToolResultTokens",
            ),
            ("invalid_context_budget", "contextBudget.historyTokens"),
            ("invalid_context_budget", "contextBudget.attachmentTokens"),
        ] {
            assert!(
                resolved.diagnostics.iter().any(|diagnostic| {
                    diagnostic.code == code && diagnostic.field.as_deref() == Some(field)
                }),
                "missing {code} diagnostic for {field}"
            );
        }
    }

    #[test]
    fn model_api_format_is_case_insensitive_and_accepts_compatible_aliases() {
        assert_eq!(
            parse_model_api_format("Anthropic"),
            Some(ApiFormat::Anthropic)
        );
        assert_eq!(
            parse_model_api_format("OpenAI-Compatible"),
            Some(ApiFormat::OpenAI)
        );
        assert_eq!(
            parse_model_api_format("openai_compatible"),
            Some(ApiFormat::OpenAI)
        );
        assert_eq!(parse_model_api_format("unknown"), None);

        let resolved = resolve_layers(
            &[layer(
                "models.json",
                serde_json::json!({
                    "model": "sonnet-proxy",
                    "models": [{
                        "name": "sonnet-proxy",
                        "baseUrl": "https://gateway.example/v1",
                        "apiKey": "fixture-key",
                        "apiFormat": "OpenAI-Compatible",
                        "default": true
                    }]
                }),
            )],
            &ConfigEnvironment::default(),
            Path::new("/project"),
        );
        assert_eq!(
            resolved.client_config(Some("sonnet-proxy")).api_format,
            ApiFormat::OpenAI
        );
        assert!(!resolved
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "invalid_api_format"));
    }

    #[test]
    fn ultra_mode_sets_low_payload_defaults_and_explicit_values_win() {
        let ultra_only = resolve_layers(
            &[layer(
                "ultra.json",
                serde_json::json!({"tokenMode": "ultra"}),
            )],
            &ConfigEnvironment::default(),
            Path::new("/project"),
        )
        .resolve_run(RunConfigOverrides::default())
        .options;
        assert_eq!(ultra_only.token_mode, TokenMode::Ultra);
        assert_eq!(
            ultra_only.prompt_profile,
            crate::prompt::PromptProfile::Ultra
        );
        assert_eq!(
            ultra_only.skill_disclosure,
            crate::skills::SkillDisclosure::Search
        );
        assert_eq!(ultra_only.tool_auto_select_top_k, 3);
        assert_eq!(ultra_only.context_budget, ContextBudget::ultra());
        assert_eq!(
            ultra_only.mcp_no_match_policy,
            crate::tool_selector::McpNoMatchPolicy::None
        );

        let layers = vec![
            layer(
                "low.json",
                serde_json::json!({
                    "tokenMode": "ultra",
                    "contextBudget": {
                        "systemPromptTokens": 333,
                        "memoryTokens": 222
                    }
                }),
            ),
            layer(
                "high.json",
                serde_json::json!({
                    "contextBudget": {"memoryTokens": 77},
                    "promptProfile": "full",
                    "skillDisclosure": "index",
                    "toolAutoSelectTopK": 4
                }),
            ),
        ];
        let resolved = resolve_layers(
            &layers,
            &ConfigEnvironment::default(),
            Path::new("/project"),
        );
        let merged_budget = resolved.settings.context_budget.as_ref().unwrap();
        assert_eq!(merged_budget.system_prompt_tokens, Some(333));
        assert_eq!(merged_budget.memory_tokens, Some(77));

        let options = resolved.resolve_run(RunConfigOverrides::default()).options;
        assert_eq!(options.token_mode, TokenMode::Ultra);
        assert_eq!(options.context_budget.system_prompt_tokens, 333);
        assert_eq!(options.context_budget.memory_tokens, 77);
        assert_eq!(
            options.context_budget.tool_schema_tokens,
            ContextBudget::ultra().tool_schema_tokens
        );
        assert_eq!(options.prompt_profile, crate::prompt::PromptProfile::Full);
        assert_eq!(
            options.skill_disclosure,
            crate::skills::SkillDisclosure::Index
        );
        assert_eq!(options.tool_auto_select_top_k, 4);
        assert_eq!(options.core_tools, crate::tool_selector::ultra_core_tools());
        assert_eq!(
            options.mcp_no_match_policy,
            crate::tool_selector::McpNoMatchPolicy::None
        );
    }

    #[test]
    fn scalar_array_permissions_models_hooks_and_mcp_rules_are_explicit() {
        let layers = vec![
            layer(
                "low.json",
                serde_json::json!({
                    "model": "low",
                    "permissions": {"allow": ["Read"], "deny": ["Bash"]},
                    "models": [{"name":"low","baseUrl":"http://low","apiKey":"key"}],
                    "hooks": {"PreToolUse": {"items": ["a"], "timeout": 1}},
                    "mcpServers": {"shared": {"command":"low"}, "kept": {"command":"kept"}}
                }),
            ),
            layer(
                "high.json",
                serde_json::json!({
                    "model": "high",
                    "permissions": {"allow": ["Read", "Edit"], "deny": ["Write"]},
                    "models": [{"name":"high","baseUrl":"http://high","apiKey":"key"}],
                    "hooks": {"PreToolUse": {"items": ["b"], "enabled": true}},
                    "mcpServers": {"shared": {"command":"high"}}
                }),
            ),
        ];
        let resolved = resolve_layers(
            &layers,
            &ConfigEnvironment::default(),
            Path::new("/project"),
        );
        assert_eq!(resolved.active_model.value, "high");
        assert_eq!(resolved.all_models()[0].name, "high");
        let permissions = resolved.settings.permissions.as_ref().unwrap();
        assert_eq!(permissions.allow.as_ref().unwrap(), &vec!["Edit", "Read"]);
        assert_eq!(permissions.deny.as_ref().unwrap(), &vec!["Bash", "Write"]);
        assert_eq!(
            resolved.settings.hooks.as_ref().unwrap()["PreToolUse"]["items"],
            serde_json::json!(["a", "b"])
        );
        assert_eq!(
            resolved
                .mcp_servers
                .iter()
                .find(|s| s.name == "shared")
                .unwrap()
                .config
                .command,
            "high"
        );
        assert!(resolved
            .mcp_servers
            .iter()
            .any(|server| server.name == "kept"));
    }

    #[test]
    fn provenance_tracks_scalar_array_and_nested_explicit_fields() {
        let layers = vec![
            layer(
                "user.json",
                serde_json::json!({
                    "model":"a",
                    "permissions":{"allow":["Read"]},
                    "models":[{
                        "name":"profile",
                        "baseUrl":"http://example",
                        "apiKey":"key",
                        "role":[]
                    }],
                    "mcpServers":{"server":{"command":"tool"}}
                }),
            ),
            layer(
                "project.json",
                serde_json::json!({"model":"b","permissions":{"allow":["Edit"]}}),
            ),
        ];
        let resolved = resolve_layers(
            &layers,
            &ConfigEnvironment::default(),
            Path::new("/project"),
        );
        assert_eq!(resolved.source_for("model"), &[source("project.json")]);
        assert_eq!(
            resolved.source_for("permissions.allow"),
            &[source("user.json"), source("project.json")]
        );
        assert_eq!(
            resolved.source_for("models.profile.role"),
            &[source("user.json")]
        );
        assert!(resolved.source_for("models.profile.default").is_empty());
        assert_eq!(
            resolved.source_for("mcpServers.server.command"),
            &[source("user.json")]
        );
        assert!(resolved.source_for("mcpServers.server.args").is_empty());
    }

    #[test]
    fn environment_is_an_explicit_input_and_process_value_has_precedence() {
        let layers = vec![layer(
            "settings.json",
            serde_json::json!({
                "env":{"API_KEY":"file-value"},
                "models":[{"name":"m","baseUrl":"http://example","apiKey":"$API_KEY","default":true}]
            }),
        )];
        let environment =
            ConfigEnvironment::from_values([("API_KEY".into(), "process-value".into())]);
        let resolved = resolve_layers(&layers, &environment, Path::new("/project"));
        let client = resolved.client_config(None);
        assert_eq!(client.api_key.as_deref(), Some("process-value"));
        assert_eq!(
            resolved.source_for("env.API_KEY"),
            &[ConfigSource::Environment {
                variable: "API_KEY".into()
            }]
        );
    }

    #[test]
    fn client_factory_selects_profiles_by_purpose_and_caches_switches() {
        let resolved = resolve_layers(
            &[layer(
                "models.json",
                serde_json::json!({
                    "models": [
                        {"name":"main-a","baseUrl":"http://a","apiKey":"key-a","default":true},
                        {"name":"main-b","baseUrl":"http://b","apiKey":"key-b"},
                        {"name":"cheap","baseUrl":"http://cheap","apiKey":"key-c","role":"compact"},
                        {"name":"worker","baseUrl":"http://worker","apiKey":"key-w","role":"subagent"},
                        {"name":"vision","baseUrl":"http://vision","apiKey":"key-v","role":"doc","apiFormat":"openai"}
                    ],
                    "compactModel": "cheap",
                    "docModel": "vision"
                }),
            )],
            &ConfigEnvironment::default(),
            Path::new("/project"),
        );

        assert_eq!(
            resolved.model_for(ClientPurpose::Conversation, Some("main-b")),
            "main-b"
        );
        assert_eq!(
            resolved.model_for(ClientPurpose::Compact, Some("main-b")),
            "cheap"
        );
        assert_eq!(
            resolved.model_for(ClientPurpose::Subagent, Some("main-b")),
            "worker"
        );
        assert_eq!(resolved.model_for(ClientPurpose::Document, None), "vision");

        let first = resolved
            .client_for(ClientPurpose::Conversation, Some("main-a"))
            .unwrap();
        let switched = resolved
            .client_for(ClientPurpose::Conversation, Some("main-b"))
            .unwrap();
        let first_again = resolved
            .client_for(ClientPurpose::Conversation, Some("main-a"))
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &switched));
        assert!(Arc::ptr_eq(&first, &first_again));
        assert_eq!(
            resolved
                .client_for(ClientPurpose::Compact, Some("main-b"))
                .unwrap()
                .base_url(),
            "http://cheap"
        );
        assert_eq!(
            resolved
                .client_for(ClientPurpose::Subagent, Some("main-b"))
                .unwrap()
                .base_url(),
            "http://worker"
        );
        assert_eq!(
            resolved
                .client_for(ClientPurpose::Document, None)
                .unwrap()
                .api_format(),
            ApiFormat::OpenAI
        );
    }

    #[test]
    fn diagnostics_identify_unknown_fields_bad_refs_and_conflicts() {
        let layers = vec![layer(
            "settings.json",
            serde_json::json!({
                "model":"missing",
                "permissions":{"allow":["Bash"],"deny":["Bash"]},
                "models":[
                    {"name":"one","baseUrl":"http://one","apiKey":"$MISSING","default":true},
                    {"name":"two","baseUrl":"http://two","apiKey":"key","default":true}
                ],
                "compactModel":"absent",
                "docModel":"absent"
            }),
        )];
        let mut resolved = resolve_layers(
            &layers,
            &ConfigEnvironment::default(),
            Path::new("/project"),
        );
        // Unknown fields are diagnosed during loading because pure layers have
        // already been schema-parsed; exercise that file-level pass directly.
        diagnose_unknown_fields(
            &serde_json::json!({"typo": true}),
            &source("settings.json"),
            &mut resolved.diagnostics,
        );
        for code in [
            "unknown_field",
            "active_model_not_found",
            "compact_model_not_found",
            "doc_model_not_found",
            "permission_conflict",
            "multiple_default_models",
            "environment_reference_missing",
        ] {
            assert!(
                resolved
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.code == code),
                "missing diagnostic {code}"
            );
        }
        assert!(resolved
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.source.is_some()));
    }

    #[test]
    fn debug_output_never_contains_resolved_credentials() {
        // **Validates: Requirements 6.6, 9.8, 11.1**
        let resolved = resolve_layers(
            &[layer(
                "secrets.json",
                serde_json::json!({
                    "elevenlabsApiKey":"eleven-secret",
                    "env":{"PRIVATE_TOKEN":"environment-secret"},
                    "models":[{
                        "name":"safe-model",
                        "baseUrl":"http://localhost",
                        "apiKey":"model-secret",
                        "default":true
                    }],
                    "docModel":{
                        "provider":"mistral_ocr",
                        "model":"doc",
                        "baseUrl":"http://localhost",
                        "apiKey":"doc-secret"
                    }
                }),
            )],
            &ConfigEnvironment::default(),
            Path::new("/project"),
        );
        let output = format!("{resolved:?} {:?}", resolved.settings());
        for secret in [
            "eleven-secret",
            "environment-secret",
            "model-secret",
            "doc-secret",
        ] {
            assert!(!output.contains(secret), "debug output leaked {secret}");
        }
        assert!(output.contains("safe-model"));
        assert!(output.contains("[REDACTED]"));
    }

    #[test]
    fn pure_merge_is_deterministic_and_does_not_mutate_inputs() {
        for count in 0..32u32 {
            let layers: Vec<_> = (0..count)
                .map(|index| {
                    layer(
                        &format!("{index}.json"),
                        serde_json::json!({
                            "maxTurns": index + 1,
                            "permissions": {"allow": [format!("Tool{}", index % 5)]}
                        }),
                    )
                })
                .collect();
            let before: Vec<_> = layers
                .iter()
                .map(|layer| layer.settings.max_turns)
                .collect();
            let first = resolve_layers(
                &layers,
                &ConfigEnvironment::default(),
                Path::new("/project"),
            );
            let second = resolve_layers(
                &layers,
                &ConfigEnvironment::default(),
                Path::new("/project"),
            );
            assert_eq!(first.settings.max_turns, second.settings.max_turns);
            assert_eq!(
                first
                    .settings
                    .permissions
                    .as_ref()
                    .and_then(|p| p.allow.clone()),
                second
                    .settings
                    .permissions
                    .as_ref()
                    .and_then(|p| p.allow.clone())
            );
            assert_eq!(
                before,
                layers
                    .iter()
                    .map(|layer| layer.settings.max_turns)
                    .collect::<Vec<_>>()
            );
            assert_eq!(first.settings.max_turns, (count > 0).then_some(count));
        }
    }

    #[test]
    fn config_layers_follow_documented_precedence() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("nonoclaw-resolved-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let cwd = root.join("project");
        std::fs::create_dir_all(cwd.join(".nonoclaw")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("settings.json"), r#"{"model":"user","permissions":{"allow":["Read"]},"mcpServers":{"shared":{"command":"user"}}}"#).unwrap();
        std::fs::write(
            cwd.join(".nonoclaw/settings.json"),
            r#"{"model":"project","permissions":{"allow":["Bash"]}}"#,
        )
        .unwrap();
        std::fs::write(
            cwd.join(".nonoclaw/settings.local.json"),
            r#"{"model":"local","permissions":{"deny":["Write"]}}"#,
        )
        .unwrap();
        let explicit = root.join("explicit.json");
        std::fs::write(&explicit, r#"{"model":"explicit","maxTurns":42,"models":[{"name":"explicit","baseUrl":"http://example","apiKey":"key"}]}"#).unwrap();
        std::fs::write(
            cwd.join(".nonoclaw/mcp.json"),
            r#"{"mcpServers":{"shared":{"command":"standalone"}}}"#,
        )
        .unwrap();
        let explicit_mcp = root.join("mcp.json");
        std::fs::write(
            &explicit_mcp,
            r#"{"mcpServers":{"shared":{"command":"explicit-mcp"}}}"#,
        )
        .unwrap();
        std::env::set_var("NONOCLAW_HOME", &home);
        let resolved = load_resolved_config(&cwd, Some(&explicit), Some(&explicit_mcp));
        std::env::remove_var("NONOCLAW_HOME");
        assert_eq!(resolved.active_model.value, "explicit");
        assert_eq!(resolved.settings.max_turns, Some(42));
        assert_eq!(
            resolved
                .mcp_servers
                .iter()
                .find(|server| server.name == "shared")
                .unwrap()
                .config
                .command,
            "explicit-mcp"
        );
        assert_eq!(
            resolved.source_for("model"),
            &[ConfigSource::ExplicitSettings { path: explicit }]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn executable_entries_merge_sibling_fields_with_field_sources() {
        let low = layer(
            "user.json",
            serde_json::json!({"executables":{"node":{"node":{"path":"/opt/node"}}}}),
        );
        let high = layer(
            "project.json",
            serde_json::json!({"executables":{"node":{"node":{"version":"20.11.1"}}}}),
        );
        let resolved = resolve_layers(
            &[low, high],
            &ConfigEnvironment::default(),
            Path::new("/workspace"),
        );
        let node = resolved
            .executable_settings()
            .and_then(|settings| settings.node.as_ref())
            .and_then(|group| group.node.as_ref())
            .unwrap();
        assert_eq!(node.path.as_deref(), Some("/opt/node"));
        assert_eq!(node.version.as_deref(), Some("20.11.1"));
        assert_eq!(
            resolved.source_for("executables.node.node.path")[0],
            source("user.json")
        );
        assert_eq!(
            resolved.source_for("executables.node.node.version")[0],
            source("project.json")
        );
    }
}
