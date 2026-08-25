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

// ── Bench-validated fact loop (Skill-MAS S* selection) ────────────────────
//
// Orchestration facts change HOW the agent plans work; a regression in the
// smoke bench is objective evidence the new guidance hurts more than helps.
// After each dream that wrote facts we re-run the local terminal-bench smoke
// harness and compare pass rates:
//   rate ≥ history  → keep facts, record the new baseline
//   rate <  history → supersede the newest fact (roll back) and keep history
// Skipped entirely when the harness or python3 is missing — the loop is an
// enhancement, never a blocker for dreaming.

/// Where the smoke harness lives relative to the workspace root.
const BENCH_SMOKE_SCRIPT: &str = "bench/terminal-bench/run_local_smoke.py";
/// History file (per project dir) tracking the last accepted pass rate.
const BENCH_HISTORY_FILE: &str = "bench_history.json";
/// Pass-rate drop (absolute) that triggers a fact rollback.
const BENCH_REGRESSION_THRESHOLD: f64 = 0.34; // 1 of 3 tasks
/// Timeout for one harness invocation.
const BENCH_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Latest line of the harness stdout: `=== N/M tasks passed ===`.
fn parse_pass_rate(stdout: &str) -> Option<f64> {
    for line in stdout.lines().rev() {
        let rest = line.trim().strip_prefix("===")?.trim();
        let rest = rest.trim_end_matches('=').trim();
        let rest = rest.strip_suffix("tasks passed")?.trim();
        let (n, m) = rest.split_once('/')?;
        let n: f64 = n.trim().parse().ok()?;
        let m: f64 = m.trim().parse().ok()?;
        if m > 0.0 {
            return Some(n / m);
        }
    }
    None
}

fn read_pass_rate(path: &Path) -> Option<f64> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("pass_rate")?.as_f64()
}

fn write_pass_rate(path: &Path, rate: f64, rolled_back: bool) {
    let v = serde_json::json!({
        "pass_rate": rate,
        "updated_at": SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "last_rollback": rolled_back,
    });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, v.to_string());
}

