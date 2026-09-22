//! Jev-assisted run-outcome classification for Level-1 reward labels.
//!
//! The heuristic path (`session::run_reward_labeled`) matches literal
//! substrings in the finish detail. That is brittle in both directions:
//! exhaustion variants not in the table score a false 1.0, and new provider
//! error long-tails are unclassifiable. Jev (TypeSafe AI System One) answers
//! exactly this shape of question — pick one class from a fixed set with
//! calibrated probabilities — at ~$0.00002 per call.
//!
//! Contract: Jev is an *overlay*, never a dependency. Every call site keeps
//! the heuristic as fallback: no key / disabled / network error / low
//! confidence all degrade to the existing score unchanged. The annotated
//! detail records which path produced the label for later offline A/B.

use nonoclaw_api::jev::{JevClient, JevDecision, JevQuestion};

use crate::session::RewardSignals;

/// Install the global Jev client. Called from `main` after settings are
/// resolved; `override_on` from `--jev on/off/auto` wins over the config
/// flag (None = follow config).
pub fn init_global_jev(resolved: &crate::settings::ResolvedConfig, override_on: Option<bool>) {
    let enabled = override_on.or_else(|| {
        resolved
            .settings()
            .jev
            .as_ref()
            .map(|j| j.effective_enabled())
    });
    let client = match enabled {
        Some(true) => {
            match resolved.jev_enabled_config() {
                Some((base_url, api_key, model)) => {
                    tracing::info!(endpoint = %base_url, model = %model, "jev reward classification enabled");
                    Some(JevClient::with_options(&base_url, api_key, &model))
                }
                // --jev on without any key configured: warn, stay heuristic.
                None => {
                    tracing::warn!("--jev on requested but no jev.apiKey in settings; staying on heuristic path");
                    None
                }
            }
        }
        _ => None,
    };
    if let Some(client) = client.as_ref() {
        // Single canonical holder lives in the api crate so retry.rs/ask.rs
        // (which cannot depend on engine) share the same client.
        nonoclaw_api::jev::set_global_client(client.clone());
    }
}

/// Snapshot of the global client for call sites that already hold async
/// context (all run-outcome writers). Cheap `Option<&Arc>`-style clone.
pub fn global_jev() -> Option<JevClient> {
    nonoclaw_api::jev::global_client()
}

/// Choice labels sent to Jev. These map to penalty rows in the exhaustion
/// table; `noise_*` classes demote non-done runs that carry no复盘 value.
pub const CLASS_CLEAN_SUCCESS: &str = "clean_success";
pub const CLASS_TRUNCATION_EXHAUSTED: &str = "truncation_exhausted";
pub const CLASS_TURN_LIMIT: &str = "turn_limit";
pub const CLASS_BUDGET: &str = "budget";
pub const CLASS_CONTEXT_LIMIT: &str = "context_limit";
pub const CLASS_NOISE_RESEND: &str = "noise_resend";
pub const CLASS_PROVIDER_TRANSIENT: &str = "provider_transient";
pub const CLASS_REAL_FAILURE: &str = "real_failure";

/// Below this Jev confidence we ignore the classification. RLCD-calibrated
/// probabilities make this threshold meaningful (unlike LLM self-reported
/// confidence); 0.6 keeps roughly-uncertain cases on the heuristic path.
/// Minimum calibrated confidence for a Jev answer to be trusted. Re-exported
/// from the api crate, which owns the value now (retry/ask paths share it).
pub use nonoclaw_api::jev::JEV_MIN_CONFIDENCE;

/// What Jev decided for one run, for annotation and offline analysis.
#[derive(Debug, Clone)]
pub struct JevRewardVerdict {
    pub chosen_class: String,
    pub confidence: f64,
    /// Full distribution when the question was a Choice.
    pub probabilities: Vec<(String, f64)>,
}

