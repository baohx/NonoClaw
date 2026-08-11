//! Query API balances from configured LLM providers.
//!
//! Each provider's balance-check endpoint returns a JSON response with a
//! different shape. This module normalises them into a human-readable
//! [`ProviderBalance`] summary for display in the Insight rail.

use nonoclaw_engine::{ModelProfile, ProviderBalance, ProviderBillingEntry};
use serde::Deserialize;

/// Match a model to a billing provider. Priority:
/// 1. explicit `billingProvider` field on the model profile;
/// 2. `base_url` inference.
///
/// `base_url` heuristics (checked in order):
/// - moonshot.cn or kimi → "kimi"
/// - deepseek.com → "deepseek"
/// - bigmodel.cn + model name contains "coding" → "glm-coding"
/// - bigmodel.cn → "glm-api"
/// - jiekou.ai or highwayapi.ai → "jiekou"
pub fn model_provider(model: &ModelProfile) -> Option<String> {
    if let Some(provider) = model.billing_provider.as_deref().filter(|p| !p.is_empty()) {
        return Some(provider.to_string());
    }
    let base = model.base_url.to_lowercase();
    if base.contains("moonshot.cn") || base.contains("kimi") {
        return Some("kimi".into());
    }
    if base.contains("deepseek") {
        return Some("deepseek".into());
    }
    if base.contains("bigmodel.cn") {
        let name = model.name.to_lowercase();
        if name.contains("coding") {
            return Some("glm-coding".into());
        }
        return Some("glm-api".into());
    }
    if base.contains("jiekou.ai") || base.contains("highwayapi.ai") {
        return Some("jiekou".into());
    }
    None
}

/// Model name → billing provider key, resolved from each model's `base_url` or
/// `billingProvider` override.  All models with a resolvable provider are
/// included — balance lookup is handled separately by `query_balances`.
pub fn model_provider_map(models: &[ModelProfile]) -> Vec<ModelProviderMapping> {
    models
        .iter()
        .filter_map(|model| {
            let provider = model_provider(model)?;
            Some(ModelProviderMapping {
                model: model.name.clone(),
                provider,
            })
        })
        .collect()
}

/// A model→provider association, safe to display (no secrets).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelProviderMapping {
    pub model: String,
    pub provider: String,
}

/// Query balances for all configured providers concurrently.
/// Silent on failure — problems are surfaced via `ok: false` in the result.
pub async fn query_balances(
    entries: &[(String, ProviderBillingEntry)],
) -> Vec<ProviderBalance> {
    let client = reqwest::Client::new();
    let mut futures = Vec::new();

    for (provider, entry) in entries {
        let provider = provider.clone();
        let url = entry.balance_url.clone();
        let api_key = entry.api_key.clone();
        let c = client.clone();
        futures.push(tokio::spawn(async move {
            query_one(&c, &provider, &url, &api_key).await
        }));
    }

    let mut results = Vec::new();
    for future in futures {
        match future.await {
            Ok(balance) => results.push(balance),
            Err(_) => {} // spawn panic — skip
        }
    }
    results
}

async fn query_one(
    client: &reqwest::Client,
    provider: &str,
    url: &str,
    api_key: &str,
) -> ProviderBalance {
    let fail = |error: String| ProviderBalance {
        provider: provider.to_string(),
        summary: String::new(),
        ok: false,
        error: Some(error),
    };

    if api_key.is_empty() {
        return fail("API key not configured".into());
    }

    let (header_name, header_value) = auth_header(provider, api_key);
    // JieKou's bill endpoint requires query params. We query the lifetime
    // `summary` bill list (one entry per month) and aggregate on the fly.
    let url = match provider {
        "jiekou" if !url.contains("cycleType") => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!(
                "{url}{}cycleType=Month&productCategory=summary&startTime=0&endTime={now}",
                if url.contains('?') { "&" } else { "?" }
            )
        }
        _ => url.to_string(),
    };
    let response = match client
        .get(&url)
        .header(header_name, header_value)
        .header("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return fail(format!("request failed: {e}")),
    };

    if !response.status().is_success() {
        return fail(format!("HTTP {}", response.status()));
    }

    let body: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(e) => return fail(format!("parse error: {e}")),
    };

    parse_provider_balance(provider, &body)
}