/// Newest fact file (by mtime) in the project's facts dir.
fn newest_fact(facts_dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(facts_dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let mtime = entry.metadata().ok()?.modified().ok()?;
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

/// Locate the workspace root (the ancestor directory holding the bench
/// harness) for bench validation. `None` outside a NonoClaw checkout.
fn workspace_root_of(cwd: &Path) -> Option<PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(BENCH_SMOKE_SCRIPT).is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Run the smoke harness, returning its stdout (empty on spawn failure).
fn run_bench(workspace_root: &Path) -> Option<String> {
    let script = workspace_root.join(BENCH_SMOKE_SCRIPT);
    if !script.is_file() {
        tracing::debug!(script = %script.display(), "bench smoke harness missing, skip validation");
        return None;
    }
    let out = std::process::Command::new("python3")
        .arg(&script)
        .current_dir(workspace_root)
        .output();
    match out {
        Ok(out) => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Err(e) => {
            tracing::debug!(error = %e, "bench harness failed to spawn, skip validation");
            None
        }
    }
}

/// Validate freshly written dream facts against the smoke bench. Runs at most
/// once per dream, never blocks the dream itself (all failures are soft).
fn bench_validate_facts(workspace_root: &Path, project_dir: &Path) {
    let Some(stdout) = run_bench(workspace_root) else {
        return;
    };
    let Some(rate) = parse_pass_rate(&stdout) else {
        tracing::debug!("bench harness produced no pass-rate line, skip validation");
        return;
    };
    let history_path = project_dir.join(BENCH_HISTORY_FILE);
    match read_pass_rate(&history_path) {
        None => {
            write_pass_rate(&history_path, rate, false);
            tracing::info!(pass_rate = rate, "bench baseline recorded");
        }
        Some(prev) => {
            if rate + f64::EPSILON >= prev {
                write_pass_rate(&history_path, rate, false);
                tracing::info!(pass_rate = rate, previous = prev, "bench validated, facts kept");
                return;
            }
            // Regression: roll back the newest fact.
            let facts_dir = project_dir.join(".nonoclaw/memory/facts");
            if let Some(fact) = newest_fact(&facts_dir) {
                let name = fact
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                let rollback = format!("{}.rollback", name);
                let result = nonoclaw_tools::memory::supersede_fact_by_path(
                    &fact,
                    &rollback,
                    "bench regression: smoke pass rate dropped",
                );
                match result {
                    Ok(()) => {
                        tracing::warn!(fact = %name, previous = prev, now = rate, "bench regression — fact rolled back");
                        write_pass_rate(&history_path, prev, true);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "bench rollback failed; recording regression anyway");
                        write_pass_rate(&history_path, rate, true);
                    }
                }
            } else {
                tracing::warn!(previous = prev, now = rate, "bench regression but no fact to roll back");
                write_pass_rate(&history_path, rate, true);
            }
        }
    }
}


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
2. 【关联分析】找出碎片之间的关联：重复出现的错误模式、前后因果（如旧配置问题和后续报错）、跨会话重复做的事。若简报里有失败/被打断的轨迹，做对比反思：检索同类任务的成功轨迹，高分 vs 低分逐段对照，定位第一个分歧点——是哪个编排决策（任务拆解方式、子代理/工具选择、步骤顺序）不同导致结果分岔。\n\
3. 【知识萃取】只把【可复用、非显而易见】的知识提炼为结构化事实：类型选 preference/convention/decision/architecture/bug。写法遵循 memory/facts 的 YAML frontmatter 格式，importance 1-5。\n\
4. 【记忆索引】用 Write 工具把每条事实写入 memory/facts/<slug>.md。\n\n\
纪律：\\
- 不要重复已有事实：先 Grep memory/facts/ 确认；如有近似事实，用 supersedes 取代而不是新增。\n\
- 通用性门槛：每条事实写之前自检——换个任务/换个项目这条还成立吗？只写通用原则，不写任务特定 trick（如「X 文件要改 Y 行」）。不成立的信息留在总结输出里，不写入 facts。\n\
- 编排经验也是知识：如果失败/成功的根因在编排层（任务拆得太碎/太粗、该 fan-out 却串行、子代理轮次不够、验证步骤缺失/冗余），把它提炼为 convention/decision 类事实（如「多文件重构类任务先 fan-out 只读探查再汇总修改」），供未来同类任务的编排参考。\n\
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
    /// Kept for log/debug fidelity even though the brief aggregates by session.
    #[allow(dead_code)]
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

/// Per-session aggregate of run outcomes — the "task" analogue of the
/// Skill-MAS rollout distribution: difficulty = negated mean reward (how
/// badly this session's runs fared), uncertainty = std of rewards (how
/// inconsistently it fared). A single-run session has zero uncertainty and
/// is ranked by difficulty alone; a session with mixed done/cancelled/error
/// outcomes is volatile even if its mean is acceptable.
#[derive(Debug, Clone)]
struct SessionStats {
    session_id: String,
    runs: usize,
    mean: f64,
    std: f64,
    /// Status + detail of the lowest-reward run (the review entry point).
    worst_status: String,
    worst_detail: String,
    /// (û + d̂) / 2 after min–max normalization across sessions.
    priority: f64,
}

const MAX_REVIEW_SESSIONS: usize = 4;

fn aggregate_sessions(outcomes: &[OutcomeSummary]) -> Vec<SessionStats> {
    // Group by session, preserving insertion order of first appearance.
    let mut order: Vec<String> = Vec::new();
    let mut grouped: std::collections::HashMap<String, Vec<&OutcomeSummary>> =
        std::collections::HashMap::new();
    for o in outcomes {
        if !grouped.contains_key(&o.session_id) {
            order.push(o.session_id.clone());
        }
        grouped.entry(o.session_id.clone()).or_default().push(o);
    }

    let mut sessions: Vec<SessionStats> = order
        .into_iter()
        .filter_map(|id| {
            let runs = grouped.remove(&id)?;
            let n = runs.len() as f64;
            let mean = runs.iter().map(|r| r.reward).sum::<f64>() / n;
            let var = runs.iter().map(|r| (r.reward - mean).powi(2)).sum::<f64>() / n;
            let worst = runs.iter().min_by(|a, b| {
                a.reward
                    .partial_cmp(&b.reward)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })?;
            Some(SessionStats {
                session_id: id,
                runs: runs.len(),
                mean,
                std: var.sqrt(),
                worst_status: worst.status.clone(),
                worst_detail: worst.detail.clone(),
                priority: 0.0,
            })
        })
        .collect();

    // Min–max normalize uncertainty and difficulty across sessions, then
    // blend into a unified priority (Skill-MAS §3.3.1). Ties (max==min)
    // normalize to 0.5 so neither axis dominates artificially.
    let norm = |vals: &[f64], v: f64| -> f64 {
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if max > min {
            (v - min) / (max - min)
        } else {
            0.5
        }
    };
    let stds: Vec<f64> = sessions.iter().map(|s| s.std).collect();
    let diffs: Vec<f64> = sessions.iter().map(|s| -s.mean).collect();
    for s in &mut sessions {
        let û = norm(&stds, s.std);
        let d̂ = norm(&diffs, -s.mean);
        s.priority = (û + d̂) / 2.0;
    }

    sessions.sort_by(|a, b| {
        b.priority
            .partial_cmp(&a.priority)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.mean.partial_cmp(&b.mean).unwrap_or(std::cmp::Ordering::Equal))
    });
    sessions
}

