//! File watcher for project and user skill directories.
//!
//! The watcher follows the atomically published ProjectContext so switching
//! projects replaces both the watched directories and the SkillsManager that
//! receives hot-reload events.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use nonoclaw_engine::SkillsManager;

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::serve_http::project_context::{ProjectContext, ProjectContextStore};

type WatchEvent = Result<Event, notify::Error>;

fn push_watch_root(roots: &mut Vec<(PathBuf, RecursiveMode)>, path: PathBuf, mode: RecursiveMode) {
    if !roots.iter().any(|(existing, _)| existing == &path) {
        roots.push((path, mode));
    }
}

/// Watch a stable ancestor when a skills directory does not exist yet. A
/// layout-creation event causes a rebind to the newly available recursive root.
fn watch_roots(cwd: &Path) -> Vec<(PathBuf, RecursiveMode)> {
    let mut roots = Vec::new();
    let project_nonoclaw = cwd.join(".nonoclaw");
    if project_nonoclaw.is_dir() {
        push_watch_root(&mut roots, project_nonoclaw, RecursiveMode::Recursive);
    } else {
        push_watch_root(&mut roots, cwd.to_path_buf(), RecursiveMode::NonRecursive);
    }

    if let Some(home) = nonoclaw_core::nonoclaw_data_dir() {
        let user_skills = home.join("skills");
        if user_skills.is_dir() {
            push_watch_root(&mut roots, user_skills, RecursiveMode::Recursive);
        } else {
            let mut ancestor = home.as_path();
            while !ancestor.is_dir() {
                let Some(parent) = ancestor.parent() else {
                    break;
                };
                ancestor = parent;
            }
            if ancestor.is_dir() {
                push_watch_root(
                    &mut roots,
                    ancestor.to_path_buf(),
                    RecursiveMode::NonRecursive,
                );
            }
        }
    }
    roots
}

fn event_requires_rebind(event: &Event) -> bool {
    let directory_change = match event.kind {
        EventKind::Create(kind) => {
            matches!(
                kind,
                notify::event::CreateKind::Any | notify::event::CreateKind::Folder
            ) || event.paths.iter().any(|path| path.is_dir())
        }
        EventKind::Remove(kind) => matches!(
            kind,
            notify::event::RemoveKind::Any | notify::event::RemoveKind::Folder
        ),
        EventKind::Modify(notify::event::ModifyKind::Name(_)) => true,
        _ => false,
    };
    if directory_change {
        // When the desired root is several levels below the nearest existing
        // ancestor, each newly created directory moves the fallback watch one
        // level closer. Reconcile on every directory-layout event, not only on
        // the final `.nonoclaw/skills` component.
        return true;
    }
    event.paths.iter().any(|path| {
        matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some(".nonoclaw" | "skills" | "plugins")
        )
    })
}

fn has_watch_root(
    roots: &[(PathBuf, RecursiveMode)],
    candidate: &(PathBuf, RecursiveMode),
) -> bool {
    roots
        .iter()
        .any(|(path, mode)| path == &candidate.0 && mode == &candidate.1)
}

struct BoundWatcher {
    watcher: RecommendedWatcher,
    watched_roots: Vec<(PathBuf, RecursiveMode)>,
    failed_roots: Vec<(PathBuf, RecursiveMode)>,
}

