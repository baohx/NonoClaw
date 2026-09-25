//! Video generation (Ark Seedance) task service.
//!
//! Task-based content generation API: create → poll → download mp4 to disk.
//! Ledger: `<cwd>/.nonoclaw/video/tasks.jsonl` (append-only). Files:
//! `<cwd>/.nonoclaw/video/{local_id}.mp4`. Console-limited concurrency 3 /
//! RPM 180 is enforced with a local semaphore + token bucket.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Mutex;

use super::connection::AppState;
use super::http_error::json_response;
use super::project_context::ProjectContext;
use nonoclaw_api::{Client, ClientPurpose, RequestParams, SystemBlock};
use nonoclaw_core::{ContentBlock, ImageSource, Message, MessageContent};

const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_TASK_IMAGES: usize = 30;
const ARK_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// Account-level Ark concurrency (console: 并发数 3).
pub(super) const MAX_CONCURRENT_TASKS: usize = 3;
/// Account-level Ark rate limit (console: RPM 180).
const RPM_LIMIT: u64 = 180;

/// Local queue + remote dispatch state shared across handlers.
pub(super) struct VideoStore {
    inner: Mutex<VideoInner>,
}

struct VideoInner {
    /// Local FIFO of pending tasks; a dispatcher takes from the front.
    queue: Vec<String>,
    /// Remote task creation timestamps within the current minute window.
    rpm_window: Vec<std::time::Instant>,
}

impl VideoStore {
    pub(super) fn new() -> Self {
        Self {
            inner: Mutex::new(VideoInner {
                queue: Vec::new(),
                rpm_window: Vec::new(),
            }),
        }
    }

    /// Push a local task id onto the FIFO queue.
    pub(super) fn enqueue(&self, local_id: &str) {
        self.inner
            .lock()
            .expect("video queue poisoned")
            .queue
            .push(local_id.to_string());
    }

    /// Remove a queued (not yet submitted) task; true if it was still queued.
    pub(super) fn dequeue(&self, local_id: &str) -> bool {
        let mut inner = self.inner.lock().expect("video queue poisoned");
        let before = inner.queue.len();
        inner.queue.retain(|id| id != local_id);
        inner.queue.len() != before
    }

    /// Queue position (0-based) for a queued task, None otherwise.
    pub(super) fn queue_position(&self, local_id: &str) -> Option<usize> {
        self.inner
            .lock()
            .expect("video queue poisoned")
            .queue
            .iter()
            .position(|id| id == local_id)
    }

