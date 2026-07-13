//! File watcher for skill directories. Uses the `notify` crate to detect
//! changes to `SKILL.md` files and hot-reload them into the `SkillsManager`.
//! Mirrors CC's `src/utils/skills/skillChangeDetector.ts`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use nonoclaw_engine::SkillsManager;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Spawn a background task that watches skill directories for changes and
/// hot-reloads them into `skills_manager`.
pub fn spawn_skill_watcher(
    skills_manager: Arc<RwLock<SkillsManager>>,
    cwd: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let mut watch_dirs: Vec<PathBuf> = vec![
            cwd.join(".nonoclaw").join("skills"),
        ];
        if let Some(home) = nonoclaw_core::nonoclaw_data_dir() {
            watch_dirs.push(home.join("skills"));
        }
        // Plugin skill dirs
        let plugins_dir = cwd.join(".nonoclaw").join("plugins");
        if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
            for entry in entries.flatten() {
                let skill_dir = entry.path().join("skills");
                if skill_dir.is_dir() {
                    watch_dirs.push(skill_dir);
                }
            }
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = tx.send(event);
                }
            },
            Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("failed to create skill file watcher: {e}");
                return;
            }
        };

        // Watch skill directories.
        for dir in &watch_dirs {
            if dir.is_dir() {
                if let Err(e) = watcher.watch(dir, RecursiveMode::Recursive) {
                    tracing::warn!("failed to watch {}: {e}", dir.display());
                }
            }
        }
        tracing::info!("watching {:?} for skill changes", watch_dirs);

        // Debounce: collect events over 500ms, then reload.
        let debounce = Duration::from_millis(500);
        let mut pending_dirs: Vec<PathBuf> = Vec::new();
        let mut last_event = std::time::Instant::now();

        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(event) => {
                    // Only care about SKILL.md changes.
                    let is_skill_change = event.paths.iter().any(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n == "SKILL.md")
                            .unwrap_or(false)
                    });
                    if !is_skill_change {
                        continue;
                    }
                    match event.kind {
                        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) => {
                            for p in &event.paths {
                                if let Some(parent) = p.parent().map(|d| d.to_path_buf()) {
                                    if !pending_dirs.contains(&parent) {
                                        pending_dirs.push(parent);
                                    }
                                }
                            }
                            last_event = std::time::Instant::now();
                        }
                        _ => {}
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Flush pending changes after debounce period.
                    if !pending_dirs.is_empty()
                        && last_event.elapsed() >= debounce
                    {
                        let dirs: Vec<PathBuf> = std::mem::take(&mut pending_dirs);
                        if let Ok(mut mgr) = skills_manager.write() {
                            for dir in &dirs {
                                mgr.load_from_dir(dir);
                            }
                        }
                        tracing::info!("hot-reloaded {} skill dirs", dirs.len());
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break;
                }
            }
        }
    })
}