/// GLM Coding Plan authenticates with the raw auth token (no `Bearer` prefix);
/// all other providers use `Bearer <key>`.
fn auth_header(provider: &str, api_key: &str) -> (&'static str, String) {
    if provider == "glm-coding" {
        ("Authorization", api_key.to_string())
    } else {
        ("Authorization", format!("Bearer {api_key}"))
    }
}

fn parse_provider_balance(provider: &str, body: &serde_json::Value) -> ProviderBalance {
    let fail = |error: String| ProviderBalance {
        provider: provider.to_string(),
        summary: String::new(),
        ok: false,
        error: Some(error),
    };

    match provider {
        "kimi" => parse_kimi(body).unwrap_or_else(|e| fail(e)),
        "deepseek" => parse_deepseek(body).unwrap_or_else(|e| fail(e)),
        "glm" | "glm-api" => parse_glm_api(body).unwrap_or_else(|e| fail(e)),
        "glm-coding" => parse_glm_coding(body).unwrap_or_else(|e| fail(e)),
        "jiekou" => parse_jiekou(body).unwrap_or_else(|e| fail(e)),
        _ => {
            // Generic: try to find a "balance" field anywhere
            find_generic_balance(body).unwrap_or_else(|e| fail(e))
        }
    }
}

// ── Provider-specific parsers ───────────────────────────────────────────────

/// Kimi: https://platform.kimi.com/docs/api/balance
/// Response: `{"data": {"available_balance": 2.61, "voucher_balance": 0, "cash_balance": 2.61}}`
#[derive(Deserialize)]
struct KimiResponse {
    data: Option<KimiData>,
}
#[derive(Deserialize)]
struct KimiData {
    available_balance: Option<f64>,
}

fn parse_kimi(body: &serde_json::Value) -> Result<ProviderBalance, String> {
    let resp: KimiResponse =
        serde_json::from_value(body.clone()).map_err(|e| format!("kimi parse: {e}"))?;
    let balance = resp
        .data
        .and_then(|d| d.available_balance)
        .ok_or_else(|| "kimi: missing available_balance".to_string())?;
    Ok(ProviderBalance {
        provider: "kimi".into(),
        summary: format!("¥{:.2}", balance),
        ok: true,
        error: None,
    })
}

/// DeepSeek: https://api-docs.deepseek.com/zh-cn/api/get-user-balance
/// Response: `{"is_available":true,"balance_infos":[{currency, total_balance, ...}]}`
/// (flat at top level — no `data` wrapper).
#[derive(Deserialize)]
struct DeepSeekResponse {
    data: Option<DeepSeekData>,
    balance_infos: Option<Vec<DeepSeekBalanceInfo>>,
}
#[derive(Deserialize)]
struct DeepSeekData {
    balance_infos: Option<Vec<DeepSeekBalanceInfo>>,
}
#[derive(Deserialize)]
struct DeepSeekBalanceInfo {
    currency: Option<String>,
    total_balance: Option<String>,
}

fn parse_deepseek(body: &serde_json::Value) -> Result<ProviderBalance, String> {
    let resp: DeepSeekResponse =
        serde_json::from_value(body.clone()).map_err(|e| format!("deepseek parse: {e}"))?;
    // Some gateways wrap the payload in `data`; the official API is flat.
    let infos = resp
        .data
        .and_then(|d| d.balance_infos)
        .or(resp.balance_infos)
        .ok_or_else(|| "deepseek: missing balance_infos".to_string())?;
    let parts: Vec<String> = infos
        .iter()
        .filter_map(|info| {
            let currency = info.currency.as_deref().unwrap_or("?");
            let balance = info.total_balance.as_deref().unwrap_or("?");
            Some(format!("{currency} {balance}"))
        })
        .collect();
    if parts.is_empty() {
        return Err("deepseek: no balance entries".into());
    }
    Ok(ProviderBalance {
        provider: "deepseek".into(),
        summary: parts.join(", "),
        ok: true,
        error: None,
    })
}