    /// Consume one RPM slot; false if the minute window is exhausted.
    pub(super) fn take_rpm_slot(&self) -> bool {
        let mut inner = self.inner.lock().expect("video queue poisoned");
        let now = std::time::Instant::now();
        inner
            .rpm_window
            .retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        if inner.rpm_window.len() as u64 >= RPM_LIMIT {
            return false;
        }
        inner.rpm_window.push(now);
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct VideoTask {
    /// Local task id (`vid-<n>`), stable across restarts.
    pub(super) id: String,
    /// Remote Ark task id once submitted.
    pub(super) remote_id: Option<String>,
    /// Generation model name.
    pub(super) model: String,
    /// t2v | i2v | reference | first_last_frame
    pub(super) mode: String,
    /// User prompt (text part of the generation content).
    pub(super) prompt: String,
    /// Reference images kept as data URLs, first/last ordered for
    /// first_last_frame mode.
    #[serde(default)]
    pub(super) images: Vec<String>,
    /// Seconds, resolution, ratio, draft mode flag.
    pub(super) duration: u32,
    pub(super) resolution: String,
    pub(super) ratio: String,
    #[serde(default)]
    pub(super) draft: bool,
    /// queued | submitted | running | succeeded | failed | cancelled
    pub(super) status: String,
    pub(super) error: Option<String>,
    /// Relative mp4 file name once persisted.
    pub(super) file: Option<String>,
    pub(super) created_at: u64,
    pub(super) updated_at: u64,
}

fn video_dir(cwd: &std::path::Path) -> PathBuf {
    cwd.join(".nonoclaw").join("video")
}

fn ledger_path(cwd: &std::path::Path) -> PathBuf {
    video_dir(cwd).join("tasks.jsonl")
}

/// Append-only ledger write; last entry for an id wins on read.
fn append_ledger(cwd: &std::path::Path, task: &VideoTask) -> std::io::Result<()> {
    use std::io::Write;
    let dir = video_dir(cwd);
    std::fs::create_dir_all(&dir)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger_path(cwd))?;
    let line = serde_json::to_string(task).map_err(std::io::Error::other)?;
    writeln!(file, "{line}")
}

/// Read the ledger, collapsing to the latest entry per local id.
fn read_ledger(cwd: &std::path::Path) -> Vec<VideoTask> {
    let Ok(text) = std::fs::read_to_string(ledger_path(cwd)) else {
        return vec![];
    };
    let mut order: Vec<String> = Vec::new();
    let mut latest: HashMap<String, VideoTask> = HashMap::new();
    for line in text.lines() {
        let Ok(task) = serde_json::from_str::<VideoTask>(line) else {
            continue;
        };
        if !latest.contains_key(&task.id) {
            order.push(task.id.clone());
        }
        latest.insert(task.id.clone(), task);
    }
    order
        .into_iter()
        .filter_map(|id| latest.remove(&id))
        .collect()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Error helper ────────────────────────────────────────────────────────────

fn video_error(status: StatusCode, message: &str) -> Response {
    json_response(status, &json!({ "error": message }))
}

// ── Handlers ────────────────────────────────────────────────────────────────

/// GET /api/video/models — sanitized video model profiles (no api keys).
pub(super) async fn models_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if !state.authorized(params.get("token").map(String::as_str)) {
        return video_error(StatusCode::UNAUTHORIZED, "invalid or missing auth token");
    }
    let profiles = state.project().config().video_models();
    let models: Vec<serde_json::Value> = profiles
        .iter()
        .map(|profile| {
            json!({
                "name": profile.name,
                "label": profile.label,
                "default": profile.default,
                "capabilities": profile.capabilities,
            })
        })
        .collect();
    json_response(StatusCode::OK, &json!({ "models": models }))
}

#[derive(Deserialize)]
pub(super) struct CreateParams {
    #[serde(default)]
    model: Option<String>,
    /// t2v | i2v | reference | first_last_frame
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    prompt: String,
    #[serde(default = "default_duration")]
    duration: u32,
    #[serde(default = "default_resolution")]
    resolution: String,
    #[serde(default = "default_ratio")]
    ratio: String,
    #[serde(default)]
    draft: bool,
}

fn default_mode() -> String {
    "t2v".into()
}
fn default_duration() -> u32 {
    5
}
fn default_resolution() -> String {
    "720p".into()
}
fn default_ratio() -> String {
    "16:9".into()
}

/// POST /api/video/tasks — multipart: `params` JSON part + `image` parts.
/// Creates the local ledger entry (status=queued). A background dispatcher
/// (spawned per create) submits to Ark when a concurrency slot frees up.
pub(super) async fn create_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    mut multipart: Multipart,
) -> Response {
    if !state.authorized(params.get("token").map(String::as_str)) {
        return video_error(StatusCode::UNAUTHORIZED, "invalid or missing auth token");
    }
    let project = state.project();
    let cwd = project.cwd().to_path_buf();

    // Parse multipart: one `params` JSON + up to N `image` fields.
    let mut create: Option<CreateParams> = None;
    let mut images: Vec<String> = Vec::new();
    let mut total_bytes = 0usize;
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or_default().to_string();
        if name == "params" {
            let Ok(text) = field.text().await else {
                return video_error(StatusCode::BAD_REQUEST, "params part unreadable");
            };
            match serde_json::from_str::<CreateParams>(&text) {
                Ok(parsed) => create = Some(parsed),
                Err(error) => {
                    return video_error(
                        StatusCode::BAD_REQUEST,
                        &format!("invalid params: {error}"),
                    )
                }
            }
        } else if name == "image" {
            let media_type = field
                .content_type()
                .unwrap_or("application/octet-stream")
                .to_string();
            let Ok(bytes) = field.bytes().await else {
                return video_error(StatusCode::BAD_REQUEST, "image part unreadable");
            };
            if bytes.is_empty() {
                continue;
            }
            if !media_type.starts_with("image/") {
                return video_error(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "reference parts must be images",
                );
            }
            if bytes.len() > MAX_IMAGE_BYTES {
                return video_error(StatusCode::PAYLOAD_TOO_LARGE, "image exceeds 10MB limit");
            }
            total_bytes = total_bytes.saturating_add(bytes.len());
            if total_bytes > 6 * MAX_IMAGE_BYTES {
                return video_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "images exceed 60MB total limit",
                );
            }
            if images.len() >= MAX_TASK_IMAGES {
                return video_error(
                    StatusCode::BAD_REQUEST,
                    "too many reference images (max 30)",
                );
            }
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            images.push(format!("data:{media_type};base64,{encoded}"));
        }
    }

    let Some(create) = create else {
        return video_error(StatusCode::BAD_REQUEST, "missing params part");
    };

    // Mode/image validation.
    let profiles = project.config().video_models();
    if profiles.is_empty() {
        return video_error(
            StatusCode::NOT_FOUND,
            "no videoModels configured (settings.json videoModels[])",
        );
    }
    let profile = if let Some(name) = &create.model {
        match profiles.iter().find(|profile| &profile.name == name) {
            Some(profile) => profile,
            None => return video_error(StatusCode::BAD_REQUEST, "unknown video model"),
        }
    } else {
        profiles
            .iter()
            .find(|profile| profile.default)
            .unwrap_or(&profiles[0])
    };
    match create.mode.as_str() {
        "t2v" => {
            if !images.is_empty() {
                return video_error(StatusCode::BAD_REQUEST, "t2v mode takes no images");
            }
        }
        "i2v" | "reference" => {
            if images.is_empty() {
                return video_error(
                    StatusCode::BAD_REQUEST,
                    &format!("{} mode requires images", create.mode),
                );
            }
        }
        "first_last_frame" => {
            if images.len() != 2 {
                return video_error(
                    StatusCode::BAD_REQUEST,
                    "first_last_frame needs exactly 2 images",
                );
            }
        }
        other => return video_error(StatusCode::BAD_REQUEST, &format!("unknown mode {other}")),
    }
    if create.prompt.trim().is_empty() {
        return video_error(StatusCode::BAD_REQUEST, "prompt must not be empty");
    }

    let task = VideoTask {
        id: next_local_id(&cwd),
        remote_id: None,
        model: profile.name.clone(),
        mode: create.mode,
        prompt: create.prompt,
        images,
        duration: create.duration,
        resolution: create.resolution,
        ratio: create.ratio,
        draft: create.draft,
        status: "queued".into(),
        error: None,
        file: None,
        created_at: now_secs(),
        updated_at: now_secs(),
    };
    if let Err(error) = append_ledger(&cwd, &task) {
        return video_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("ledger write failed: {error}"),
        );
    }
    state.video_queue.enqueue(&task.id);
    // Fire-and-forget dispatcher: submits to Ark, then polls to completion
    // and persists the mp4. Later list/get reads poll status on demand too.
    let dispatcher_state = state.clone();
    let dispatch_task = task.clone();
    tokio::spawn(async move {
        run_task(dispatcher_state, dispatch_task).await;
    });

    json_response(StatusCode::CREATED, &json!({ "task": sanitize(&task) }))
}