impl BoundWatcher {
    /// Recompute logical roots on every retry. This preserves healthy watches
    /// while migrating a failed non-recursive fallback to the final recursive
    /// skills root if the missing hierarchy appeared in the meantime.
    fn reconcile(&mut self, cwd: &Path) -> bool {
        let desired = watch_roots(cwd);
        let obsolete: Vec<_> = self
            .watched_roots
            .iter()
            .filter(|root| !has_watch_root(&desired, root))
            .cloned()
            .collect();
        for root in obsolete {
            match self.watcher.unwatch(&root.0) {
                Ok(()) => self
                    .watched_roots
                    .retain(|watched| !has_watch_root(std::slice::from_ref(&root), watched)),
                Err(error) => tracing::debug!(
                    path = %root.0.display(),
                    %error,
                    "obsolete skill watch root could not be detached"
                ),
            }
        }
        self.failed_roots
            .retain(|failed| has_watch_root(&desired, failed));

        let mut recovered = false;
        for (root, mode) in desired {
            let candidate = (root.clone(), mode);
            if has_watch_root(&self.watched_roots, &candidate) {
                continue;
            }
            match self.watcher.watch(&root, mode) {
                Ok(()) => {
                    self.failed_roots
                        .retain(|failed| !has_watch_root(std::slice::from_ref(&candidate), failed));
                    self.watched_roots.push(candidate);
                    recovered = true;
                    tracing::info!(path = %root.display(), "skill watch root attached");
                }
                Err(error) => {
                    tracing::debug!(path = %root.display(), %error, "skill watch root still unavailable");
                    if !has_watch_root(&self.failed_roots, &candidate) {
                        self.failed_roots.push(candidate);
                    }
                }
            }
        }
        recovered
    }
}

fn bind_watcher(
    event_tx: &std::sync::mpsc::Sender<WatchEvent>,
    cwd: &Path,
) -> Option<BoundWatcher> {
    let callback_tx = event_tx.clone();
    let mut watcher = match RecommendedWatcher::new(
        move |result: WatchEvent| {
            let _ = callback_tx.send(result);
        },
        Config::default(),
    ) {
        Ok(watcher) => watcher,
        Err(error) => {
            tracing::warn!(%error, "failed to create skill file watcher");
            return None;
        }
    };

    let roots = watch_roots(cwd);
    let mut watched_roots = Vec::new();
    let mut failed_roots = Vec::new();
    for (root, mode) in &roots {
        if let Err(error) = watcher.watch(root, *mode) {
            tracing::warn!(path = %root.display(), %error, "failed to watch skill root; will retry");
            failed_roots.push((root.clone(), *mode));
        } else {
            watched_roots.push((root.clone(), *mode));
        }
    }
    tracing::info!(
        cwd = %cwd.display(),
        ?roots,
        failed = failed_roots.len(),
        "skill watcher bound to project"
    );
    Some(BoundWatcher {
        watcher,
        watched_roots,
        failed_roots,
    })
}

fn bind_project_watcher(
    event_tx: &std::sync::mpsc::Sender<WatchEvent>,
    project: &ProjectContext,
) -> Option<BoundWatcher> {
    let watcher = bind_watcher(event_tx, project.cwd());
    // Scan after binding. An edit between ProjectContext preparation and the
    // native watch attachment is then either present in this scan or delivered
    // as an event, closing the rebind gap.
    if let Ok(mut manager) = project.skills_manager().write() {
        manager.rescan(project.cwd());
    }
    watcher
}

