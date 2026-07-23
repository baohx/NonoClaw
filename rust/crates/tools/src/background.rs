//! Canonical lifecycle owner for background shell tasks.
//!
//! [`BackgroundTaskManager`] owns each child process, cancellation token,
//! observable status, output file, completion notification, and final reap.
//! Dropping the final manager handle cancels every live task; each monitor uses
//! `kill_on_drop` as a final process-exit safeguard.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn now_timestamp() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}Z", elapsed.as_secs(), elapsed.subsec_millis())
}

/// Public, serializable state for one background command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,
    pub command: String,
    pub status: TaskStatus,
    pub output_path: PathBuf,
    pub exit_code: Option<i32>,
    pub started_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Backgrounded,
    Completed,
    Failed,
    Killed,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Killed)
    }
}

struct ManagedTask {
    task: BackgroundTask,
    cancel: CancellationToken,
    notified: bool,
}

#[derive(Default)]
struct ManagerState {
    tasks: HashMap<String, ManagedTask>,
}

struct ManagerInner {
    state: Mutex<ManagerState>,
    changed: Notify,
}

impl Drop for ManagerInner {
    fn drop(&mut self) {
        if let Ok(state) = self.state.lock() {
            for task in state.tasks.values() {
                if !task.task.status.is_terminal() {
                    task.cancel.cancel();
                }
            }
        }
    }
}

/// Thread-safe owner of background command lifecycles.
///
/// The type is internally synchronized and cheap to clone. Existing callers
/// that wrap `BackgroundTaskRegistry` in `Arc<Mutex<_>>` remain compatible via
/// the alias below while new code can use this manager directly.
#[derive(Clone)]
pub struct BackgroundTaskManager {
    inner: Arc<ManagerInner>,
}

impl Default for BackgroundTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundTaskManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                state: Mutex::new(ManagerState::default()),
                changed: Notify::new(),
            }),
        }
    }

    /// Spawn in the process current directory without a parent cancellation
    /// token. Kept for the original registry API.
    pub fn spawn(&self, command: &str, timeout_ms: u64) -> String {
        self.spawn_with_cancel(command, timeout_ms, CancellationToken::new())
    }

    /// Spawn in the process current directory and tie the child to `cancel`.
    /// Kept for the original registry API.
    pub fn spawn_with_cancel(
        &self,
        command: &str,
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> String {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        self.spawn_in_with_cancel(command, &cwd, timeout_ms, cancel)
    }

    /// Spawn a background shell in `cwd`. Output is persisted immediately and
    /// the monitor owns/reaps the child until a terminal state is committed.
    pub fn spawn_in_with_cancel(
        &self,
        command: &str,
        cwd: &Path,
        timeout_ms: u64,
        parent_cancel: CancellationToken,
    ) -> String {
        let id = format!("bg-{}", Uuid::new_v4());
        let output_dir = std::env::temp_dir().join("nonoclaw-tasks");
        let _ = std::fs::create_dir_all(&output_dir);
        let output_path = output_dir.join(format!("{id}.output"));
        let task_cancel = parent_cancel.child_token();
        let task = BackgroundTask {
            id: id.clone(),
            command: command.to_string(),
            status: TaskStatus::Running,
            output_path: output_path.clone(),
            exit_code: None,
            started_at: now_timestamp(),
            completed_at: None,
        };
        self.inner.state.lock().unwrap().tasks.insert(
            id.clone(),
            ManagedTask {
                task,
                cancel: task_cancel.clone(),
                notified: false,
            },
        );

        let weak = Arc::downgrade(&self.inner);
        let task_id = id.clone();
        let command = command.to_string();
        let cwd = cwd.to_path_buf();
        tokio::spawn(async move {
            monitor_task(
                weak,
                task_id,
                command,
                cwd,
                output_path,
                timeout_ms,
                task_cancel,
            )
            .await;
        });
        id
    }

    pub fn get_task(&self, task_id: &str) -> Option<BackgroundTask> {
        self.inner
            .state
            .lock()
            .unwrap()
            .tasks
            .get(task_id)
            .map(|managed| managed.task.clone())
    }

    pub fn list_tasks(&self) -> Vec<BackgroundTask> {
        let mut tasks = self
            .inner
            .state
            .lock()
            .unwrap()
            .tasks
            .values()
            .map(|managed| managed.task.clone())
            .collect::<Vec<_>>();
        tasks.sort_by(|left, right| left.started_at.cmp(&right.started_at));
        tasks
    }

    pub fn read_output(&self, task_id: &str) -> Option<String> {
        let path = self.get_task(task_id)?.output_path;
        std::fs::read_to_string(path).ok()
    }

    /// Request real process termination. The monitor sends termination to the
    /// process group where supported, then reaps the shell child.
    pub fn stop(&self, task_id: &str) -> bool {
        let state = self.inner.state.lock().unwrap();
        let Some(task) = state.tasks.get(task_id) else {
            return false;
        };
        if task.task.status.is_terminal() {
            return false;
        }
        task.cancel.cancel();
        true
    }

    /// Backwards-compatible name for [`Self::stop`].
    pub fn kill(&self, task_id: &str) -> bool {
        self.stop(task_id)
    }

    pub fn cancel_all(&self) {
        let state = self.inner.state.lock().unwrap();
        for task in state.tasks.values() {
            if !task.task.status.is_terminal() {
                task.cancel.cancel();
            }
        }
    }

    /// Wait until one task reaches a terminal state or the wait duration ends.
    pub async fn wait_for_terminal(
        &self,
        task_id: &str,
        timeout: Duration,
    ) -> Option<BackgroundTask> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let task = self.get_task(task_id)?;
            if task.status.is_terminal() {
                return Some(task);
            }
            let notified = self.inner.changed.notified();
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.get_task(task_id);
            }
        }
    }

    /// Drain each terminal notification exactly once.
    pub fn drain_notifications(&self) -> Vec<BackgroundTask> {
        let mut state = self.inner.state.lock().unwrap();
        let mut ready = Vec::new();
        for managed in state.tasks.values_mut() {
            if managed.task.status.is_terminal() && !managed.notified {
                managed.notified = true;
                ready.push(managed.task.clone());
            }
        }
        ready.sort_by(|left, right| left.started_at.cmp(&right.started_at));
        ready
    }
}