/// Classify one terminal run state via Jev. `None` = unavailable/low
/// confidence → caller falls back to `run_reward_labeled`.
pub async fn classify_run_outcome(
    jev: &JevClient,
    status: &str,
    finish_detail: &str,
    turns: u32,
) -> Option<JevRewardVerdict> {
    // English state: Jev is English-primary (CJK handled but weaker); the
    // finish details we classify are engine literals plus model summaries.
    let state = serde_json::json!({
        "status": status,
        "turns_completed": turns,
        "finish_detail": finish_detail,
        "context": "Terminal metadata for one coding-agent run inside the NonoClaw harness. Classify how this run ended so its trajectory-level reward label can be assigned.",
    });
    let question = JevQuestion::choice(
        "Classify how this coding-agent run ended. Choose exactly one.",
        &[
            (CLASS_CLEAN_SUCCESS, "The run converged and produced its promised output or change; no exhaustion signal in the detail."),
            (CLASS_TRUNCATION_EXHAUSTED, "Output was cut off mid-generation (mid-thinking or per-turn max_tokens stop reason) — the model ran out of its per-turn output budget before producing usable content."),
            (CLASS_TURN_LIMIT, "The agent hit the maximum number of turns without converging."),
            (CLASS_BUDGET, "The run stopped because a monetary or token budget was exhausted."),
            (CLASS_CONTEXT_LIMIT, "The run stopped because the context window filled up."),
            (CLASS_NOISE_RESEND, "The run was cancelled immediately (≈0 turns) — user resent the prompt or touched cancel by accident; no failure cause to mine."),
            (CLASS_PROVIDER_TRANSIENT, "The run died from a transient provider/network error, not from agent behavior."),
            (CLASS_REAL_FAILURE, "The run errored for a real reason attributable to the agent or task (not transient, not noise)."),
        ],
    );
    let answers = match jev
        .ask(&state.to_string(), &[("outcome_class".into(), question)])
        .await
    {
        Ok(answers) => answers,
        Err(e) => {
            tracing::debug!(status, turns, error = %e, "jev run-outcome classification unavailable — heuristic kept");
            return None;
        }
    };
    let Some(answer) = answers.get("outcome_class") else {
        tracing::debug!(
            status,
            turns,
            "jev run-outcome answer missing — heuristic kept"
        );
        return None;
    };
    let Some(chosen) = answer.choice.clone() else {
        tracing::debug!(
            status,
            turns,
            "jev run-outcome returned no choice — heuristic kept"
        );
        return None;
    };
    let confidence = answer.confidence.unwrap_or(0.0);
    if confidence < JEV_MIN_CONFIDENCE {
        tracing::debug!(status, turns, class = %chosen, confidence, "jev run-outcome below confidence floor — heuristic kept");
        return None;
    }
    tracing::debug!(status, turns, class = %chosen, confidence, "jev run-outcome classified");
    Some(JevRewardVerdict {
        chosen_class: chosen,
        confidence,
        probabilities: answer
            .probabilities
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
    })
}

/// Map a Jev class to a reward override. `None` means "no override — use the
/// heuristic score". Overrides deliberately stay inside the existing reward
/// bands so downstream consumers (dream briefs, ledger stats) see no scale
/// change; Jev only *corrects class assignment*, not the scale.
pub fn reward_for_class(class: &str, status: &str) -> Option<f64> {
    match (status, class) {
        // done + truncation/turn-limit are the false-1.0 bug class: override.
        ("done", CLASS_TRUNCATION_EXHAUSTED) => Some(0.6),
        ("done", CLASS_TURN_LIMIT) => Some(0.6),
        ("done", CLASS_BUDGET) => Some(0.7),
        ("done", CLASS_CONTEXT_LIMIT) => Some(0.8),
        // done + transient provider error surfaced as done: correct down.
        ("done", CLASS_PROVIDER_TRANSIENT) => Some(0.0),
        // Non-done noise: demote to 0 so dream briefs stop ranking resends.
        ("cancelled", CLASS_NOISE_RESEND) => Some(0.0),
        // Everything else keeps the heuristic score.
        _ => None,
    }
}

/// Full pipeline used by run-completion write paths: heuristic label, then
/// Jev classification overlay when a client is available. Returns
/// `(reward, annotated_detail)`. The annotation is appended to detail so the
/// JSONL ledger records which path scored each run (offline A/B material).
pub async fn run_reward_with_jev(
    status: &str,
    finish_detail: &str,
    turns: u32,
    signals: &RewardSignals,
) -> (f64, String) {
    let jev = global_jev();
    let heuristic = crate::session::run_reward_labeled(status, finish_detail, signals);
    let Some(jev) = jev else {
        return (heuristic, finish_detail.to_string());
    };
    match classify_run_outcome(&jev, status, finish_detail, turns).await {
        Some(verdict) => match reward_for_class(&verdict.chosen_class, status) {
            Some(override_reward) => {
                let detail = format!(
                    "{finish_detail} [jev: class={} conf={:.2} p(trunc)={:.2} reward {}→{}]",
                    verdict.chosen_class,
                    verdict.confidence,
                    verdict
                        .probabilities
                        .iter()
                        .find(|(k, _)| k == CLASS_TRUNCATION_EXHAUSTED)
                        .map(|&(_, v)| v)
                        .unwrap_or(0.0),
                    heuristic,
                    override_reward
                );
                (override_reward, detail)
            }
            None => {
                let detail = format!(
                    "{finish_detail} [jev: class={} conf={:.2} heuristic kept]",
                    verdict.chosen_class, verdict.confidence
                );
                (heuristic, detail)
            }
        },
        None => (heuristic, finish_detail.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_overrides_stay_in_bands() {
        assert_eq!(
            reward_for_class(CLASS_TRUNCATION_EXHAUSTED, "done"),
            Some(0.6)
        );
        assert_eq!(reward_for_class(CLASS_NOISE_RESEND, "cancelled"), Some(0.0));
        assert_eq!(reward_for_class(CLASS_CLEAN_SUCCESS, "done"), None);
        // unknown class → heuristic
        assert_eq!(reward_for_class("something_new", "done"), None);
    }

    #[tokio::test]
    async fn no_client_means_pure_heuristic() {
        // Default global state (no init) → heuristic path, detail untouched.
        let (reward, detail) =
            run_reward_with_jev("done", "max turns reached", 12, &RewardSignals::default()).await;
        assert_eq!(reward, 0.6);
        assert_eq!(detail, "max turns reached", "detail untouched without jev");
    }

    #[test]
    fn low_confidence_verdicts_are_dropped() {
        // The 0.5-confidence case must be treated as unavailable.
        assert!(0.5 < JEV_MIN_CONFIDENCE);
    }
}
