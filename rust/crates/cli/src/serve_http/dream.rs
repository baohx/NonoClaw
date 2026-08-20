//! AutoDream — background memory consolidation ("dreaming") scheduler.
//!
//! Inspired by the AutoDream/Dream Memory system revealed in the Claude Code
//! source leak: when the user has been idle for a while and no work is
//! happening, the server quietly launches a headless "dream" run that walks
//! recent session transcripts, correlates fragments, distills reusable
//! knowledge into `memory/facts/`, and refreshes the session vector index —
//! so the next session starts with organized long-term memory.
//!
//! Trigger conditions (all must hold, checked every minute):
//!   1. No run in flight (no WS session peers, no pending permissions)
//!   2. Last user activity is older than `dreamIdleMinutes` (default 10)
//!   3. New session files exist since the last dream (or first run)
//!   4. Opt-in enabled via `dreamEnabled` in settings (default true)
//!
//! The dream itself is a normal REST run (same handler path as external
//! automation) with a fixed four-phase prompt, restricted to read + Memory
//! tools, low max_turns.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;

use super::connection::AppState;

/// Default idle threshold before a dream may start.
const DEFAULT_IDLE_MINUTES: u64 = 10;
/// How often the watcher loop re-evaluates trigger conditions.
const TICK: Duration = Duration::from_secs(60);
/// Turn cap for the dream run — it summarizes, it does not work.
const DREAM_MAX_TURNS: u32 = 16;

/// How far back the reward brief looks for run_outcome labels (24h). Long
/// enough to always cover the gap between dreams; short enough to keep the
/// brief about recent behaviour.
const DREAM_BRIEF_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// Marker recording when the last dream ran; stored in the project state dir.
fn dream_marker_path(cwd: &Path) -> Option<PathBuf> {
    nonoclaw_engine::session::project_dir(cwd).map(|d| d.join("last_dream.json"))
}

/// Fixed dream prompt: four-phase REM consolidation. `reward_brief` is the
/// Level-1 RL signal summary (aggregates + pointers, never trajectory text)
/// that steers which fragments the dream reviews first.
#[cfg(test)]
pub(super) fn dream_prompt() -> String {
    dream_prompt_with_brief(None)
}

pub(super) fn dream_prompt_with_brief(reward_brief: Option<String>) -> String {
    let brief = reward_brief.unwrap_or_default();
    format!(
        "\
你正在执行 AutoDream 后台记忆整理（用户离线期间运行）。严格按四个阶段工作，全程只读 + 写记忆，不修改任何项目代码：\n\n\
{brief}\
1. 【碎片收集】优先检索 Reward 简报里列出的低 reward 轨迹（session_search 用其 detail 中的关键词：取消原因、错误信息）；再做常规收集：用 Memory session_search 检索最近的会话片段（多个关键词：最近的 bug、修复、决策、配置、用户反馈）。用 Bash `ls -t` 看最近改动的文件。\n\
2. 【关联分析】找出碎片之间的关联：重复出现的错误模式、前后因果（如旧配置问题和后续报错）、跨会话重复做的事。若简报里有失败/被打断的轨迹，对照同类任务的成功轨迹找差异（这类任务怎么做会失败）。\n\
3. 【知识萃取】只把【可复用、非显而易见】的知识提炼为结构化事实：类型选 preference/convention/decision/architecture/bug。写法遵循 memory/facts 的 YAML frontmatter 格式，importance 1-5。\n\
4. 【记忆索引】用 Write 工具把每条事实写入 memory/facts/<slug>.md。\n\n\
纪律：\\
- 不要重复已有事实：先 Grep memory/facts/ 确认；如有近似事实，用 supersedes 取代而不是新增。\n\
- reward 标签（run_outcome 条目）本身是数据不是知识——萃取的是轨迹里【导致成功/失败的做法】。\n\
- 单次 dream 最多产出 3 条事实，宁缺毋滥；没有值得萃取的就一个都不写\n\
- 事件类/一次性信息不要写成事实\n\
- 完成后输出一行总结：检查了几个片段、萃取了几条事实（或为何不萃取）\n\
"
    )
}

/// Coarse fingerprint of the session directory: (count, latest mtime).
fn session_fingerprint(dir: &Path) -> Option<(usize, SystemTime)> {
    let mut count = 0usize;
    let mut latest: Option<SystemTime> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
            count += 1;
            if let Ok(meta) = entry.metadata() {
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                latest = Some(match latest {
                    Some(cur) if cur >= mtime => cur,
                    _ => mtime,
                });
            }
        }
    }
    latest.map(|l| (count, l))
}

/// One parsed `run_outcome` metadata line from a session JSONL.
#[derive(Debug, Clone)]
struct OutcomeSummary {
    session_id: String,
    run_id: String,
    status: String,
    reward: f64,
    detail: String,
}

