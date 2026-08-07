//! Live smoke test for balance endpoints (opt-in via NONOCLAW_BILLING_LIVE=1).
use nonoclaw_engine::load_resolved_config;

#[tokio::test]
async fn query_configured_provider_balances_live() {
    if std::env::var("NONOCLAW_BILLING_LIVE").is_err() {
        return;
    }
    let cwd = std::path::Path::new("/home/baohx");
    let config = load_resolved_config(cwd, None, None);
    let entries = config.provider_billing_entries();
    assert!(!entries.is_empty(), "no providerBilling configured");
    for (name, _) in &entries {
        eprintln!("provider configured: {name}");
    }
}
