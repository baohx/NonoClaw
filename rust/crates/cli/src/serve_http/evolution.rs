//! Self-evolution pipeline (AutoGenesis SEPL borrow).
//!
//! Paper's core safety semantics, adapted to NonoClaw's file-based substrate:
//! every self-modification goes through  Reflect → [shadow] → Evaluate →
//! Commit, where **Evaluate gates Commit**:
//!
//!   * Shadow phase: a proposed change is written to a `.shadow` sibling,
//!     never the live resource.
//!   * Evaluate gate: a headless smoke run (bench or a probe prompt) must
//!     pass — or the shadow is discarded (rollback = delete the shadow).
//!   * Commit: promote shadow → live via atomic rename, snapshot the previous
//!     version into `.nonoclaw/evolution/history/` with a lineage note.
//!     Rollback = copy the newest history snapshot back.
//!
//! Evolvable resources (RSPL borrow, scoped to what NonoClaw actually runs):
//!   1. Skills — `.nonoclaw/skills/**` files
//!   2. System-prompt deltas — `.nonoclaw/APPEND_SYSTEM.md`
//! (facts already evolve via `supersede_fact_by_path` + bench validation.)

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
/// Directory holding committed-version snapshots for rollback.
fn history_dir(project_nonoclaw: &Path) -> PathBuf {
    project_nonoclaw.join("evolution").join("history")
}

fn shadow_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".shadow");
    path.with_file_name(name)
}

/// Stage a proposed resource change as a shadow (never mutates the live
/// file). Returns the shadow path. Callers: the Dream agent writes proposals
/// to `<file>.shadow` instead of the live file.
pub fn stage_shadow(path: &Path, content: &str) -> std::io::Result<PathBuf> {
    let sp = shadow_path(path);
    if let Some(parent) = sp.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&sp, content)?;
    Ok(sp)
}

/// Snapshot `path` into the history directory with a timestamped lineage
/// name. No-op (Ok) when the live file does not exist yet (first version).
fn snapshot(path: &Path, project_nonoclaw: &Path, resource: &str) -> std::io::Result<()> {
    let Some(content) = std::fs::read_to_string(path).ok() else {
        return Ok(());
    };
    let dir = history_dir(project_nonoclaw);
    std::fs::create_dir_all(&dir)?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let safe = resource.replace(['/', '\\'], "_");
    std::fs::write(dir.join(format!("{ts}-{safe}.bak")), content)
}

/// Commit gate outcome.
#[derive(Debug, PartialEq)]
pub enum CommitOutcome {
    /// Shadow promoted; the live file is now the new version.
    Committed,
    /// No shadow existed — nothing to do (common case: dream produced only
    /// facts, or the gate already ran this cycle).
    NoShadow,
    /// Gate evaluation failed; the shadow was discarded, live file untouched.
    Rejected(String),
}

/// Evaluate-gated commit: snapshot the live version, promote the shadow.
/// `gate: FnOnce() -> Result<(), String>` decides whether to commit; a
/// rejected gate deletes the shadow (safe rollback-by-discard).
pub fn commit_gated(
    live: &Path,
    project_nonoclaw: &Path,
    resource: &str,
    gate: impl FnOnce() -> Result<(), String>,
) -> std::io::Result<CommitOutcome> {
    let sp = shadow_path(live);
    if !sp.is_file() {
        return Ok(CommitOutcome::NoShadow);
    }
    match gate() {
        Ok(()) => {
            snapshot(live, project_nonoclaw, resource)?;
            std::fs::rename(&sp, live)?;
            tracing::info!(resource, "evolution: committed shadow -> live");
            Ok(CommitOutcome::Committed)
        }
        Err(reason) => {
            let _ = std::fs::remove_file(&sp);
            tracing::info!(resource, %reason, "evolution: gate rejected, shadow discarded");
            Ok(CommitOutcome::Rejected(reason))
        }
    }
}

/// Restore the most recent snapshot of `resource` over `live` (rollback).
pub fn restore(project_nonoclaw: &Path, resource: &str, live: &Path) -> std::io::Result<bool> {
    let dir = history_dir(project_nonoclaw);
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok().into_iter().flatten().flatten() {
        let p = entry.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some((ts, rest)) = name.split_once('-') else {
            continue;
        };
        if !rest.starts_with(resource) || !rest.ends_with(".bak") {
            continue;
        }
        let Ok(ts) = ts.parse::<u64>() else { continue };
        if best.as_ref().map_or(true, |(k, _)| ts > *k) {
            best = Some((ts, p));
        }
    }
    let Some((_, src)) = best else {
        return Ok(false);
    };
    std::fs::copy(src, live).map(|_| true)
}

// ── Default gate: probe run outcome from the latest shadow-window sessions ──

/// The gate used by the dream loop: a shadow change is committed only if the
/// most recent completed run since staging ended `done` with reward ≥ 0.5.
/// (`run_reward_labeled` already folds in tool-error-rate and
/// verification-evidence, so this reuses the existing label-free scorer.)
pub fn default_gate(latest_status: &str, latest_reward: f64) -> Result<(), String> {
    if latest_status == "done" && latest_reward >= 0.5 {
        Ok(())
    } else {
        Err(format!("gate: status={latest_status} reward={latest_reward:.2}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nc-evo-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".nonoclaw/evolution/history")).unwrap();
        d
    }

    #[test]
    fn commit_and_rollback_roundtrip() {
        let root = tmp("round");
        let live = root.join(".nonoclaw/skills/test.md");
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(&live, "v1").unwrap();
        stage_shadow(&live, "v2 proposal").unwrap();

        // Pass gate -> committed, live becomes v2.
        assert_eq!(
            commit_gated(&live, &root.join(".nonoclaw"), "skills_test.md", || Ok(())).unwrap(),
            CommitOutcome::Committed
        );
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "v2 proposal");
        assert!(!shadow_path(&live).exists(), "shadow consumed");

        // Rollback restores v1.
        assert!(restore(&root.join(".nonoclaw"), "skills_test.md", &live).unwrap());
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "v1");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejected_gate_discards_shadow_keeps_live() {
        let root = tmp("reject");
        let live = root.join(".nonoclaw/APPEND_SYSTEM.md");
        std::fs::write(&live, "live").unwrap();
        stage_shadow(&live, "bad proposal").unwrap();
        assert_eq!(
            commit_gated(&live, &root.join(".nonoclaw"), "APPEND_SYSTEM.md", || {
                Err("bench regressed".into())
            })
            .unwrap(),
            CommitOutcome::Rejected("bench regressed".into())
        );
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "live");
        assert!(!shadow_path(&live).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_shadow_is_noop() {
        let root = tmp("noop");
        let live = root.join(".nonoclaw/APPEND_SYSTEM.md");
        std::fs::write(&live, "live").unwrap();
        assert_eq!(
            commit_gated(&live, &root.join(".nonoclaw"), "APPEND_SYSTEM.md", || Ok(())).unwrap(),
            CommitOutcome::NoShadow
        );
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "live");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_gate_thresholds() {
        assert!(default_gate("done", 1.0).is_ok());
        assert!(default_gate("done", 0.5).is_ok());
        assert!(default_gate("done", 0.2).is_err());
        assert!(default_gate("error", 1.0).is_err());
    }
}