fn sanitize(task: &VideoTask) -> serde_json::Value {
    // Never expose raw data-URL images in listings (large payloads);
    // the client already knows what it uploaded.
    json!({
        "id": task.id,
        "remoteId": task.remote_id,
        "model": task.model,
        "mode": task.mode,
        "prompt": task.prompt,
        "imageCount": task.images.len(),
        "duration": task.duration,
        "resolution": task.resolution,
        "ratio": task.ratio,
        "draft": task.draft,
        "status": task.status,
        "error": task.error,
        "file": task.file,
        "createdAt": task.created_at,
        "updatedAt": task.updated_at,
    })
}

static LOCAL_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_local_id(cwd: &std::path::Path) -> String {
    // Ledger max + 1 keeps ids monotonic across restarts.
    let ledger_max = read_ledger(cwd)
        .iter()
        .filter_map(|task| task.id.strip_prefix("vid-"))
        .filter_map(|suffix| suffix.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    let start = LOCAL_ID_COUNTER
        .fetch_max(ledger_max, Ordering::Relaxed)
        .max(ledger_max);
    let next = start + 1;
    LOCAL_ID_COUNTER.store(next, Ordering::Relaxed);
    format!("vid-{next}")
}

/// Submit → poll → persist one task. Guarded by the global dispatch permit
/// (account concurrency). Errors land in the ledger, not the HTTP response.
async fn run_task(state: Arc<AppState>, mut task: VideoTask) {
    let project = state.project();
    let cwd = project.cwd().to_path_buf();
    let profiles = project.config().video_models();
    let Some(profile) = profiles.iter().find(|profile| profile.name == task.model) else {
        fail_task(&cwd, &mut task, "video model disappeared from settings");
        return;
    };

    if !state.video_queue.take_rpm_slot() {
        // Rare (180/min); retry the submission after a short backoff.
        tokio::time::sleep(Duration::from_secs(2)).await;
        if !state.video_queue.take_rpm_slot() {
            fail_task(&cwd, &mut task, "local RPM budget exhausted");
            return;
        }
    }

    // Submit.
    let content = build_content(&task);
    let mut body = json!({
        "model": task.model,
        "content": content,
    });
    if let Some(duration) = json!({ "duration": task.duration }).as_object() {
        // duration/resolution/ratio ride as top-level params per Seedance API.
        body.as_object_mut().unwrap().extend(duration.clone());
    }
    body["resolution"] = json!(task.resolution);
    body["ratio"] = json!(task.ratio);
    if task.draft {
        body["draft"] = json!(true);
    }

    let client = reqwest_client();
    let create_url = format!("{}/api/v3/contents/generations/tasks", profile.base_url);
    let response = client
        .post(&create_url)
        .header("Authorization", format!("Bearer {}", profile.api_key))
        .json(&body)
        .timeout(ARK_TIMEOUT)
        .send()
        .await;
    let remote_id = match response {
        Ok(response) if response.status().is_success() => {
            match response.json::<serde_json::Value>().await {
                Ok(value) => value
                    .get("id")
                    .and_then(|id| id.as_str())
                    .map(str::to_string),
                Err(_) => None,
            }
        }
        Ok(response) => {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            fail_task(
                &cwd,
                &mut task,
                &format!("ark create failed: HTTP {status}: {text}"),
            );
            return;
        }
        Err(error) => {
            fail_task(
                &cwd,
                &mut task,
                &format!("ark create request failed: {error}"),
            );
            return;
        }
    };
    let Some(remote_id) = remote_id else {
        fail_task(&cwd, &mut task, "ark create response missing task id");
        return;
    };
    task.remote_id = Some(remote_id.clone());
    task.status = "submitted".into();
    task.updated_at = now_secs();
    let _ = append_ledger(&cwd, &task);

    // Poll until terminal, then persist the mp4 on success.
    let query_url = format!(
        "{}/api/v3/contents/generations/tasks/{remote_id}",
        profile.base_url
    );
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let response = client
            .get(&query_url)
            .header("Authorization", format!("Bearer {}", profile.api_key))
            .timeout(ARK_TIMEOUT)
            .send()
            .await;
        let Ok(response) = response else {
            continue; // transient network error: keep polling
        };
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        }
        let Ok(value) = response.json::<serde_json::Value>().await else {
            continue;
        };
        let Some(status) = value.get("status").and_then(|s| s.as_str()) else {
            continue;
        };
        match status {
            "queued" | "running" => {
                if task.status != "running" {
                    task.status = "running".into();
                    task.updated_at = now_secs();
                    let _ = append_ledger(&cwd, &task);
                }
            }
            "succeeded" => {
                task.status = "succeeded".into();
                task.updated_at = now_secs();
                let url = value
                    .pointer("/content/video_url")
                    .and_then(|u| u.as_str())
                    .map(str::to_string);
                if let Some(url) = url {
                    task.file = persist_mp4(&state, &cwd, &task.id, &url, &client).await;
                }
                let _ = append_ledger(&cwd, &task);
                return;
            }
            "failed" | "expired" => {
                task.status = "failed".into();
                task.error = value
                    .pointer("/error/message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
                    .or_else(|| Some(status.to_string()));
                task.updated_at = now_secs();
                let _ = append_ledger(&cwd, &task);
                return;
            }
            other => {
                // Unknown status: surface it but keep polling a few rounds.
                task.status = other.to_string();
                task.updated_at = now_secs();
                let _ = append_ledger(&cwd, &task);
            }
        }
    }
}

