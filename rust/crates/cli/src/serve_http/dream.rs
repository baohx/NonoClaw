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

/// Marker recording when the last dream ran; stored in the project state dir.
fn dream_marker_path(cwd: &Path) -> Option<PathBuf> {
    nonoclaw_engine::session::project_dir(cwd).map(|d| d.join("last_dream.json"))
}

/// Fixed dream prompt: four-phase REM consolidation.
pub(super) fn dream_prompt() -> String {
    "\
你正在执行 AutoDream 后台记忆整理（用户离线期间运行）。严格按四个阶段工作，全程只读 + 写记忆，不修改任何项目代码：\n\n\
1. 【碎片收集】用 Memory session_search 检索最近的会话片段（多个关键词：最近的 bug、修复、决策、配置、用户反馈）。用 Bash `ls -t` 看最近改动的文件。\n\
2. 【关联分析】找出碎片之间的关联：重复出现的错误模式、前后因果（如旧配置问题和后续报错）、跨会话重复做的事。\n\
3. 【知识萃取】只把【可复用、非显而易见】的知识提炼为结构化事实：类型选 preference/convention/decision/architecture/bug。写法遵循 memory/facts 的 YAML frontmatter 格式，importance 1-5。\n\
4. 【记忆索引】用 Write 工具把每条事实写入 memory/facts/<slug>.md。\n\n\
纪律：\n\
- 不要重复已有事实：先 Grep memory/facts/ 检查相似条目，重复就跳过\n\
- 单次 dream 最多产出 3 条事实，宁缺毋滥；没有值得萃取的就一个都不写\n\
- 事件类/一次性信息不要写成事实\n\
- 完成后输出一行总结：检查了几个片段、萃取了几条事实（或为何不萃取）"
        .to_string()
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
    let req = super::run_api::RunRequest {
        prompt: dream_prompt(),
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