/// GLM coding plan: https://docs.bigmodel.cn/cn/coding-plan/extension/usage-query-plugin
/// Real endpoint (from zai-coding-plugins): {base}/api/monitor/usage/quota/limit
/// Authenticates with the raw auth token (no `Bearer`). Response:
/// `{"data": {"limits": [{type: "TOKENS_LIMIT"|"TIME_LIMIT", percentage, ...}]}}`.
#[derive(Deserialize)]
struct GlmCodingResponse {
    data: Option<GlmCodingData>,
}
#[derive(Deserialize)]
struct GlmCodingData {
    limits: Option<Vec<GlmLimit>>,
}
#[derive(Deserialize)]
struct GlmLimit {
    #[serde(rename = "type")]
    kind: Option<String>,
    percentage: Option<f64>,
}

fn parse_glm_coding(body: &serde_json::Value) -> Result<ProviderBalance, String> {
    let resp: GlmCodingResponse =
        serde_json::from_value(body.clone()).map_err(|e| format!("glm-coding parse: {e}"))?;
    let data = resp
        .data
        .ok_or_else(|| "glm-coding: missing data".to_string())?;
    let limits = data
        .limits
        .ok_or_else(|| "glm-coding: missing limits".to_string())?;
    let parts: Vec<String> = limits
        .iter()
        .filter_map(|limit| {
            let kind = match limit.kind.as_deref() {
                Some("TOKENS_LIMIT") => "Token",
                Some("TIME_LIMIT") => "MCP",
                other => other.unwrap_or("?"),
            };
            limit
                .percentage
                .map(|p| format!("{kind} {p:.0}%"))
        })
        .collect();
    if parts.is_empty() {
        return Err("glm-coding: no quota entries".into());
    }
    Ok(ProviderBalance {
        provider: "glm-coding".into(),
        summary: format!("used {}", parts.join(" · ")),
        ok: true,
        error: None,
    })
}

/// GLM API (pay-as-you-go interface billing) has no public balance endpoint
/// documented; falls back to generic `data.balance` / `data.total_balance`
/// parsing for any custom-configured endpoint.
fn parse_glm_api(body: &serde_json::Value) -> Result<ProviderBalance, String> {
    let mut balance = find_generic_balance(body)?;
    balance.provider = "glm-api".into();
    Ok(balance)
}

/// JieKou.ai pay-as-you-go billing:
/// https://docs.jiekou.ai/docs/models/reference-get-bill-pay-as-you-model
/// Endpoint: GET {base}/openapi/v1/billing/bill/list with Authorization: Bearer.
/// We query the lifetime `summary` bill list (one entry per month).
/// JieKou has no account-balance endpoint — we surface total lifetime spend.
#[derive(Deserialize)]
struct JieKouBill {
    #[serde(rename = "startTime")]
    #[allow(dead_code)]
    start_time: Option<String>,
    #[serde(rename = "payAmountDisplay")]
    pay_amount_display: Option<f64>,
}

fn parse_jiekou(body: &serde_json::Value) -> Result<ProviderBalance, String> {
    let mut bills: Vec<JieKouBill> = body
        .get("bills")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|b| serde_json::from_value(b.clone()).ok()).collect())
        .unwrap_or_default();

    if bills.is_empty() {
        return Ok(ProviderBalance {
            provider: "jiekou".into(),
            summary: "$0.00".into(),
            ok: true,
            error: None,
        });
    }

    // API may return bills in any order; sort by start_time ascending.
    bills.sort_by_key(|b| b.start_time.as_deref().unwrap_or("0").to_string());

    let this_month = bills
        .last()
        .and_then(|b| b.pay_amount_display)
        .unwrap_or(0.0);
    let total: f64 = bills.iter().filter_map(|b| b.pay_amount_display).sum();

    let summary = if total > 0.0 {
        format!("本月 ${:.2} · 累计 ${:.2}", this_month, total)
    } else {
        "$0.00".into()
    };

    Ok(ProviderBalance {
        provider: "jiekou".into(),
        summary,
        ok: true,
        error: None,
    })
}

