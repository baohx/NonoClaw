//! Server-side project registry for the Web UI project tree.
//!
//! The registry is a plain JSON document persisted under the NonoClaw data
//! directory (`$NONOCLAW_HOME` / `~/.nonoclaw`). Entries are canonical
//! absolute paths; adding is idempotent by canonical path and removing only
//! drops the entry, never anything on disk. The document is written through a
//! temp file plus rename so a crash mid-write can never leave a torn file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use super::protocol::RegisteredProject;

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct RegistryDocument {
    #[serde(default)]
    projects: Vec<RegisteredProject>,
}

/// Shared, persistently-backed project registry.
#[derive(Debug, Clone)]
pub(super) struct ProjectRegistry {
    state: Arc<Mutex<RegistryDocument>>,
    file: PathBuf,
}

impl ProjectRegistry {
    /// Load (or initialize) the registry stored under the NonoClaw data dir.
    pub(super) fn load() -> Option<Self> {
        let file = registry_file()?;
        let doc = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str::<RegistryDocument>(&text).ok())
            .unwrap_or_default();
        Some(Self {
            state: Arc::new(Mutex::new(doc)),
            file,
        })
    }

    /// Load, or fall back to an empty in-memory registry when the data dir is
    /// unavailable (project-tree features silently no-op on persistence).
    pub(super) fn load_or_ephemeral() -> Self {
        Self::load().unwrap_or_else(|| Self {
            state: Arc::new(Mutex::new(RegistryDocument::default())),
            file: PathBuf::new(),
        })
    }

    fn snapshot(doc: &RegistryDocument) -> Vec<RegisteredProject> {
        let mut projects = doc.projects.clone();
        projects.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
        projects
    }

    async fn persist(&self, doc: &RegistryDocument) {
        if self.file.as_os_str().is_empty() {
            return;
        }
        let tmp = self.file.with_extension("json.tmp");
        let Ok(text) = serde_json::to_string_pretty(doc) else {
            return;
        };
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.file);
        }
    }

    /// Current entries, sorted for stable display.
    pub(super) async fn list(&self) -> Vec<RegisteredProject> {
        let doc = self.state.lock().await;
        Self::snapshot(&doc)
    }

    /// Register `path` if absent. The path is canonicalized, must exist, and
    /// must be a directory. Returns the up-to-date full list.
    pub(super) async fn add(&self, path: &str) -> Result<Vec<RegisteredProject>, String> {
        let canonical = canonical_project_dir(path)?;
        let mut doc = self.state.lock().await;
        if !doc.projects.iter().any(|p| p.path == canonical) {
            doc.projects.push(RegisteredProject {
                name: project_name(&canonical),
                path: canonical,
                added_at: chrono::Utc::now().to_rfc3339(),
            });
            self.persist(&doc).await;
        }
        Ok(Self::snapshot(&doc))
    }

    /// Remove the entry matching `path` (compared after canonicalization when
    /// the path still resolves). Refuses to remove the active project.
    pub(super) async fn remove(
        &self,
        path: &str,
        active_cwd: &Path,
    ) -> Result<Vec<RegisteredProject>, String> {
        let canonical = canonical_project_dir(path)?;
        let active = canonical_project_dir(&active_cwd.to_string_lossy())
            .unwrap_or_else(|_| active_cwd.to_path_buf());
        if canonical == active {
            return Err("cannot remove the active project".to_string());
        }
        let mut doc = self.state.lock().await;
        if doc.projects.iter().any(|p| p.path == canonical) {
            doc.projects.retain(|p| p.path != canonical);
            self.persist(&doc).await;
        }
        Ok(Self::snapshot(&doc))
    }

    /// Ensure `cwd` is present (idempotent). Used at serve startup so the
    /// currently served project always appears in the tree. Best-effort: any
    /// I/O failure is ignored, matching the ephemeral-registry fallback.
    pub(super) async fn ensure(&self, cwd: &Path) {
        let _ = self.add(&cwd.to_string_lossy()).await;
    }
}

fn registry_file() -> Option<PathBuf> {
    nonoclaw_core::nonoclaw_data_dir().map(|root| root.join("projects_registry.json"))
}

/// Resolve `path` to a canonical absolute directory path, or an error string
/// suitable for returning to the client.
fn canonical_project_dir(path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("project path is empty".to_string());
    }
    let candidate = PathBuf::from(path);
    let canonical = std::fs::canonicalize(&candidate).map_err(|e| {
        format!(
            "project path `{}` cannot be resolved: {e}",
            candidate.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!(
            "project path `{}` is not a directory",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn project_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_registry() -> (tempfile::TempDir, ProjectRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("projects_registry.json");
        let registry = ProjectRegistry {
            state: Arc::new(Mutex::new(RegistryDocument::default())),
            file,
        };
        (dir, registry)
    }

    #[tokio::test]
    async fn add_is_idempotent_and_persists() {
        let (_dir, registry) = temp_registry();
        let path = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let first = registry.add(path.to_str().unwrap()).await.unwrap();
        let second = registry.add(path.to_str().unwrap()).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        let text = std::fs::read_to_string(&registry.file).unwrap();
        assert!(text.contains("projects"));
    }

    #[tokio::test]
    async fn add_rejects_missing_and_non_directory() {
        let (dir, registry) = temp_registry();
        assert!(registry
            .add(dir.path().join("nope").to_str().unwrap())
            .await
            .is_err());
        let file_path = dir.path().join("a_file");
        std::fs::write(&file_path, b"x").unwrap();
        assert!(registry.add(file_path.to_str().unwrap()).await.is_err());
    }

    #[tokio::test]
    async fn remove_drops_entry_but_not_active() {
        let (_dir, registry) = temp_registry();
        let path = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        registry.add(path.to_str().unwrap()).await.unwrap();
        let active_err = registry
            .remove(path.to_str().unwrap(), &path)
            .await
            .unwrap_err();
        assert!(active_err.contains("active"));
        let after = registry
            .remove(path.to_str().unwrap(), Path::new("/tmp/other"))
            .await
            .unwrap();
        assert!(after.is_empty());
    }
}
