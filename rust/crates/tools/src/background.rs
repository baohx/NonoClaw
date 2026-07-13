//! Background task execution for long-running shell commands. Mirrors CC's
//! `LocalShellTask` + `ShellCommand` state machine: commands can be spawned
//! asynchronously (`run_in_background: true`), output is persisted to disk,
//! and a `<task_notification>` is injected when the task completes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

fn now_rfc3339() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple RFC3339-like format.
    let secs = ts % 86400;
    let days = ts / 86400;
    // Just return ISO-like timestamp string.
    format!("1970-01-01T00:00:00+00:00-{ts}")
}

/// A background shell task.
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

/// Thread-safe registry of background tasks. Shared between the engine loop
/// and the bash tool via `Arc<Mutex<>>`.
pub struct BackgroundTaskRegistry {
    tasks: HashMap<String, BackgroundTask>,
    pending_notifications: Vec<BackgroundTask>,
}

impl BackgroundTaskRegistry {
    pub fn new() -> Self {
        BackgroundTaskRegistry {
            tasks: HashMap::new(),
            pending_notifications: Vec::new(),
        }
    }

    /// Spawn a command in the background. Returns the task ID immediately.
    /// Output is redirected to a temp file. A monitoring task is spawned to
    /// track completion.
    pub fn spawn(&mut self, command: &str, timeout_ms: u64) -> String {
        let id = format!("bg-{}", Uuid::new_v4());
        let output_dir = std::env::temp_dir().join("nonoclaw-tasks");
        let _ = std::fs::create_dir_all(&output_dir);
        let output_path = output_dir.join(format!("{id}.output"));

        // Spawn the process, redirect stdout/stderr to output file.
        let cmd = command.to_string();
        let out = output_path.clone();
        let task_id = id.clone();

        std::thread::spawn(move || {
            use std::process::{Command, Stdio};
            let output_file = std::fs::File::create(&out).unwrap();
            let child = Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .stdout(Stdio::from(output_file.try_clone().unwrap()))
                .stderr(Stdio::from(output_file))
                .stdin(Stdio::null())
                .spawn();

            match child {
                Ok(mut c) => {
                    // Wait with timeout.
                    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
                    loop {
                        match c.try_wait() {
                            Ok(Some(status)) => {
                                let _ = write_status(
                                    &out,
                                    &task_id,
                                    if status.success() {
                                        "completed"
                                    } else {
                                        "failed"
                                    },
                                    status.code(),
                                );
                                return;
                            }
                            Ok(None) => {
                                if Instant::now() > deadline {
                                    let _ = c.kill();
                                    let _ = c.wait();
                                    let _ = write_status(
                                        &out, &task_id, "killed", None,
                                    );
                                    return;
                                }
                                std::thread::sleep(Duration::from_millis(200));
                            }
                            Err(_) => {
                                let _ = write_status(&out, &task_id, "failed", None);
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("spawn error: {e}\n");
                    let _ = std::fs::write(&out, msg.as_bytes());
                    let _ = write_status(&out, &task_id, "failed", None);
                }
            }
        });

        let now = now_rfc3339();
        let task = BackgroundTask {
            id: id.clone(),
            command: command.to_string(),
            status: TaskStatus::Running,
            output_path,
            exit_code: None,
            started_at: now,
            completed_at: None,
        };
        self.tasks.insert(id.clone(), task);
        id
    }

    /// Get task status and (optionally) output.
    pub fn get_task(&self, task_id: &str) -> Option<BackgroundTask> {
        let mut task = self.tasks.get(task_id).cloned()?;
        // Check if the status file indicates completion.
        if task.status == TaskStatus::Running || task.status == TaskStatus::Backgrounded {
            if let Ok(content) = std::fs::read_to_string(&task.output_path) {
                // Parse status markers from output.
                for line in content.lines() {
                    if line.starts_with("__TASK_STATUS__:") {
                        let s = line.trim_start_matches("__TASK_STATUS__:").trim();
                        match s {
                            "completed" => {
                                task.status = TaskStatus::Completed;
                                task.completed_at = Some(now_rfc3339());
                            }
                            "failed" => {
                                task.status = TaskStatus::Failed;
                                task.exit_code = Some(1);
                                task.completed_at = Some(now_rfc3339());
                            }
                            "killed" => {
                                task.status = TaskStatus::Killed;
                                task.completed_at = Some(now_rfc3339());
                            }
                            _ => {}
                        }
                    }
                    if line.starts_with("__TASK_EXIT_CODE__:") {
                        task.exit_code = line
                            .trim_start_matches("__TASK_EXIT_CODE__:")
                            .trim()
                            .parse()
                            .ok();
                    }
                }
            }
        }
        Some(task)
    }

    /// Read task output from disk.
    pub fn read_output(&self, task_id: &str) -> Option<String> {
        let task = self.tasks.get(task_id)?;
        let raw = std::fs::read_to_string(&task.output_path).ok()?;
        // Strip status markers from output.
        let cleaned: String = raw
            .lines()
            .filter(|l| {
                !l.starts_with("__TASK_STATUS__:")
                    && !l.starts_with("__TASK_EXIT_CODE__:")
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some(cleaned)
    }

    /// Kill a running background task.
    pub fn kill(&mut self, task_id: &str) -> bool {
        if let Some(task) = self.tasks.get_mut(task_id) {
            if task.status == TaskStatus::Running
                || task.status == TaskStatus::Backgrounded
            {
                task.status = TaskStatus::Killed;
                task.completed_at = Some(now_rfc3339());
                return true;
            }
        }
        false
    }

    /// Drain pending completion notifications. Called before each turn to
    /// inject `<task_notification>` messages.
    pub fn drain_notifications(&mut self) -> Vec<BackgroundTask> {
        let mut ready: Vec<BackgroundTask> = Vec::new();
        let completed_ids: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, t)| {
                t.status == TaskStatus::Completed
                    || t.status == TaskStatus::Failed
                    || t.status == TaskStatus::Killed
            })
            .map(|(id, _)| id.clone())
            .collect();

        // Check status files for tasks still marked running.
        for id in &completed_ids {
            if let Some(task) = self.get_task(id) {
                if task.status != TaskStatus::Running
                    && task.status != TaskStatus::Backgrounded
                {
                    ready.push(task);
                }
            }
        }

        // Check all tasks by re-reading status files.
        let all_ids: Vec<String> = self.tasks.keys().cloned().collect();
        for id in &all_ids {
            if let Some(task) = self.get_task(id) {
                if task.status != TaskStatus::Running
                    && task.status != TaskStatus::Backgrounded
                    && !self.pending_notifications.iter().any(|t| t.id == task.id)
                    && !ready.iter().any(|t| t.id == task.id)
                {
                    ready.push(task);
                }
            }
        }

        // Mark as notified.
        for t in &ready {
            if !self.pending_notifications.iter().any(|p| p.id == t.id) {
                self.pending_notifications.push(t.clone());
            }
        }
        ready
    }
}

fn write_status(
    path: &std::path::Path,
    _task_id: &str,
    status: &str,
    exit_code: Option<i32>,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
    writeln!(f, "__TASK_STATUS__:{status}")?;
    if let Some(code) = exit_code {
        writeln!(f, "__TASK_EXIT_CODE__:{code}")?;
    }
    Ok(())
}