fn fail_task(cwd: &std::path::Path, task: &mut VideoTask, message: &str) {
    task.status = "failed".into();
    task.error = Some(message.to_string());
    task.updated_at = now_secs();
    let _ = append_ledger(cwd, task);
}

/// Build the Seedance `content` array: text part + image parts with roles.
fn build_content(task: &VideoTask) -> serde_json::Value {
    let mut parts = vec![json!({ "type": "text", "text": task.prompt })];
    match task.mode.as_str() {
        "first_last_frame" => {
            parts.push(json!({ "type": "image_url", "image_url": { "url": task.images[0] }, "role": "first_frame" }));
            parts.push(json!({ "type": "image_url", "image_url": { "url": task.images[1] }, "role": "last_frame" }));
        }
        _ => {
            for image in &task.images {
                parts.push(json!({ "type": "image_url", "image_url": { "url": image } }));
            }
        }
    }
    json!(parts)
}

/// Download the mp4 (TOS URLs expire ~24h) to `.nonoclaw/video/{id}.mp4`.
/// Returns the relative file name on success.
async fn persist_mp4(
    state: &Arc<AppState>,
    cwd: &std::path::Path,
    local_id: &str,
    url: &str,
    client: &reqwest::Client,
) -> Option<String> {
    let dir = video_dir(cwd);
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let file_name = format!("{local_id}.mp4");
    let target = dir.join(&file_name);
    for _ in 0..3 {
        let Ok(response) = client.get(url).timeout(DOWNLOAD_TIMEOUT).send().await else {
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        };
        if !response.status().is_success() {
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        }
        let Ok(bytes) = response.bytes().await else {
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        };
        if std::fs::write(&target, &bytes).is_ok() {
            let _ = state; // reserved for future quota notifications
            return Some(file_name);
        }
    }
    None
}