/// Spawn one long-lived watcher that rebinds whenever ProjectContext changes.
pub fn spawn_project_skill_watcher(projects: ProjectContextStore) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let (event_tx, event_rx) = std::sync::mpsc::channel::<WatchEvent>();
        let initial = projects.snapshot();
        let mut generation = initial.generation();
        let mut watcher = bind_project_watcher(&event_tx, &initial);
        let debounce = Duration::from_millis(500);
        let mut pending_dirs: Vec<PathBuf> = Vec::new();
        let mut last_event = std::time::Instant::now();

        loop {
            let current = projects.snapshot();
            if current.generation() != generation {
                generation = current.generation();
                pending_dirs.clear();
                // Drop the old native watcher before draining its queued
                // callbacks; otherwise a delayed old-project event could be
                // loaded into the new project's SkillsManager.
                drop(watcher.take());
                while event_rx.try_recv().is_ok() {}
                watcher = bind_project_watcher(&event_tx, &current);
            }
            // Creation/watch errors can leave no event source. Drain callbacks
            // from the failed watcher, then retry before every bounded receive.
            if watcher.is_none() {
                while event_rx.try_recv().is_ok() {}
                watcher = bind_project_watcher(&event_tx, &current);
            }
            if watcher
                .as_mut()
                .is_some_and(|bound| bound.reconcile(current.cwd()))
            {
                if let Ok(mut manager) = current.skills_manager().write() {
                    manager.rescan(current.cwd());
                }
            }

            match event_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(event)) => {
                    if event_requires_rebind(&event) {
                        pending_dirs.clear();
                        drop(watcher.take());
                        tracing::info!("skill directory layout changed; rebinding watcher");
                        continue;
                    }
                    let is_skill_change = event.paths.iter().any(|path| {
                        path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md")
                    });
                    if !is_skill_change {
                        continue;
                    }
                    if matches!(
                        event.kind,
                        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                    ) {
                        for path in &event.paths {
                            if let Some(parent) = path.parent().map(Path::to_path_buf) {
                                if !pending_dirs.contains(&parent) {
                                    pending_dirs.push(parent);
                                }
                            }
                        }
                        last_event = std::time::Instant::now();
                    }
                }
                Ok(Err(error)) => {
                    tracing::warn!(%error, "skill watcher callback failed; rebinding");
                    pending_dirs.clear();
                    drop(watcher.take());
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if pending_dirs.is_empty() || last_event.elapsed() < debounce {
                        continue;
                    }
                    let current = projects.snapshot();
                    if current.generation() != generation {
                        pending_dirs.clear();
                        continue;
                    }
                    let dirs: Vec<PathBuf> = std::mem::take(&mut pending_dirs);
                    if let Ok(mut manager) = current.skills_manager().write() {
                        for dir in &dirs {
                            manager.load_from_dir(dir);
                        }
                    }
                    tracing::info!(count = dirs.len(), "hot-reloaded skill directories");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    })
}

/// Static watcher retained for non-Web CLI/ACP sessions, whose project cannot
/// change after startup.
pub fn spawn_skill_watcher(
    skills_manager: Arc<RwLock<SkillsManager>>,
    cwd: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let (event_tx, event_rx) = std::sync::mpsc::channel::<WatchEvent>();
        let mut watcher = None;
        let debounce = Duration::from_millis(500);
        let mut pending_dirs: Vec<PathBuf> = Vec::new();
        let mut last_event = std::time::Instant::now();

        loop {
            if watcher.is_none() {
                while event_rx.try_recv().is_ok() {}
                watcher = bind_watcher(&event_tx, &cwd);
                if watcher.is_some() {
                    if let Ok(mut manager) = skills_manager.write() {
                        manager.rescan(&cwd);
                    }
                }
            }
            if watcher.as_mut().is_some_and(|bound| bound.reconcile(&cwd)) {
                if let Ok(mut manager) = skills_manager.write() {
                    manager.rescan(&cwd);
                }
            }
            match event_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(event)) => {
                    if event_requires_rebind(&event) {
                        pending_dirs.clear();
                        drop(watcher.take());
                        continue;
                    }
                    let is_skill_change = event.paths.iter().any(|path| {
                        path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md")
                    });
                    if is_skill_change
                        && matches!(
                            event.kind,
                            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                        )
                    {
                        for path in &event.paths {
                            if let Some(parent) = path.parent().map(Path::to_path_buf) {
                                if !pending_dirs.contains(&parent) {
                                    pending_dirs.push(parent);
                                }
                            }
                        }
                        last_event = std::time::Instant::now();
                    }
                }
                Ok(Err(error)) => {
                    tracing::warn!(%error, "skill watcher callback failed; rebinding");
                    pending_dirs.clear();
                    drop(watcher.take());
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if pending_dirs.is_empty() || last_event.elapsed() < debounce {
                        continue;
                    }
                    let dirs: Vec<PathBuf> = std::mem::take(&mut pending_dirs);
                    if let Ok(mut manager) = skills_manager.write() {
                        for dir in &dirs {
                            manager.load_from_dir(dir);
                        }
                    }
                    tracing::info!(count = dirs.len(), "hot-reloaded skill directories");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    })
}
