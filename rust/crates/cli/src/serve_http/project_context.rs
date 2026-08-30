//! Atomically swappable project-scoped runtime state.
//!
//! Every operation takes one `Arc<ProjectContext>` snapshot so cwd, resolved
//! configuration, skills, and upload storage can never come from different
//! projects during a concurrent project switch.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use nonoclaw_engine::{ResolvedConfig, SkillsManager};
use tokio::sync::{watch, Mutex, OwnedMutexGuard};

pub(crate) struct ProjectContext {
    generation: u64,
    cwd: PathBuf,
    config: Arc<ResolvedConfig>,
    skills_manager: Arc<RwLock<SkillsManager>>,
    upload_dir: PathBuf,
}

impl ProjectContext {
    pub(crate) fn new(
        generation: u64,
        cwd: PathBuf,
        config: Arc<ResolvedConfig>,
        skills_manager: Arc<RwLock<SkillsManager>>,
        upload_dir: PathBuf,
    ) -> Self {
        Self {
            generation,
            cwd,
            config,
            skills_manager,
            upload_dir,
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub(crate) fn config(&self) -> &Arc<ResolvedConfig> {
        &self.config
    }

    pub(crate) fn skills_manager(&self) -> &Arc<RwLock<SkillsManager>> {
        &self.skills_manager
    }

    pub(crate) fn upload_dir(&self) -> &Path {
        &self.upload_dir
    }
}

#[derive(Clone)]
pub(crate) struct ProjectContextStore {
    current: Arc<RwLock<Arc<ProjectContext>>>,
    transition: Arc<Mutex<()>>,
    generation_tx: watch::Sender<u64>,
}

impl ProjectContextStore {
    pub(crate) fn new(initial: ProjectContext) -> Self {
        let generation = initial.generation();
        let (generation_tx, _) = watch::channel(generation);
        Self {
            current: Arc::new(RwLock::new(Arc::new(initial))),
            transition: Arc::new(Mutex::new(())),
            generation_tx,
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<ProjectContext> {
        Arc::clone(&self.current.read().unwrap())
    }

    /// Capture the context and create its generation receiver under the same
    /// read lock. A replacement can therefore happen wholly before or after
    /// this pair, never in the gap between two independent calls.
    pub(crate) fn snapshot_and_subscribe(&self) -> (Arc<ProjectContext>, watch::Receiver<u64>) {
        let current = self.current.read().unwrap();
        let updates = self.generation_tx.subscribe();
        (Arc::clone(&current), updates)
    }

    /// Serialize project replacement with run startup. The guard is owned so
    /// WebSocket run preparation can move it into a spawned supervisor task.
    pub(crate) async fn lock_transition(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.transition).lock_owned().await
    }

    pub(crate) fn prepare(&self, cwd: PathBuf) -> std::io::Result<Arc<ProjectContext>> {
        let current = self.snapshot();
        let config = Arc::new(current.config().reload_for_cwd(&cwd));
        let skills_manager = Arc::new(RwLock::new(SkillsManager::new(&cwd)));
        let upload_dir = upload_dir_for(&cwd);
        std::fs::create_dir_all(&upload_dir)?;
        Ok(Arc::new(ProjectContext::new(
            current.generation().saturating_add(1),
            cwd,
            config,
            skills_manager,
            upload_dir,
        )))
    }

    /// Publish a fully prepared project in one pointer swap, then notify live
    /// WebSocket connections so stale per-connection session handles close.
    pub(crate) fn replace(&self, next: Arc<ProjectContext>) {
        let generation = next.generation();
        *self.current.write().unwrap() = next;
        self.generation_tx.send_replace(generation);
    }
}

pub(crate) fn upload_dir_for(cwd: &Path) -> PathBuf {
    nonoclaw_engine::session::project_dir(cwd)
        .map(|project| project.join("uploads"))
        .unwrap_or_else(|| cwd.join(".nonoclaw/uploads"))
}