fn reqwest_client() -> reqwest::Client {
    reqwest::Client::builder().build().unwrap_or_default()
}

/// GET /api/video/tasks — ledger view; refreshes in-flight remote statuses.
pub(super) async fn list_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if !state.authorized(params.get("token").map(String::as_str)) {
        return video_error(StatusCode::UNAUTHORIZED, "invalid or missing auth token");
    }
    let cwd = state.project().cwd().to_path_buf();
    let mut tasks = read_ledger(&cwd);
    for task in &mut tasks {
        if let Some(position) = state.video_queue.queue_position(&task.id) {
            task.status = format!("queued+{position}");
        }
    }
    json_response(
        StatusCode::OK,
        &json!({ "tasks": tasks.iter().map(sanitize).collect::<Vec<_>>() }),
    )
}

/// GET /api/video/tasks/:id — one remote status refresh + detail.
pub(super) async fn get_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    Path(task_id): Path<String>,
) -> Response {
    if !state.authorized(params.get("token").map(String::as_str)) {
        return video_error(StatusCode::UNAUTHORIZED, "invalid or missing auth token");
    }
    let cwd = state.project().cwd().to_path_buf();
    let Some(mut task) = read_ledger(&cwd)
        .into_iter()
        .find(|task| task.id == task_id)
    else {
        return video_error(StatusCode::NOT_FOUND, "unknown task");
    };
    refresh_remote(&state, &cwd, &mut task).await;
    json_response(StatusCode::OK, &json!({ "task": sanitize(&task) }))
}

/// On-demand remote refresh (belt-and-braces alongside the dispatcher poll).
async fn refresh_remote(state: &Arc<AppState>, cwd: &std::path::Path, task: &mut VideoTask) {
    if task.status == "succeeded" || task.status == "failed" || task.status == "cancelled" {
        return;
    }
    let Some(remote_id) = task.remote_id.clone() else {
        return;
    };
    let profiles = state.project().config().video_models();
    let Some(profile) = profiles.iter().find(|profile| profile.name == task.model) else {
        return;
    };
    let url = format!(
        "{}/api/v3/contents/generations/tasks/{remote_id}",
        profile.base_url
    );
    let Ok(response) = reqwest_client()
        .get(&url)
        .header("Authorization", format!("Bearer {}", profile.api_key))
        .timeout(ARK_TIMEOUT)
        .send()
        .await
    else {
        return;
    };
    if !response.status().is_success() {
        return;
    }
    let Ok(value) = response.json::<serde_json::Value>().await else {
        return;
    };
    let Some(status) = value.get("status").and_then(|s| s.as_str()) else {
        return;
    };
    match status {
        "succeeded" => {
            task.status = "succeeded".into();
            task.updated_at = now_secs();
            if task.file.is_none() {
                if let Some(video_url) = value
                    .pointer("/content/video_url")
                    .and_then(|u| u.as_str())
                    .map(str::to_string)
                {
                    task.file =
                        persist_mp4(state, cwd, &task.id, &video_url, &reqwest_client()).await;
                }
            }
            let _ = append_ledger(cwd, task);
        }
        "failed" | "expired" => {
            task.status = "failed".into();
            task.error = value
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .or_else(|| Some(status.to_string()));
            task.updated_at = now_secs();
            let _ = append_ledger(cwd, task);
        }
        other if other != task.status => {
            task.status = other.to_string();
            task.updated_at = now_secs();
            let _ = append_ledger(cwd, task);
        }
        _ => {}
    }
}

