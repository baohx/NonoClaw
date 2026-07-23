//! Project information, file-tree, Git, and safe file-opening service.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use nonoclaw_engine::{ResolvedConfig, SkillsManager};
use nonoclaw_tools::ToolRegistry;
use tokio::sync::Mutex;

use super::protocol::FileEntry;
use crate::project_info::{gather, ProjectInfo};

const FILE_TREE_SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".cache",
    ".turbo",
    "__pycache__",
    ".venv",
    "venv",
    ".idea",
    ".gradle",
    ".dart_tool",
];
const FILE_TREE_MAX_DEPTH: u32 = 10;
const FILE_TREE_MAX_ENTRIES: usize = 10_000;

/// Single owner for project snapshots. The refresh gate explicitly deduplicates
/// concurrent git/config/skills scans without holding registry or WebSocket
/// locks across disk or Git awaits.
pub(super) struct ProjectService {
    cwd: PathBuf,
    registry: Arc<ToolRegistry>,
    config: Arc<ResolvedConfig>,
    public_url: Option<String>,
    skills_manager: Arc<RwLock<SkillsManager>>,
    refresh_gate: Mutex<()>,
}

impl ProjectService {
    pub(super) fn new(
        cwd: PathBuf,
        registry: Arc<ToolRegistry>,
        config: Arc<ResolvedConfig>,
        public_url: Option<String>,
        skills_manager: Arc<RwLock<SkillsManager>>,
    ) -> Self {
        Self {
            cwd,
            registry,
            config,
            public_url,
            skills_manager,
            refresh_gate: Mutex::new(()),
        }
    }

    pub(super) fn file_tree(&self) -> Vec<FileEntry> {
        build_file_tree(&self.cwd)
    }

    pub(super) async fn snapshot(&self, model: &str) -> ProjectInfo {
        let _refresh = self.refresh_gate.lock().await;
        self.gather_with(model, &self.config).await
    }

    pub(super) async fn refresh(&self, model: &str) -> ProjectInfo {
        let _refresh = self.refresh_gate.lock().await;
        self.skills_manager.write().unwrap().rescan(&self.cwd);
        let config = self.config.reload();
        config.log_diagnostics();
        self.gather_with(model, &config).await
    }

    async fn gather_with(&self, model: &str, config: &ResolvedConfig) -> ProjectInfo {
        let (skills, extensions, diagnostics) = {
            let manager = self.skills_manager.read().unwrap();
            (
                manager.all_active(),
                manager.descriptors(),
                manager.diagnostics(),
            )
        };
        gather(
            &self.cwd,
            model,
            &self.registry,
            config,
            self.public_url.clone(),
            &skills,
            &extensions,
            &diagnostics,
        )
        .await
    }

    pub(super) async fn git_show(&self, sha: &str) -> Option<String> {
        crate::project_info::git_show(&self.cwd, sha).await
    }

    pub(super) fn open(&self, relative: &str, force_code: bool) -> std::io::Result<()> {
        let roots = open_roots(&self.cwd);
        let full = resolve_within(&roots, relative).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "path outside allowed roots",
            )
        })?;
        if !full.exists() {
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::File::create(&full)?;
        }
        if force_code {
            tokio::process::Command::new("code")
                .arg(full)
                .spawn()
                .map(|_| ())
        } else {
            open_with_default(&full)
        }
    }
}

fn build_file_tree(root: &Path) -> Vec<FileEntry> {
    let mut entries = Vec::new();
    let mut count = 0;
    walk_dir(root, root, 0, &mut entries, &mut count);
    entries
}

fn walk_dir(root: &Path, dir: &Path, depth: u32, out: &mut Vec<FileEntry>, count: &mut usize) {
    if depth >= FILE_TREE_MAX_DEPTH || *count >= FILE_TREE_MAX_ENTRIES {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<_> = read.filter_map(Result::ok).collect();
    items.sort_by(|left, right| {
        let left_dir = left.file_type().is_ok_and(|kind| kind.is_dir());
        let right_dir = right.file_type().is_ok_and(|kind| kind.is_dir());
        right_dir.cmp(&left_dir).then_with(|| {
            left.file_name()
                .to_string_lossy()
                .to_lowercase()
                .cmp(&right.file_name().to_string_lossy().to_lowercase())
        })
    });
    for entry in items {
        if *count >= FILE_TREE_MAX_ENTRIES {
            break;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if name.starts_with('.') || is_dir && FILE_TREE_SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root).map(Path::to_path_buf) else {
            continue;
        };
        out.push(FileEntry {
            path: relative.to_string_lossy().replace('\\', "/"),
            name,
            is_dir,
            depth,
        });
        *count += 1;
        if is_dir {
            walk_dir(root, &entry.path(), depth + 1, out, count);
        }
    }
}

fn resolve_within(roots: &[PathBuf], relative: &str) -> Option<PathBuf> {
    let path = Path::new(relative);
    if !path.is_absolute()
        && path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return None;
    }
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        roots.first()?.join(path)
    };
    let mut ancestor = joined;
    let mut tail = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(canonical) => {
                ancestor = canonical;
                break;
            }
            Err(_) => {
                tail.push(ancestor.file_name()?.to_os_string());
                ancestor = ancestor.parent()?.to_path_buf();
            }
        }
    }
    if !roots.iter().any(|root| ancestor.starts_with(root)) {
        return None;
    }
    for component in tail.into_iter().rev() {
        ancestor.push(component);
    }
    Some(ancestor)
}

fn open_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf())];
    if let Some(home) = crate::project_info::nonoclaw_home() {
        if let Ok(home) = home.canonicalize() {
            roots.push(home);
        }
    }
    roots
}

fn open_with_default(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    return tokio::process::Command::new("open")
        .arg(path)
        .spawn()
        .map(|_| ());
    #[cfg(target_os = "windows")]
    return tokio::process::Command::new("cmd")
        .args(["/C", "start", "", &path.to_string_lossy()])
        .spawn()
        .map(|_| ());
    #[cfg(all(unix, not(target_os = "macos")))]
    return tokio::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .or_else(|_| tokio::process::Command::new("code").arg(path).spawn())
        .map(|_| ());
    #[allow(unreachable_code)]
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no opener configured for this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_within_rejects_parent_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        assert!(resolve_within(std::slice::from_ref(&root), "../outside").is_none());

        let sibling = root.parent().unwrap().join(format!(
            "{}-sibling",
            root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(resolve_within(
            std::slice::from_ref(&root),
            sibling.to_string_lossy().as_ref()
        )
        .is_none());
        assert_eq!(
            resolve_within(std::slice::from_ref(&root), "new/nested.txt"),
            Some(root.join("new/nested.txt"))
        );
        std::fs::remove_dir_all(sibling).ok();
    }

    #[cfg(unix)]
    #[test]
    fn resolve_within_rejects_symlink_escape_for_existing_and_new_children() {
        use std::os::unix::fs::symlink;

        let root_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root_dir.path().join("escape")).unwrap();
        let root = root_dir.path().canonicalize().unwrap();
        assert!(resolve_within(std::slice::from_ref(&root), "escape").is_none());
        assert!(resolve_within(&[root], "escape/new.txt").is_none());
    }

    #[test]
    fn file_tree_omits_hidden_and_generated_directories() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("visible.txt"), "ok").unwrap();
        std::fs::create_dir(temp.path().join("target")).unwrap();
        std::fs::write(temp.path().join("target/hidden.txt"), "no").unwrap();
        let entries = build_file_tree(temp.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "visible.txt");
    }
}
