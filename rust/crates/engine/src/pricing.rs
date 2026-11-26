//! Approximate per-model token pricing for run-cost budgeting.
//!
//! [`EngineOptions::max_budget_usd`] needs a dollar figure to compare against,
//! but the provider only reports token counts (no price). This module maps a
//! model name to an *approximate* per-million-token price so the budget gate
//! can fire deterministically. These numbers are **estimates for the purpose
//! of stopping runaway runs** — they are not billing-grade, and they lag
//! official price changes. Unknown models return `None`, which disables the
//! cost gate (a run without a price is never cut off for cost, matching the
//! historical behavior before budgeting existed).

use nonoclaw_core::Usage;

/// Per-million-token prices in USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    /// Non-cached input tokens, USD per 1M tokens.
    pub input_per_mtok: f64,
    /// Output tokens, USD per 1M tokens.
    pub output_per_mtok: f64,
    /// Cache-read input tokens, USD per 1M tokens. Usually ~10% of input.
    pub cache_read_per_mtok: f64,
}

impl ModelPricing {
    pub const fn new(input: f64, output: f64, cache_read: f64) -> Self {
        Self {
            input_per_mtok: input,
            output_per_mtok: output,
            cache_read_per_mtok: cache_read,
        }
    }

    /// No cache discount (cache read billed at the input rate).
    pub const fn simple(input: f64, output: f64) -> Self {
        Self::new(input, output, input)
    }
}

/// Estimate the USD cost of one turn's usage against a price table.
///
/// Cache-read tokens are charged at the (cheaper) cache-read rate; the
/// remaining input tokens at the full input rate. This mirrors how Anthropic
/// and DeepSeek report usage (`input_tokens` before cache deduction plus a
/// separate `cache_read_input_tokens`).
pub fn estimate_cost(usage: &Usage, pricing: &ModelPricing) -> f64 {
    let per_mtok = 1_000_000.0_f64;
    let cache_read = usage.cache_read_input_tokens as f64;
    let regular_input = usage
        .input_tokens
        .saturating_sub(usage.cache_read_input_tokens) as f64;
    let output = usage.output_tokens as f64;

    (regular_input / per_mtok) * pricing.input_per_mtok
        + (cache_read / per_mtok) * pricing.cache_read_per_mtok
        + (output / per_mtok) * pricing.output_per_mtok
}

/// Resolve an approximate price table for a model name. Matching is
/// case-insensitive substring matching, so `deepseek-v4-pro` and
/// `deepseek-chat` both resolve to the DeepSeek entry. Returns `None` for
/// unknown providers (which disables the cost gate).
pub fn lookup_pricing(model_name: &str) -> Option<ModelPricing> {
    let m = model_name.to_ascii_lowercase();

    if m.contains("deepseek") {
        return Some(if m.contains("reasoner") || m.contains("r1") {
            // deepseek-reasoner: heavier reasoning output.
            ModelPricing::new(0.55, 2.19, 0.055)
        } else {
            // deepseek-chat / V3.x / V4.x.
            ModelPricing::new(0.28, 0.42, 0.028)
        });
    }

    if m.contains("gpt-5") {
        return Some(ModelPricing::new(1.25, 10.0, 0.125));
    }
    if m.contains("gpt-4") || m.contains("gpt-4o") || m.contains("gpt-4.1") {
        return Some(ModelPricing::new(2.50, 10.0, 0.25));
    }
    if m.contains("o1") || m.contains("o3") || m.contains("o4") {
        return Some(ModelPricing::new(15.0, 60.0, 1.5));
    }

    if m.contains("claude") {
        if m.contains("opus") {
            return Some(ModelPricing::new(15.0, 75.0, 1.50));
        }
        if m.contains("haiku") {
            return Some(ModelPricing::new(0.80, 4.0, 0.08));
        }
        // sonnet (default Claude tier)
        return Some(ModelPricing::new(3.0, 15.0, 0.30));
    }

    if m.contains("glm") {
        return Some(ModelPricing::new(0.50, 2.0, 0.05));
    }
    if m.contains("kimi") {
        return Some(ModelPricing::new(0.60, 2.50, 0.06));
    }
    if m.contains("qwen") {
        return Some(ModelPricing::new(0.30, 0.60, 0.03));
    }
    if m.contains("minimax") || m.contains("abab") {
        return Some(ModelPricing::new(0.30, 0.70, 0.03));
    }
    if m.contains("gemini") {
        return Some(ModelPricing::new(1.25, 10.0, 0.125));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use nonoclaw_core::Usage;

    fn usage(input: u64, output: u64, cache_read: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: cache_read,
        }
    }

    #[test]
    fn deepseek_v4_pro_resolves_to_chat_pricing() {
        let p = lookup_pricing("deepseek-v4-pro").unwrap();
        assert_eq!(p.input_per_mtok, 0.28);
        assert_eq!(p.output_per_mtok, 0.42);
    }

    #[test]
    fn reasoner_is_more_expensive() {
        let chat = lookup_pricing("deepseek-chat").unwrap();
        let reasoner = lookup_pricing("deepseek-reasoner").unwrap();
        assert!(reasoner.output_per_mtok > chat.output_per_mtok);
    }

    #[test]
    fn unknown_provider_disables_gate() {
        assert!(lookup_pricing("some-future-model").is_none());
    }

    #[test]
    fn cache_read_is_cheaper_than_input() {
        let p = lookup_pricing("claude-sonnet-4-5").unwrap();
        let with_cache = estimate_cost(&usage(1000, 100, 500), &p);
        let without_cache = estimate_cost(&usage(1000, 100, 0), &p);
        assert!(with_cache < without_cache, "cache discount must lower cost");
    }

    #[test]
    fn cost_scales_linearly() {
        let p = ModelPricing::simple(1.0, 2.0);
        // 1M input + 1M output = 1 + 2 = 3 USD exactly.
        let cost = estimate_cost(&usage(1_000_000, 1_000_000, 0), &p);
        assert!((cost - 3.0).abs() < 1e-9);
    }
}