/// DELETE /api/video/tasks/:id — queued tasks are dequeued (real cancel);
/// terminal tasks delete ledger tail entry + local file. Running remote
/// tasks cannot be cancelled (Ark rejects); we keep the local record.
pub(super) async fn delete_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    Path(task_id): Path<String>,
) -> Response {
    if !state.authorized(params.get("token").map(String::as_str)) {
        return video_error(StatusCode::UNAUTHORIZED, "invalid or missing auth token");
    }
    let cwd = state.project().cwd().to_path_buf();
    let Some(mut task) = read_ledger(&cwd)
        .into_iter()
        .find(|task| task.id == task_id)
    else {
        return video_error(StatusCode::NOT_FOUND, "unknown task");
    };
    if task.status == "queued" || task.remote_id.is_none() {
        if state.video_queue.dequeue(&task_id) || task.status == "queued" {
            task.status = "cancelled".into();
            task.updated_at = now_secs();
            if let Err(error) = append_ledger(&cwd, &task) {
                return video_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("ledger write failed: {error}"),
                );
            }
            return json_response(StatusCode::OK, &json!({ "cancelled": task_id }));
        }
    }
    if task.status == "succeeded" || task.status == "failed" || task.status == "cancelled" {
        if let Some(file) = task.file.clone() {
            let _ = std::fs::remove_file(video_dir(&cwd).join(file));
        }
        // Remote cleanup when the record still exists server-side.
        if let Some(remote_id) = task.remote_id.clone() {
            let profiles = state.project().config().video_models();
            if let Some(profile) = profiles.iter().find(|profile| profile.name == task.model) {
                let url = format!(
                    "{}/api/v3/contents/generations/tasks/{remote_id}",
                    profile.base_url
                );
                let _ = reqwest_client()
                    .delete(&url)
                    .header("Authorization", format!("Bearer {}", profile.api_key))
                    .timeout(ARK_TIMEOUT)
                    .send()
                    .await;
            }
        }
        // Tombstone: keep the id known so duplicate deletes 404 cleanly.
        task.status = "cancelled".into();
        task.file = None;
        task.updated_at = now_secs();
        if let Err(error) = append_ledger(&cwd, &task) {
            return video_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("ledger write failed: {error}"),
            );
        }
        return json_response(StatusCode::OK, &json!({ "deleted": task_id }));
    }
    video_error(
        StatusCode::CONFLICT,
        "running tasks cannot be deleted (Ark rejects cancel)",
    )
}

