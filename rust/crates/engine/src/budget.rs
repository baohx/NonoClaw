//! Provider-independent token-budget presets and configuration overlays.

use serde::{Deserialize, Serialize};

/// High-level payload policy. `Standard` preserves compatibility-oriented
/// defaults; `Ultra` aggressively reduces every request's real payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TokenMode {
    #[default]
    Standard,
    Ultra,
}

impl TokenMode {
    pub fn parse(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "ultra" => Self::Ultra,
            _ => Self::Standard,
        }
    }

    pub fn is_ultra(self) -> bool {
        self == Self::Ultra
    }
}

/// Partial per-partition token limits as represented in settings files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBudgetSettings {
    #[serde(rename = "systemPromptTokens", default)]
    pub system_prompt_tokens: Option<usize>,
    #[serde(rename = "toolSchemaTokens", default)]
    pub tool_schema_tokens: Option<usize>,
    #[serde(rename = "skillIndexTokens", default)]
    pub skill_index_tokens: Option<usize>,
    #[serde(rename = "projectRulesTokens", default)]
    pub project_rules_tokens: Option<usize>,
    #[serde(rename = "memoryTokens", default)]
    pub memory_tokens: Option<usize>,
    #[serde(rename = "gitTokens", default)]
    pub git_tokens: Option<usize>,
    #[serde(rename = "singleToolResultTokens", default)]
    pub single_tool_result_tokens: Option<usize>,
    #[serde(rename = "historyTokens", default)]
    pub history_tokens: Option<usize>,
    #[serde(rename = "attachmentTokens", default)]
    pub attachment_tokens: Option<usize>,
}

impl ContextBudgetSettings {
    pub fn merge_from(&mut self, overlay: &Self) {
        macro_rules! replace_present {
            ($field:ident) => {
                if overlay.$field.is_some() {
                    self.$field = overlay.$field;
                }
            };
        }
        replace_present!(system_prompt_tokens);
        replace_present!(tool_schema_tokens);
        replace_present!(skill_index_tokens);
        replace_present!(project_rules_tokens);
        replace_present!(memory_tokens);
        replace_present!(git_tokens);
        replace_present!(single_tool_result_tokens);
        replace_present!(history_tokens);
        replace_present!(attachment_tokens);
    }

    pub fn fields(&self) -> [(&'static str, Option<usize>); 9] {
        [
            ("systemPromptTokens", self.system_prompt_tokens),
            ("toolSchemaTokens", self.tool_schema_tokens),
            ("skillIndexTokens", self.skill_index_tokens),
            ("projectRulesTokens", self.project_rules_tokens),
            ("memoryTokens", self.memory_tokens),
            ("gitTokens", self.git_tokens),
            ("singleToolResultTokens", self.single_tool_result_tokens),
            ("historyTokens", self.history_tokens),
            ("attachmentTokens", self.attachment_tokens),
        ]
    }
}

/// Fully resolved limits used by the engine. Values are in estimated tokens;
/// conversion to character caps happens at the final payload boundary using
/// the active model's `charsPerToken` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    pub system_prompt_tokens: usize,
    pub tool_schema_tokens: usize,
    pub skill_index_tokens: usize,
    pub project_rules_tokens: usize,
    pub memory_tokens: usize,
    pub git_tokens: usize,
    pub single_tool_result_tokens: usize,
    pub history_tokens: usize,
    pub attachment_tokens: usize,
}

impl ContextBudget {
    pub const fn standard() -> Self {
        Self {
            system_prompt_tokens: 12_000,
            tool_schema_tokens: 8_000,
            skill_index_tokens: 500,
            project_rules_tokens: 8_000,
            memory_tokens: 12_500,
            git_tokens: 1_000,
            single_tool_result_tokens: 7_500,
            history_tokens: 150_000,
            attachment_tokens: 25_000,
        }
    }

    pub const fn ultra() -> Self {
        Self {
            system_prompt_tokens: 1_200,
            tool_schema_tokens: 1_600,
            skill_index_tokens: 128,
            project_rules_tokens: 800,
            memory_tokens: 400,
            git_tokens: 200,
            single_tool_result_tokens: 2_000,
            history_tokens: 12_000,
            attachment_tokens: 4_000,
        }
    }

    pub fn resolve(
        mode: TokenMode,
        overlay: Option<&ContextBudgetSettings>,
        legacy_skill_index_tokens: Option<usize>,
    ) -> Self {
        let mut resolved = if mode.is_ultra() {
            Self::ultra()
        } else {
            Self::standard()
        };
        if let Some(value) = legacy_skill_index_tokens {
            resolved.skill_index_tokens = value;
        }
        if let Some(overlay) = overlay {
            macro_rules! apply {
                ($field:ident) => {
                    if let Some(value) = overlay.$field {
                        resolved.$field = value;
                    }
                };
            }
            apply!(system_prompt_tokens);
            apply!(tool_schema_tokens);
            apply!(skill_index_tokens);
            apply!(project_rules_tokens);
            apply!(memory_tokens);
            apply!(git_tokens);
            apply!(single_tool_result_tokens);
            apply!(history_tokens);
            apply!(attachment_tokens);
        }
        resolved
    }

    pub fn chars(tokens: usize, chars_per_token: usize) -> usize {
        tokens.saturating_mul(chars_per_token.max(1))
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self::standard()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ultra_is_strictly_smaller_in_every_partition() {
        let standard = ContextBudget::standard();
        let ultra = ContextBudget::ultra();
        assert!(ultra.system_prompt_tokens < standard.system_prompt_tokens);
        assert!(ultra.tool_schema_tokens < standard.tool_schema_tokens);
        assert!(ultra.skill_index_tokens < standard.skill_index_tokens);
        assert!(ultra.project_rules_tokens < standard.project_rules_tokens);
        assert!(ultra.memory_tokens < standard.memory_tokens);
        assert!(ultra.git_tokens < standard.git_tokens);
        assert!(ultra.single_tool_result_tokens < standard.single_tool_result_tokens);
        assert!(ultra.history_tokens < standard.history_tokens);
        assert!(ultra.attachment_tokens < standard.attachment_tokens);
    }

    #[test]
    fn explicit_overlay_wins_over_mode_and_legacy_skill_limit() {
        let overlay = ContextBudgetSettings {
            system_prompt_tokens: Some(321),
            skill_index_tokens: Some(123),
            ..Default::default()
        };
        let resolved = ContextBudget::resolve(TokenMode::Ultra, Some(&overlay), Some(250));
        assert_eq!(resolved.system_prompt_tokens, 321);
        assert_eq!(resolved.skill_index_tokens, 123);
        assert_eq!(resolved.memory_tokens, ContextBudget::ultra().memory_tokens);
    }
}