// ── Generic fallback ────────────────────────────────────────────────────────

fn find_generic_balance(body: &serde_json::Value) -> Result<ProviderBalance, String> {
    // Try to find a numeric "balance" or "total_balance" field anywhere
    if let Some(balance) = body.pointer("/data/balance").and_then(|v| v.as_f64()) {
        return Ok(ProviderBalance {
            provider: "unknown".into(),
            summary: format!("{:.2}", balance),
            ok: true,
            error: None,
        });
    }
    if let Some(balance) = body
        .pointer("/data/total_balance")
        .and_then(|v| v.as_str())
    {
        return Ok(ProviderBalance {
            provider: "unknown".into(),
            summary: balance.to_string(),
            ok: true,
            error: None,
        });
    }
    Err("no recognisable balance field".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nonoclaw_engine::ModelProfile;

    fn profile(name: &str, base_url: &str) -> ModelProfile {
        ModelProfile {
            name: name.into(),
            label: None,
            base_url: base_url.into(),
            api_key: "sk-test".into(),
            default: false,
            role: vec![],
            context_window: None,
            max_tokens: None,
            chars_per_token: None,
            profile: None,
            api_format: None,
            billing_provider: None,
        }
    }

    #[test]
    fn provider_matching_from_base_url() {
        assert_eq!(
            model_provider(&profile("kimi-k2", "https://api.moonshot.cn/v1")),
            Some("kimi".into())
        );
        assert_eq!(
            model_provider(&profile("deepseek-chat", "https://api.deepseek.com/v1")),
            Some("deepseek".into())
        );
        assert_eq!(
            model_provider(&profile("glm-4.5-coding", "https://open.bigmodel.cn/api/paas/v4")),
            Some("glm-coding".into())
        );
        assert_eq!(
            model_provider(&profile("glm-4-plus", "https://open.bigmodel.cn/api/paas/v4")),
            Some("glm-api".into())
        );
        assert_eq!(
            model_provider(&profile("claude-sonnet-4-5", "https://api.jiekou.ai/v1")),
            Some("jiekou".into())
        );
        assert_eq!(
            model_provider(&profile("claude-sonnet-4-5", "https://api.highwayapi.ai/anthropic")),
            Some("jiekou".into())
        );
        assert_eq!(
            model_provider(&profile("claude-sonnet-4-5", "https://api.anthropic.com/v1")),
            None
        );
    }

    #[test]
    fn explicit_billing_provider_overrides_base_url() {
        let mut model = profile("claude-sonnet-4-5", "https://api.anthropic.com/v1");
        model.billing_provider = Some("jiekou".into());
        assert_eq!(model_provider(&model), Some("jiekou".into()));
    }

    #[test]
    fn model_provider_map_includes_all_resolvable() {
        let models = vec![
            profile("deepseek-chat", "https://api.deepseek.com/v1"),
            profile("claude-sonnet-4-5", "https://api.anthropic.com/v1"),
            profile("kimi-k2", "https://api.moonshot.cn/v1"),
        ];
        let map = model_provider_map(&models);
        // deepseek + kimi resolved; anthropic has no match → excluded
        assert_eq!(map.len(), 2);
        assert!(map.iter().any(|m| m.model == "deepseek-chat" && m.provider == "deepseek"));
        assert!(map.iter().any(|m| m.model == "kimi-k2" && m.provider == "kimi"));
        assert!(!map.iter().any(|m| m.model == "claude-sonnet-4-5"));
    }

    #[test]
    fn parses_kimi_balance() {
        let body = serde_json::json!({
            "code": 0,
            "data": {"available_balance": 123.45, "voucher_balance": 0, "cash_balance": 123.45}
        });
        let balance = parse_provider_balance("kimi", &body);
        assert!(balance.ok);
        assert_eq!(balance.summary, "¥123.45");
    }

    #[test]
    fn parses_deepseek_balance() {
        // Official API: flat `balance_infos` at top level (no `data`).
        let body = serde_json::json!({
            "is_available": true,
            "balance_infos": [
                {"currency": "CNY", "total_balance": "110.00"}
            ]
        });
        let balance = parse_provider_balance("deepseek", &body);
        assert!(balance.ok);
        assert_eq!(balance.summary, "CNY 110.00");
    }

    #[test]
    fn parses_deepseek_balance_wrapped() {
        // Some gateways wrap the payload in `data`.
        let body = serde_json::json!({
            "data": {
                "balance_infos": [
                    {"currency": "CNY", "total_balance": "99.00"}
                ]
            }
        });
        let balance = parse_provider_balance("deepseek", &body);
        assert!(balance.ok);
        assert_eq!(balance.summary, "CNY 99.00");
    }

    #[test]
    fn parses_glm_coding_balance() {
        let body = serde_json::json!({
            "data": {
                "limits": [
                    {"type": "TOKENS_LIMIT", "percentage": 45.0},
                    {"type": "TIME_LIMIT", "percentage": 20.0}
                ]
            }
        });
        let balance = parse_provider_balance("glm-coding", &body);
        assert!(balance.ok);
        assert!(balance.summary.contains("Token 45%"));
        assert!(balance.summary.contains("MCP 20%"));
    }

    #[test]
    fn parses_jiekou_bills() {
        // Reverse-chronological input: parser should sort by startTime.
        let body = serde_json::json!({
            "bills": [
                {"startTime": "1785542400", "endTime": "1788220799", "payAmountDisplay": 17.55},
                {"startTime": "1782864000", "endTime": "1785542399", "payAmountDisplay": 3.98}
            ]
        });
        let balance = parse_provider_balance("jiekou", &body);
        assert!(balance.ok);
        assert!(balance.summary.contains("本月 $17.55"), "expected 本月 $17.55, got: {}", balance.summary);
        assert!(balance.summary.contains("累计 $21.53"), "expected 累计 $21.53, got: {}", balance.summary);
    }

    #[test]
    fn jiekou_empty_bills_is_ok() {
        let body = serde_json::json!({"bills": []});
        let balance = parse_provider_balance("jiekou", &body);
        assert!(balance.ok);
        assert_eq!(balance.summary, "$0.00");
    }

    #[test]
    fn reports_glm_error_code() {
        // glm-coding with no data entry reports a parse error via the Result
        let body = serde_json::json!({"code": 401, "msg": "unauthorized"});
        let balance = parse_provider_balance("glm-coding", &body);
        assert!(!balance.ok);
        assert!(balance.error.unwrap().contains("missing data"));
    }

    #[test]
    fn glm_api_uses_generic_fallback() {
        let body = serde_json::json!({"data": {"total_balance": "88.00"}});
        let balance = parse_provider_balance("glm-api", &body);
        assert!(balance.ok);
        assert_eq!(balance.summary, "88.00");
        assert_eq!(balance.provider, "glm-api");
    }

    #[test]
    fn generic_fallback_finds_balance() {
        let body = serde_json::json!({"data": {"balance": 99.9}});
        let balance = parse_provider_balance("custom", &body);
        assert!(balance.ok);
        assert_eq!(balance.summary, "99.90");
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use nonoclaw_engine::load_resolved_config;

    /// Opt-in live test: NONOCLAW_BILLING_LIVE=1 cargo test -p nonoclaw --bin nonoclaw live
    #[tokio::test]
    async fn live_query_all_configured_providers() {
        if std::env::var("NONOCLAW_BILLING_LIVE").is_err() {
            return;
        }
        let cwd = std::path::Path::new("/home/baohx");
        let config = load_resolved_config(cwd, None, None);
        let entries = config.provider_billing_entries();
        assert!(!entries.is_empty(), "no providerBilling configured");
        let balances = query_balances(&entries).await;
        for b in &balances {
            eprintln!("[{:?}] ok={} summary={:?} error={:?}", b.provider, b.ok, b.summary, b.error);
        }
        // At least one provider must succeed against the real endpoints.
        assert!(balances.iter().any(|b| b.ok), "all provider queries failed: {balances:#?}");
    }
}