/// Elbow truncation over a descending-sorted priority curve (Skill-MAS
/// §3.3.1): first-order differences δⱼ = pⱼ − pⱼ₊₁, elbow at the maximum
/// absolute second-order difference; select the top j* sessions. Fewer than
/// 3 sessions give no second-order signal — take all (bounded by the cap).
fn elbow_select(priorities_desc: &[f64]) -> usize {
    let n = priorities_desc.len().min(MAX_REVIEW_SESSIONS);
    if n < 3 {
        return n;
    }
    let deltas: Vec<f64> = (0..n - 1).map(|j| priorities_desc[j] - priorities_desc[j + 1]).collect();
    let mut best_j = n; // fallback: keep everything
    let mut best = 1e-12; // curvature must be non-trivial to count as an elbow
    for j in 0..deltas.len() - 1 {
        let curvature = (deltas[j] - deltas[j + 1]).abs();
        if curvature > best {
            best = curvature;
            best_j = j + 1;
        }
    }
    best_j.clamp(1, n)
}

/// Build the reward brief injected into the dream prompt. Skill-MAS-style:
/// aggregate outcomes per session into (uncertainty, difficulty), rank by a
/// blended priority, elbow-truncate to the most informative subset, and
/// instruct contrastive (high-vs-low trajectory) reflection.
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
    let sessions = aggregate_sessions(&outcomes);
    let mut brief = format!(
        "【Reward 简报】上次 dream 以来 run 结局：done × {done}，cancelled × {cancelled}，error × {error}（{} 个 session）。\n",
        sessions.len()
    );
    let failed: Vec<&SessionStats> = sessions.iter().filter(|s| s.mean < 1.0).collect();
    if !failed.is_empty() {
        let priorities: Vec<f64> = sessions.iter().map(|s| s.priority).collect();
        let selected = elbow_select(&priorities);
        brief.push_str(&format!(
            "重点复盘（优先级 = 不稳定度×难度，elbow 截断选前 {selected} 个）：\n"
        ));
        for s in sessions.iter().take(selected) {
            brief.push_str(&format!(
                "- session {}（{} runs，均分 {:.2}，波动 {:.2}，最差 {}）：{}\n",
                &s.session_id.chars().take(8).collect::<String>(),
                s.runs,
                s.mean,
                s.std,
                s.worst_status,
                s.worst_detail
            ));
        }
        brief.push_str(
            "对比反思：对每个 session，用 session_search 检索同类任务的成功轨迹，高分 vs 低分对照，定位第一个分歧点（哪个编排决策不同导致了结果分岔）。\n",
        );
    } else {
        brief.push_str("全部成功。萃取最近成功轨迹的工具使用与编排模式（怎么做对的）。\n");
    }
    // Cap the brief so it cannot grow unboundedly with session count.
    brief.chars().take(900).collect()
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

    let cwd = state.cwd();
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
            // Bench-validated fact loop: the dream may have written
            // orchestration facts — verify they did not regress the smoke
            // bench, roll back the newest fact if they did. Runs on this
            // watcher thread (idle anyway); all failures are soft.
            if ok {
                if let (Some(project), Some(workspace)) = (
                    nonoclaw_engine::session::project_dir(&cwd),
                    workspace_root_of(&cwd),
                )
                {
                    bench_validate_facts(&workspace, &project);
                }
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
                    .cwd()
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

    #[test]
    fn parse_pass_rate_reads_summary_line() {
        let out = "✅ PASS hello_world\n\n=== 2/3 tasks passed ===\n";
        assert!((parse_pass_rate(out).unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(parse_pass_rate("no summary here"), None);
        assert_eq!(parse_pass_rate("=== 0/0 tasks passed ==="), None);
    }

    #[test]
    fn bench_regression_supersedes_newest_fact() {
        let base = std::env::temp_dir().join("dream_bench_loop_test");
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("project");
        let facts = project.join(".nonoclaw/memory/facts");
        std::fs::create_dir_all(&facts).unwrap();
        let old_fact = facts.join("aaa-old.md");
        let new_fact = facts.join("zzz-new.md");
        let fm = |name: &str| {
            format!("---\nname: {name}\ntitle: t\ntype: convention\nimportance: 0.5\nconfidence: 0.5\ntags: []\nsources: []\n---\nbody\n")
        };
        std::fs::write(&old_fact, fm("aaa-old")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&new_fact, fm("zzz-new")).unwrap();

        // Newest fact is zzz-new (mtime ordering).
        assert_eq!(newest_fact(&facts).unwrap(), new_fact);

        // Baseline record.
        let history = project.join("bench_history.json");
        assert_eq!(read_pass_rate(&history), None);
        write_pass_rate(&history, 1.0, false);
        assert_eq!(read_pass_rate(&history), Some(1.0));

        // Regression → newest fact superseded, old untouched.
        let rollback = "zzz-new.rollback";
        nonoclaw_tools::memory::supersede_fact_by_path(
            &new_fact,
            rollback,
            "bench regression: smoke pass rate dropped",
        )
        .unwrap();
        let raw = std::fs::read_to_string(&new_fact).unwrap();
        assert!(raw.contains("superseded_by: zzz-new.rollback"), "{raw}");
        assert!(raw.contains("superseded_reason: bench regression"), "{raw}");
        let old_raw = std::fs::read_to_string(&old_fact).unwrap();
        assert!(!old_raw.contains("superseded_by"));
    }

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

    fn outcome(session: &str, status: &str, reward: f64) -> OutcomeSummary {
        OutcomeSummary {
            session_id: session.into(),
            run_id: format!("{session}-r"),
            status: status.into(),
            reward,
            detail: "d".into(),
        }
    }

    #[test]
    fn aggregate_sessions_ranks_volatile_and_difficult_first() {
        // s-volatile: mixed outcomes (high std, mid mean) → should outrank
        // s-steady-good (low std, high mean) even though its mean is worse.
        let out = vec![
            outcome("s-steady-good", "done", 1.0),
            outcome("s-volatile", "done", 1.0),
            outcome("s-volatile", "error", -1.0),
            outcome("s-flat-bad", "error", -1.0),
        ];
        let sessions = aggregate_sessions(&out);
        let names: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        // s-flat-bad: max difficulty, zero uncertainty; s-volatile: mid both.
        // Both must outrank the clean session; the clean one sorts last.
        assert_eq!(names.last(), Some(&"s-steady-good"));
        assert!(names.contains(&"s-flat-bad") && names.contains(&"s-volatile"));
        let vol = sessions.iter().find(|s| s.session_id == "s-volatile").unwrap();
        assert!((vol.std - 1.0).abs() < 1e-9, "std was {}", vol.std);
    }

    #[test]
    fn elbow_select_cuts_at_sharpest_curvature() {
        // Sharp drop after the 2nd element → elbow at index 2.
        let pri = [1.0, 0.9, 0.2, 0.15, 0.1];
        assert_eq!(elbow_select(&pri), 2);
        // Flat curve → keeps all (capped).
        assert_eq!(elbow_select(&[0.5, 0.5, 0.5, 0.5]), 4);
        // Fewer than 3 → take all.
        assert_eq!(elbow_select(&[0.9, 0.1]), 2);
    }

    #[test]
    fn reward_brief_lists_priority_sessions_and_contrast_instruction() {
        let dir = std::env::temp_dir().join("dream_reward_brief_priority");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lines = vec![
            r#"{"type":"user","text":"t"}"#.to_string(),
            r#"{"kind":"run_outcome","run_id":"r1","status":"error","reward":-1.0,"detail":"boom"}"#.to_string(),
        ];
        std::fs::write(dir.join("abc.jsonl"), lines.join("\n") + "\n").unwrap();
        let brief = reward_brief(&dir, SystemTime::now() - DREAM_BRIEF_WINDOW);
        assert!(brief.contains("重点复盘"), "has review section: {brief}");
        assert!(brief.contains("对比反思"), "has contrastive instruction: {brief}");
        assert!(brief.contains("boom"), "cites worst detail: {brief}");
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