/// Compatibility name retained for existing CLI/engine integrations.
pub type BackgroundTaskRegistry = BackgroundTaskManager;

async fn monitor_task(
    owner: Weak<ManagerInner>,
    task_id: String,
    command: String,
    cwd: PathBuf,
    output_path: PathBuf,
    timeout_ms: u64,
    cancel: CancellationToken,
) {
    let output = match std::fs::File::create(&output_path) {
        Ok(file) => file,
        Err(error) => {
            finish_task(&owner, &task_id, TaskStatus::Failed, None);
            tracing::warn!(task_id, %error, "failed to create background task output");
            return;
        }
    };
    let stderr = match output.try_clone() {
        Ok(file) => file,
        Err(error) => {
            finish_task(&owner, &task_id, TaskStatus::Failed, None);
            tracing::warn!(task_id, %error, "failed to clone background task output");
            return;
        }
    };

    #[cfg(windows)]
    let mut process = {
        let mut process = Command::new("cmd");
        process.arg("/C").arg(&command);
        process
    };
    #[cfg(not(windows))]
    let mut process = {
        let mut process = Command::new("sh");
        process.arg("-c").arg(&command);
        // Give the shell and descendants a dedicated process group so stop,
        // cancellation, timeout, and manager drop cannot orphan grandchildren.
        process.process_group(0);
        process
    };

    let child = process
        .current_dir(cwd)
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(stderr))
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            let _ = std::fs::write(&output_path, format!("spawn error: {error}\n"));
            finish_task(&owner, &task_id, TaskStatus::Failed, None);
            return;
        }
    };

    let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timeout);
    let (status, exit_code) = tokio::select! {
        result = child.wait() => match result {
            Ok(exit) if exit.success() => (TaskStatus::Completed, exit.code()),
            Ok(exit) => (TaskStatus::Failed, exit.code()),
            Err(_) => (TaskStatus::Failed, None),
        },
        _ = cancel.cancelled() => {
            terminate_and_reap(&mut child).await;
            (TaskStatus::Killed, None)
        },
        _ = &mut timeout => {
            terminate_and_reap(&mut child).await;
            (TaskStatus::Killed, None)
        },
    };
    finish_task(&owner, &task_id, status, exit_code);
}

fn finish_task(
    owner: &Weak<ManagerInner>,
    task_id: &str,
    status: TaskStatus,
    exit_code: Option<i32>,
) {
    let Some(owner) = owner.upgrade() else {
        return;
    };
    if let Some(managed) = owner.state.lock().unwrap().tasks.get_mut(task_id) {
        managed.task.status = status;
        managed.task.exit_code = exit_code;
        managed.task.completed_at = Some(now_timestamp());
    }
    owner.changed.notify_waiters();
}

async fn terminate_and_reap(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let group = format!("-{pid}");
        let _ = Command::new("kill")
            .args(["-TERM", "--", group.as_str()])
            .status()
            .await;
        if tokio::time::timeout(Duration::from_millis(500), child.wait())
            .await
            .is_ok()
        {
            return;
        }
        let _ = Command::new("kill")
            .args(["-KILL", "--", group.as_str()])
            .status()
            .await;
        let _ = child.wait().await;
        return;
    }

    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn completion_is_queryable_and_notified_once() {
        let manager = BackgroundTaskManager::new();
        let id = manager.spawn("printf lifecycle-ok", 2_000);
        let task = manager
            .wait_for_terminal(&id, Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.exit_code, Some(0));
        assert_eq!(manager.read_output(&id).as_deref(), Some("lifecycle-ok"));
        assert_eq!(manager.drain_notifications().len(), 1);
        assert!(manager.drain_notifications().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_terminates_and_reaps_a_running_process() {
        let manager = BackgroundTaskManager::new();
        let id = manager.spawn("sleep 30", 60_000);
        assert!(manager.stop(&id));
        let task = manager
            .wait_for_terminal(&id, Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(task.status, TaskStatus::Killed);
        assert!(!manager.stop(&id));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn parent_cancellation_prevents_descendant_side_effects() {
        let marker = std::env::temp_dir().join(format!("nonoclaw-bg-marker-{}", Uuid::new_v4()));
        let manager = BackgroundTaskManager::new();
        let cancel = CancellationToken::new();
        let id = manager.spawn_with_cancel(
            &format!("sleep 1; touch '{}'", marker.display()),
            10_000,
            cancel.clone(),
        );
        cancel.cancel();
        let task = manager
            .wait_for_terminal(&id, Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(task.status, TaskStatus::Killed);
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(
            !marker.exists(),
            "cancelled process group left a descendant"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_manager_prevents_background_process_leaks() {
        let marker = std::env::temp_dir().join(format!("nonoclaw-bg-drop-{}", Uuid::new_v4()));
        {
            let manager = BackgroundTaskManager::new();
            manager.spawn(&format!("sleep 1; touch '{}'", marker.display()), 10_000);
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(!marker.exists(), "manager drop left a background process");
    }
}