/// GET /api/video/tasks/:id/file — stream the persisted mp4.
pub(super) async fn file_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    Path(task_id): Path<String>,
) -> Response {
    if !state.authorized(params.get("token").map(String::as_str)) {
        return video_error(StatusCode::UNAUTHORIZED, "invalid or missing auth token");
    }
    let cwd = state.project().cwd().to_path_buf();
    let Some(task) = read_ledger(&cwd)
        .into_iter()
        .find(|task| task.id == task_id)
    else {
        return video_error(StatusCode::NOT_FOUND, "unknown task");
    };
    let Some(file) = task.file else {
        return video_error(StatusCode::NOT_FOUND, "task has no persisted file");
    };
    // Path confinement: task ids and file names are server-generated, but
    // verify anyway (defense in depth against ledger tampering).
    let path = video_dir(&cwd).join(&file);
    if !path.starts_with(video_dir(&cwd)) || file.contains("..") || file.contains('/') {
        return video_error(StatusCode::BAD_REQUEST, "invalid file name");
    }
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return video_error(StatusCode::NOT_FOUND, "file missing on disk");
    };
    (
        StatusCode::OK,
        [
            ("content-type", "video/mp4".to_string()),
            (
                "content-disposition",
                format!("attachment; filename=\"{file}\""),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// Seedance prompt-engineering spec injected into every enhance/vision call.
const ENHANCE_SYSTEM: &str = "You are a Seedance (字节跳动视频生成模型) prompt engineer. \
Rewrite the user's draft into ONE Chinese video-generation prompt, 80-150 字, following this structure: \
主体(外观/服装细节) → 动作/运动 → 镜头语言(推拉摇移/景别) → 光线 → 氛围与风格. \
No preamble, no explanations, no markdown — output the rewritten prompt only.";

#[derive(Deserialize)]
pub(super) struct EnhanceParams {
    /// Draft prompt to rewrite (`action: "enhance"`) — required for enhance.
    #[serde(default)]
    prompt: Option<String>,
    /// `enhance` (default) or `describe` (read reference images, produce
    /// insertable character/scene description fragments).
    #[serde(default)]
    action: Option<String>,
    /// Reference images as base64 data-URLs (used by both actions).
    #[serde(default)]
    images: Vec<String>,
    /// Optional chat model override for the enhancer (defaults to the
    /// active conversation model).
    #[serde(rename = "enhanceModel", default)]
    model: Option<String>,
}

/// Pick the chat client for enhance/describe. An explicit `enhanceModel`
/// request parameter wins; otherwise the active conversation model.
fn enhance_client(
    project: &ProjectContext,
    requested_model: Option<&str>,
) -> Result<Arc<Client>, String> {
    project
        .config()
        .client_for(ClientPurpose::Conversation, requested_model)
        .map_err(|error| format!("no chat client available: {error}"))
}

/// POST /api/video/enhance — prompt enhancement / reference-image reading
/// via a vision chat model (Seedance prompt spec injected; PRD §3 Step 3).
/// Goes through the engine's `Client` so every configured wire format
/// (Anthropic / OpenAI / Responses / Gemini) and auth scheme just works.
pub(super) async fn enhance_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<EnhanceParams>,
) -> Response {
    if !state.authorized(params.get("token").map(String::as_str)) {
        return video_error(StatusCode::UNAUTHORIZED, "invalid or missing auth token");
    }
    let action = body.action.as_deref().unwrap_or("enhance");
    if action != "enhance" && action != "describe" {
        return video_error(
            StatusCode::BAD_REQUEST,
            "action must be enhance or describe",
        );
    }
    let describe = action == "describe";
    if describe && body.images.is_empty() {
        return video_error(StatusCode::BAD_REQUEST, "describe requires images");
    }
    if !describe && body.prompt.as_deref().unwrap_or_default().trim().is_empty() {
        return video_error(
            StatusCode::BAD_REQUEST,
            "enhance requires a non-empty prompt",
        );
    }
    if body.images.len() > MAX_TASK_IMAGES {
        return video_error(StatusCode::BAD_REQUEST, "too many images");
    }

    let project = state.project();
    let model_name = project
        .config()
        .model_for(ClientPurpose::Conversation, body.model.as_deref());
    let client = match enhance_client(&project, Some(&model_name)) {
        Ok(client) => client,
        Err(message) => return video_error(StatusCode::NOT_FOUND, &message),
    };

    let system_text = if describe {
        "You are analyzing reference images for a video-generation workflow. \
Output 2-4 short Chinese description fragments (角色外观/场景/氛围), one per line, \
each ≤40 字, ready to paste into a video prompt. No preamble, no numbering."
    } else {
        ENHANCE_SYSTEM
    };
    let mut user_text = if describe {
        "描述这些参考图中的可用素材：".to_string()
    } else {
        format!(
            "请增强这段视频提示词：{}",
            body.prompt.as_deref().unwrap_or_default()
        )
    };
    if !body.images.is_empty() && !describe {
        user_text.push_str("\n（参考图附后，可结合画面内容改写）");
    }

    // One user turn: text + optional base64 images.
    let mut content = vec![ContentBlock::Text {
        text: user_text,
        cache_control: None,
    }];
    for data_url in &body.images {
        let Some((media_type, data)) = split_data_url(data_url) else {
            return video_error(
                StatusCode::BAD_REQUEST,
                "images must be data:...;base64,... URLs",
            );
        };
        content.push(ContentBlock::Image {
            source: ImageSource {
                kind: "base64".into(),
                media_type: media_type.to_string(),
                data: data.to_string(),
            },
        });
    }

    let params = RequestParams {
        model: model_name.clone(),
        max_tokens: 1024,
        system: vec![SystemBlock {
            kind: "text".into(),
            text: system_text.to_string(),
            cache_control: None,
        }],
        messages: vec![Message::user(MessageContent::from_blocks(content))],
        tools: vec![],
        tool_choice: None,
        thinking: None,
        temperature: Some(0.4),
        betas: vec![],
        extra_body: None,
        trace_label: Some("video-enhance".into()),
        session_id: None,
    };
    let output = match client.run_turn(&params, |_| {}).await {
        Ok(output) => output,
        Err(error) => {
            return video_error(
                StatusCode::BAD_GATEWAY,
                &format!("enhance request failed: {error}"),
            )
        }
    };
    // Fold text blocks out of the turn output.
    let result: String = output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
        .trim()
        .to_string();
    if result.is_empty() {
        return video_error(StatusCode::BAD_GATEWAY, "enhance model returned no text");
    }
    json_response(
        StatusCode::OK,
        &json!({ "result": result, "model": output.model, "action": action }),
    )
}

/// Split `data:<mime>;base64,<payload>` into its parts (Anthropic wire needs
/// them separate). Returns `None` for non-data URLs.
fn split_data_url(data_url: &str) -> Option<(&str, &str)> {
    let rest = data_url.strip_prefix("data:")?;
    let (mime, payload) = rest.split_once(";base64,")?;
    Some((mime, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_roundtrip_latest_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        let mut task = VideoTask {
            id: "vid-1".into(),
            remote_id: None,
            model: "m".into(),
            mode: "t2v".into(),
            prompt: "p".into(),
            images: vec![],
            duration: 5,
            resolution: "720p".into(),
            ratio: "16:9".into(),
            draft: false,
            status: "queued".into(),
            error: None,
            file: None,
            created_at: 1,
            updated_at: 1,
        };
        append_ledger(cwd, &task).expect("append 1");
        task.status = "running".into();
        append_ledger(cwd, &task).expect("append 2");
        let loaded = read_ledger(cwd);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].status, "running");
    }

    #[test]
    fn local_ids_are_monotonic_after_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        let first = next_local_id(cwd);
        let second = next_local_id(cwd);
        assert_eq!(first, "vid-1");
        assert_eq!(second, "vid-2");
        // Simulate restart with an existing ledger.
        let task = VideoTask {
            id: "vid-9".into(),
            remote_id: None,
            model: "m".into(),
            mode: "t2v".into(),
            prompt: "p".into(),
            images: vec![],
            duration: 5,
            resolution: "720p".into(),
            ratio: "16:9".into(),
            draft: false,
            status: "queued".into(),
            error: None,
            file: None,
            created_at: 1,
            updated_at: 1,
        };
        append_ledger(cwd, &task).expect("append");
        let next = next_local_id(cwd);
        assert_eq!(next, "vid-10");
    }

    #[test]
    fn build_content_modes() {
        let mut task = VideoTask {
            id: "vid-1".into(),
            remote_id: None,
            model: "m".into(),
            mode: "t2v".into(),
            prompt: "p".into(),
            images: vec![],
            duration: 5,
            resolution: "720p".into(),
            ratio: "16:9".into(),
            draft: false,
            status: "queued".into(),
            error: None,
            file: None,
            created_at: 1,
            updated_at: 1,
        };
        let content = build_content(&task);
        assert_eq!(content.as_array().unwrap().len(), 1);

        task.mode = "first_last_frame".into();
        task.images = vec![
            "data:image/png;base64,AAA".into(),
            "data:image/png;base64,BBB".into(),
        ];
        let content = build_content(&task);
        let parts = content.as_array().unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1]["role"], "first_frame");
        assert_eq!(parts[2]["role"], "last_frame");

        task.mode = "reference".into();
        let content = build_content(&task);
        assert_eq!(content.as_array().unwrap().len(), 3);
    }

    #[test]
    fn sanitize_hides_image_payloads() {
        let task = VideoTask {
            id: "vid-1".into(),
            remote_id: Some("cgt-1".into()),
            model: "m".into(),
            mode: "reference".into(),
            prompt: "p".into(),
            images: vec!["data:image/png;base64,AAAA".into()],
            duration: 5,
            resolution: "720p".into(),
            ratio: "16:9".into(),
            draft: false,
            status: "running".into(),
            error: None,
            file: None,
            created_at: 1,
            updated_at: 2,
        };
        let value = sanitize(&task);
        assert!(value.get("images").is_none());
        assert_eq!(value["imageCount"], 1);
        assert_eq!(value["remoteId"], "cgt-1");
    }

    #[test]
    fn rpm_window_limits() {
        let store = VideoStore::new();
        for _ in 0..RPM_LIMIT {
            assert!(store.take_rpm_slot());
        }
        assert!(!store.take_rpm_slot());
    }

    #[test]
    fn queue_dequeue_semantics() {
        let store = VideoStore::new();
        store.enqueue("vid-1");
        store.enqueue("vid-2");
        assert_eq!(store.queue_position("vid-1"), Some(0));
        assert_eq!(store.queue_position("vid-2"), Some(1));
        assert!(store.dequeue("vid-1"));
        assert!(!store.dequeue("vid-1"));
        assert_eq!(store.queue_position("vid-2"), Some(0));
        assert_eq!(store.queue_position("missing"), None);
    }
}