/// Scan session files for `run_outcome` entries (Level-1 RL labels) newer
/// than `since`, skipping dream-tagged sessions (a dream's own outcome would
/// inflate the brief). Returns outcomes sorted by reward ascending — the
/// lowest-reward trajectories first, since those deserve the deepest review.
fn scan_run_outcomes(
    dir: &Path,
    since: SystemTime,
) -> Vec<OutcomeSummary> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).ok().into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.modified().unwrap_or(SystemTime::UNIX_EPOCH) < since {
            continue;
        }
        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        // Skip dream sessions so the brief only describes real work.
        if text.contains("\"tag\":\"dream\"") {
            continue;
        }
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if value.get("kind").and_then(|k| k.as_str()) != Some("run_outcome") {
                continue;
            }
            out.push(OutcomeSummary {
                session_id: session_id.clone(),
                run_id: value
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                status: value
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                reward: value.get("reward").and_then(|v| v.as_f64()).unwrap_or(0.0),
                detail: value
                    .get("detail")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect(),
            });
        }
    }
    out.sort_by(|a, b| a.reward.partial_cmp(&b.reward).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// Build the reward brief injected into the dream prompt. Aggregates terminal
/// outcomes and points at the worst trajectories for targeted review.
fn reward_brief(dir: &Path, since: SystemTime) -> String {
    let outcomes = scan_run_outcomes(dir, since);
    if outcomes.is_empty() {
        return "【Reward 简报】上次 dream 以来没有带 reward 标签的新轨迹（旧 session 可能无标签），按常规四阶段整理。".to_string();
    }
    let done = outcomes.iter().filter(|o| o.status == "done").count();
    let cancelled = outcomes
        .iter()
        .filter(|o| o.status == "cancelled")
        .count();
    let error = outcomes.iter().filter(|o| o.status == "error").count();
    let mut brief = format!(
        "【Reward 简报】上次 dream 以来的 run 结局：done × {done}，cancelled × {cancelled}，error × {error}。\n"
    );
    // Worst 3 trajectories (lowest reward first) for targeted review.
    let worst: Vec<&OutcomeSummary> = outcomes
        .iter()
        .filter(|o| o.status != "done")
        .take(3)
        .collect();
    if !worst.is_empty() {
        brief.push_str("重点复盘（低 reward 轨迹，优先检索分析失败/被打断的原因）：\n");
        for o in worst {
            brief.push_str(&format!(
                "- session {} · run {}（{}，reward {:.1}）：{}\n",
                &o.session_id.chars().take(8).collect::<String>(),
                &o.run_id.chars().take(8).collect::<String>(),
                o.status,
                o.reward,
                o.detail
            ));
        }
    } else {
        brief.push_str("全部成功。萃取最近成功轨迹的工具使用模式（怎么做对的）。\n");
    }
    // Cap the brief so it cannot grow unboundedly with session count.
    brief.chars().take(600).collect()
}

#[derive(Default)]
struct DreamState {
    /// Fingerprint at the moment the last dream finished.
    last_fingerprint: Option<(usize, SystemTime)>,
    /// True while a dream run is in flight (prevents re-entry).
    dreaming: bool,
}

/// Spawn the idle watcher. `last_activity` is updated by every inbound
/// client message (WS + REST entrypoints touch it via `touch_activity`).
pub(super) fn spawn_dream_scheduler(state: Arc<AppState>, last_activity: Arc<Mutex<SystemTime>>) {
    // Config: enable + idle threshold. Read once at startup — dream cadence
    // does not need hot reload.
    let settings = state.config.settings();
    let enabled = settings.dream_enabled.unwrap_or(true);
    let idle_minutes = settings.dream_idle_minutes.unwrap_or(DEFAULT_IDLE_MINUTES);
    if !enabled {
        tracing::info!(idle_minutes, "dream scheduler disabled by settings");
        return;
    }

    let cwd = state.cwd.clone();
    tokio::spawn(async move {
        let mut dream = DreamState::default();
        // Startup grace period: never dream in the first interval.
        tokio::time::sleep(TICK).await;
        tracing::info!(idle_minutes, "dream scheduler watching for idle");
        loop {
            tokio::time::sleep(TICK).await;
            if dream.dreaming {
                continue;
            }
            // Condition 1: idle long enough.
            let idle_for = last_activity
                .lock()
                .await
                .elapsed()
                .unwrap_or_default();
            if idle_for < Duration::from_secs(idle_minutes * 60) {
                continue;
            }
            // Condition 2: no active work — no pending permissions/questions,
            // no background bash tasks.
            if !state.pending_permissions.lock().await.is_empty()
                || !state.pending_questions.lock().await.is_empty()
                || state
                    .background_registry
                    .lock()
                    .map(|r| {
                        r.list_tasks()
                            .iter()
                            .any(|t| !t.status.is_terminal())
                    })
                    .unwrap_or(false)
            {
                continue;
            }
            // Condition 3: fresh session material since the last dream.
            let Some(sessions_dir) = nonoclaw_engine::session::home_root().map(|r| {
                r.join("projects")
                    .join(
                        cwd.to_string_lossy()
                            .trim_start_matches('/')
                            .replace('/', "-"),
                    )
                    .join("sessions")
            }) else {
                continue;
            };
            let Some(fp) = session_fingerprint(&sessions_dir) else {
                continue;
            };
            if dream.last_fingerprint == Some(fp) {
                continue;
            }

            // All conditions hold — dream.
            dream.dreaming = true;
            let state2 = Arc::clone(&state);
            let ok = run_dream(state2).await;
            // Refresh the session index (Layer 3) with anything new, then
            // stamp the fingerprint regardless of success so a failing
            // dream does not hot-loop.
            {
                let cwd2 = cwd.clone();
                let dir = sessions_dir.clone();
                std::thread::spawn(move || {
                    let index = nonoclaw_tools::session_index::build_index(&cwd2, &dir);
                    tracing::debug!(
                        chunks = index.chunks.len(),
                        "post-dream session index refreshed"
                    );
                });
            }
            if let Some(marker) = dream_marker_path(&cwd) {
                let _ = std::fs::write(
                    &marker,
                    format!(
                        "{{\"finished_at\":{}}}",
                        SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    ),
                );
            }
            dream.last_fingerprint = session_fingerprint(&sessions_dir).or(Some(fp));
            dream.dreaming = false;
            if ok {
                tracing::info!("dream run finished");
            }
        }
    });
}

/// Launch the dream as a REST run via the same handler path used by external
/// automation (in-process — no HTTP self-call).
async fn run_dream(state: Arc<AppState>) -> bool {
    let model = state.active_model.lock().await.clone();
    // Reward-guided brief: aggregate Level-1 RL labels since the last dream
    // so the dream reviews the worst trajectories first. Falls back to the
    // plain prompt when the sessions dir is unavailable.
    let brief = nonoclaw_engine::session::home_root().map(|root| {
        let sessions_dir = root
            .join("projects")
            .join(
                state
                    .cwd
                    .to_string_lossy()
                    .trim_start_matches('/')
                    .replace('/', "-"),
            )
            .join("sessions");
        let since = SystemTime::now() - DREAM_BRIEF_WINDOW;
        reward_brief(&sessions_dir, since)
    });
    let req = super::run_api::RunRequest {
        prompt: dream_prompt_with_brief(brief),
        session_id: None,
        model: Some(model),
        max_turns: Some(DREAM_MAX_TURNS),
        append_system_prompt: None,
        arguments: None,
        // Read-mostly autonomy: the dream writes facts via the Write tool,
        // which `auto` permits after the standard edit gate.
        permission_mode: Some("auto".into()),
        dream: true,
    };
    // Drive the NDJSON stream to completion; we only care that it finishes.
    let resp = match super::run_api::run_handler_for_dream(Arc::clone(&state), req).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(error = %e, "dream run failed to start");
            return false;
        }
    };
    tracing::info!("dream run started");
    // Consume the body so the run actually executes to completion: collect
    // via into_data_stream (the same stream type run_api built it from).
    let mut stream = resp.into_body().into_data_stream();
    let mut ok = true;
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        if chunk.is_err() {
            tracing::warn!("dream run stream error");
            ok = false;
            break;
        }
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reward brief: aggregates outcomes, lists worst-first, skips dream
    /// sessions, and degrades gracefully when there is nothing to review.
    #[test]
    fn reward_brief_aggregates_and_targets_worst() {
        let dir = std::env::temp_dir().join("dream_reward_brief_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();

        let work = dir.join("work11111111-aaaa.jsonl");
        std::fs::write(
            &work,
            concat!(
                "{\"kind\":\"session\",\"id\":\"w\"}\n",
                "{\"kind\":\"run_outcome\",\"run_id\":\"r1\",\"status\":\"done\",\"reward\":1.0,\"turns\":2,\"detail\":\"ok\"}\n",
                "{\"kind\":\"run_outcome\",\"run_id\":\"r2\",\"status\":\"error\",\"reward\":-1.0,\"turns\":0,\"detail\":\"provider 500\"}\n",
            ),
        )
        .unwrap();
        let other = dir.join("other22222222-bbbb.jsonl");
        std::fs::write(
            &other,
            concat!(
                "{\"kind\":\"session\",\"id\":\"o\"}\n",
                "{\"kind\":\"run_outcome\",\"run_id\":\"r3\",\"status\":\"cancelled\",\"reward\":-0.3,\"turns\":0,\"detail\":\"user requested cancellation\"}\n",
            ),
        )
        .unwrap();
        // A dream session whose own outcome must NOT be counted.
        let dream = dir.join("dream33333333-cccc.jsonl");
        std::fs::write(
            &dream,
            concat!(
                "{\"kind\":\"tag\",\"tag\":\"dream\"}\n",
                "{\"kind\":\"run_outcome\",\"run_id\":\"r4\",\"status\":\"done\",\"reward\":1.0,\"turns\":9,\"detail\":\"dream done\"}\n",
            ),
        )
        .unwrap();
        // A session with a pre-label (old) mtime: excluded by `since`.
        let stale = dir.join("stale44444444-dddd.jsonl");
        std::fs::write(
            &stale,
            "{\"kind\":\"run_outcome\",\"run_id\":\"r5\",\"status\":\"done\",\"reward\":1.0,\"turns\":1,\"detail\":\"old\"}\n",
        )
        .unwrap();
        let old = std::fs::File::options().write(true).open(&stale).unwrap();
        old.set_modified(now - DREAM_BRIEF_WINDOW - Duration::from_secs(600))
            .unwrap();
        drop(old);

        let brief = reward_brief(&dir, now - DREAM_BRIEF_WINDOW);
        assert!(brief.contains("done × 1"), "done count excludes dream+stale: {brief}");
        assert!(brief.contains("cancelled × 1"), "cancelled count: {brief}");
        assert!(brief.contains("error × 1"), "error count: {brief}");
        assert!(brief.contains("work1111"), "worst-first pointer to work session: {brief}");
        assert!(brief.contains("other222"), "pointer to cancelled session: {brief}");
        assert!(!brief.contains("dream3333"), "dream session excluded");
        assert!(!brief.contains("r5"), "stale outcome excluded");
        assert!(brief.chars().count() <= 620, "brief capped: {}", brief.chars().count());
    }

    #[test]
    fn reward_brief_empty_when_no_outcomes() {
        let dir = std::env::temp_dir().join("dream_reward_brief_empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let brief = reward_brief(&dir, SystemTime::now() - DREAM_BRIEF_WINDOW);
        assert!(brief.contains("没有"), "degraded brief explains absence: {brief}");
    }

    #[test]
    fn dream_prompt_embeds_reward_brief_and_keeps_phases() {
        let plain = dream_prompt();
        for phase in ["碎片收集", "关联分析", "知识萃取", "记忆索引"] {
            assert!(plain.contains(phase), "missing phase {phase}");
        }
        // The brief (with its 【Reward 简报】 header) only appears when provided;
        // the plain prompt mentions "Reward 简报" only in phase-1 guidance,
        // never as an actual injected section.
        assert!(!plain.contains("【Reward 简报】"), "plain prompt has no brief section");
        let guided = dream_prompt_with_brief(Some(
            "【Reward 简报】done × 3，cancelled × 1，error × 0。\n重点复盘：\n- session abcdef12 · run 12345678（cancelled，reward -0.3）：user stop\n".into(),
        ));
        assert!(guided.contains("abcdef12"), "brief pointer embedded");
        assert!(guided.contains("碎片收集"));
        // Brief must come before the phases so it frames them.
        assert!(guided.find("Reward 简报").unwrap() < guided.find("碎片收集").unwrap());
    }

    #[test]
    fn fingerprint_changes_when_sessions_change() {
        let dir = std::env::temp_dir().join("dream_fp_test");
        std::fs::create_dir_all(&dir).unwrap();
        for f in dir.read_dir().unwrap().flatten() {
            let _ = std::fs::remove_file(f.path());
        }
        let a = dir.join("a.jsonl");
        std::fs::write(&a, "x").unwrap();
        let fp1 = session_fingerprint(&dir).unwrap();
        assert_eq!(fp1.0, 1);
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("b.jsonl"), "y").unwrap();
        let fp2 = session_fingerprint(&dir).unwrap();
        assert_eq!(fp2.0, 2);
        assert_ne!(fp1, fp2);
        // No change → same fingerprint (idempotent, no re-dream).
        assert_eq!(session_fingerprint(&dir).unwrap(), fp2);
    }

    #[test]
    fn non_jsonl_files_ignored() {
        let dir = std::env::temp_dir().join("dream_fp_test2");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        assert!(session_fingerprint(&dir).is_none()); // no jsonl → None
    }

    #[test]
    fn dream_prompt_has_four_phases() {
        let p = dream_prompt();
        for phase in ["碎片收集", "关联分析", "知识萃取", "记忆索引"] {
            assert!(p.contains(phase), "missing phase {phase}");
        }
    }
}
