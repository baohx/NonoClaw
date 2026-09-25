//! VideoGenerate tool: agent-facing video generation (Ark Seedance).
//!
//! Writes the same append-only ledger (`<cwd>/.nonoclaw/video/tasks.jsonl`)
//! and mp4 layout as the web wizard's HTTP service, so tasks created here
//! show up in the Video Studio UI and the Insight rail badge. The tool runs
//! its own submit→poll→download cycle and blocks until the video is ready
//! (or fails) — the result is a local file path the model can pass to Read.

use crate::tool::{Tool, ToolCtx, ToolResult};
use async_trait::async_trait;
use nonoclaw_core::{Error, PermissionDecision, PermissionResult, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const PROMPT: &str = "Generate a video with a Seedance model (paid Ark API). \
Creates a local video task, submits it to the provider, waits for completion \
(typically 1-6 minutes), downloads the mp4, and returns the local file path. \
Costs real money per task (roughly 0.1-1 元).";

const DESCRIPTION: &str = "Create an AI-generated video from a text prompt (and optionally \
reference images) using configured Seedance video models. The prompt should follow the \
Seedance convention: 主体 → 动作 → 镜头语言 → 景别 → 光线 → 风格. Blocks until the video \
finishes and returns the downloaded mp4 path plus task id.";

const SUBMIT_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Hard ceiling for the blocking wait (~8 minutes) so a stuck remote task
/// cannot pin a turn forever; the task stays in the ledger and the wizard
/// can still pick it up later.
const POLL_BUDGET: Duration = Duration::from_secs(8 * 60);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

static LOCAL_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Minimal mirror of the engine's `VideoModelProfile` (the tools crate
/// cannot depend on nonoclaw-engine). Reads the project layer first, then
/// the user layer — same precedence direction as the engine.
#[derive(Deserialize)]
struct ProfileEntry {
    name: String,
    #[serde(rename = "baseUrl")]
    base_url: String,
    #[serde(rename = "apiKey")]
    api_key: String,
    #[serde(default)]
    default: bool,
}

#[derive(Deserialize)]
struct SettingsMirror {
    #[serde(rename = "videoModels", default)]
    video_models: Option<Vec<ProfileEntry>>,
}

fn load_profiles(cwd: &Path) -> Result<Vec<ProfileEntry>> {
    let mut paths = vec![cwd.join(".nonoclaw/settings.json")];
    if let Some(home) = nonoclaw_home_config_dir() {
        paths.push(home.join("settings.json"));
    }
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<SettingsMirror>(&text) else {
            continue;
        };
        if let Some(mut models) = parsed.video_models {
            if !models.is_empty() {
                // `$ENV` reference expansion (engine convention).
                for profile in &mut models {
                    if let Some(name) = profile.api_key.strip_prefix('$') {
                        if let Ok(value) = std::env::var(name) {
                            profile.api_key = value;
                        }
                    }
                }
                return Ok(models);
            }
        }
    }
    Err(Error::Tool {
        tool: "VideoGenerate".into(),
        message: "no videoModels configured (settings videoModels[] is empty or missing)".into(),
    })
}

/// Mirror of the engine's `nonoclaw_config_dir` (XDG-aware home config dir).
fn nonoclaw_home_config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|base| PathBuf::from(base).join("nonoclaw"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|base| base.join("nonoclaw"))
    }
}

fn video_dir(cwd: &Path) -> PathBuf {
    cwd.join(".nonoclaw/video")
}

fn ledger_path(cwd: &Path) -> PathBuf {
    video_dir(cwd).join("tasks.jsonl")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One ledger line: same JSON shape as the HTTP service (`VideoTask`).
#[derive(Deserialize)]
struct LedgerEntry {
    id: String,
}

fn next_local_id(cwd: &Path) -> String {
    let ledger_max = read_ledger_ids(cwd)
        .iter()
        .filter_map(|id| id.strip_prefix("vid-"))
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

fn read_ledger_ids(cwd: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(ledger_path(cwd)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<LedgerEntry>(line).ok())
        .map(|entry| entry.id)
        .collect()
}

fn read_task(cwd: &Path, id: &str) -> Option<Value> {
    let text = std::fs::read_to_string(ledger_path(cwd)).ok()?;
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry.get("id").and_then(Value::as_str) == Some(id))
        .last()
}

fn append_task(cwd: &Path, task: &Value) -> Result<()> {
    std::fs::create_dir_all(video_dir(cwd))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger_path(cwd))?;
    let line = serde_json::to_string(task).map_err(std::io::Error::other)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Rewrite the whole ledger with `task` as the last entry for `task.id`
/// (older entries for that id are dropped — latest-wins on read anyway).
fn replace_task(cwd: &Path, task: &Value) -> Result<()> {
    let id = task
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let text = std::fs::read_to_string(ledger_path(cwd)).unwrap_or_default();
    let mut kept: Vec<String> = text
        .lines()
        .filter(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|entry| entry.get("id").and_then(Value::as_str).map(String::from))
                .as_deref()
                != Some(id.as_str())
        })
        .map(String::from)
        .collect();
    kept.push(serde_json::to_string(task).map_err(std::io::Error::other)?);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(ledger_path(cwd))?;
    for line in &kept {
        writeln!(file, "{line}")?;
    }
    Ok(())
}

fn update_status(cwd: &Path, id: &str, status: &str, error: Option<&str>) -> Result<()> {
    let mut task = read_task(cwd, id)
        .ok_or_else(|| Error::Tool {
            tool: "VideoGenerate".into(),
            message: format!("task {id} vanished from ledger"),
        })?
        .as_object()
        .cloned()
        .ok_or_else(|| Error::Tool {
            tool: "VideoGenerate".into(),
            message: format!("task {id} ledger entry is not an object"),
        })?;
    task.insert("status".into(), json!(status));
    task.insert("updated_at".into(), json!(now_secs()));
    if let Some(error) = error {
        task.insert("error".into(), json!(error));
    }
    replace_task(cwd, &Value::Object(task))
}

/// Parse a `data:<mime>;base64,<payload>` URL into (mime, payload).
fn split_data_url(data_url: &str) -> Option<(&str, &str)> {
    let rest = data_url.strip_prefix("data:")?;
    let (mime, payload) = rest.split_once(";base64,")?;
    Some((mime, payload))
}

/// Build the Ark content-generation payload (single-image i2v or plain t2v).
fn build_payload(model: &str, prompt: &str, images: &[String]) -> Value {
    let text = prompt.to_string();
    // Ark accepts data-URLs for Seedance reference/first-frame images; pass
    // them through after validating the shape (no decode/re-encode round-trip).
    let image_urls: Vec<&str> = images
        .iter()
        .map(String::as_str)
        .filter(|url| split_data_url(url).is_some())
        .collect();
    if image_urls.is_empty() {
        json!({ "model": model, "content": [{ "type": "text", "text": text }] })
    } else {
        let mut content = vec![json!({ "type": "text", "text": text })];
        for url in image_urls {
            content.push(json!({ "type": "image_url", "image_url": { "url": url } }));
        }
        json!({ "model": model, "content": content })
    }
}

pub struct VideoGenerateTool;

#[async_trait]
impl Tool for VideoGenerateTool {
    fn name(&self) -> &str {
        "VideoGenerate"
    }

    fn prompt(&self) -> &str {
        PROMPT
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Video generation prompt. Seedance convention: 主体 → 动作 → 镜头语言 → 景别 → 光线 → 风格."
                },
                "model": {
                    "type": "string",
                    "description": "Video model profile name from videoModels settings. Defaults to the profile marked default."
                },
                "images": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional reference/first-frame images as data:...;base64,... URLs (local files must be base64-encoded first)."
                },
                "duration": { "type": "integer", "description": "Clip length in seconds (model-dependent, e.g. 5 or 10)." },
                "resolution": { "type": "string", "description": "e.g. 720p / 1080p (model-dependent)." },
                "ratio": { "type": "string", "description": "Aspect ratio, e.g. 16:9 / 9:16 / adaptive." }
            },
            "required": ["prompt"]
        })
    }

    async fn check_permissions(&self, _input: &Value, _ctx: &ToolCtx<'_>) -> PermissionResult {
        // Spends real money — always ask, even in permissive modes.
        PermissionDecision::Ask {
            message: "VideoGenerate creates a paid Seedance generation task (~0.1-1 元).".into(),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        // Polls/wait mostly; ledger appends are the only mutation and they
        // are line-atomic appends.
        true
    }

    fn is_destructive(&self, _input: &Value) -> bool {
        false
    }

    fn search_hint(&self) -> Option<&str> {
        Some("video generation seedance text-to-video image-to-video mp4")
    }

    async fn call(
        &self,
        input: Value,
        ctx: &ToolCtx<'_>,
        cancel: CancellationToken,
    ) -> Result<ToolResult> {
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
            .ok_or_else(|| Error::Tool {
                tool: "VideoGenerate".into(),
                message: "prompt is required".into(),
            })?
            .to_string();
        let images: Vec<String> = input
            .get("images")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        if images.len() > 30 {
            return Err(Error::Tool {
                tool: "VideoGenerate".into(),
                message: "at most 30 images".into(),
            });
        }
        let profiles = load_profiles(ctx.cwd)?;
        let profile = match input.get("model").and_then(Value::as_str) {
            Some(wanted) => profiles
                .iter()
                .find(|profile| profile.name == wanted)
                .ok_or_else(|| Error::Tool {
                    tool: "VideoGenerate".into(),
                    message: format!(
                        "unknown video model {wanted:?}; available: {}",
                        profiles
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                })?,
            None => profiles
                .iter()
                .find(|profile| profile.default)
                .unwrap_or_else(|| profiles.first().expect("checked non-empty")),
        };

        let task_id = next_local_id(ctx.cwd);
        let task = json!({
            "id": task_id,
            "remote_id": null,
            "model": profile.name,
            "mode": if images.is_empty() { "t2v" } else { "i2v" },
            "prompt": prompt,
            "images": images,
            "duration": input.get("duration").cloned().unwrap_or(Value::Null),
            "resolution": input.get("resolution").cloned().unwrap_or(Value::Null),
            "ratio": input.get("ratio").cloned().unwrap_or(Value::Null),
            "draft": null,
            "status": "queued",
            "error": null,
            "file": null,
            "created_at": now_secs(),
            "updated_at": now_secs(),
        });
        append_task(ctx.cwd, &task)?;

        match self
            .run_to_completion(&cancel, ctx, &task_id, &profile, &prompt, &images)
            .await
        {
            Ok(file_path) => {
                let size_mb = std::fs::metadata(&file_path)
                    .map(|meta| meta.len() as f64 / (1024.0 * 1024.0))
                    .unwrap_or(0.0);
                Ok(ToolResult::ok(format!(
                    "video generated: {} ({size_mb:.1} MB, task {task_id}, model {})",
                    file_path.display(),
                    profile.name
                )))
            }
            Err(error) => {
                // Leave the failure in the ledger; wizard shows it too.
                let _ = update_status(ctx.cwd, &task_id, "failed", Some(&error));
                Ok(ToolResult::error(format!(
                    "video task {task_id} failed: {error}"
                )))
            }
        }
    }
}

impl VideoGenerateTool {
    /// Submit → poll → download. All state transitions land in the ledger.
    async fn run_to_completion(
        &self,
        cancel: &CancellationToken,
        ctx: &ToolCtx<'_>,
        task_id: &str,
        profile: &ProfileEntry,
        prompt: &str,
        images: &[String],
    ) -> std::result::Result<PathBuf, String> {
        let client = reqwest::Client::builder()
            .timeout(SUBMIT_TIMEOUT)
            .build()
            .map_err(|error| format!("client: {error}"))?;
        let base = profile.base_url.trim_end_matches('/');
        let url = format!("{base}/api/v3/contents/generations/tasks");

        // Submit.
        let payload = build_payload(&profile.name, prompt, images);
        let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", profile.api_key))
            .json(&payload)
            .timeout(SUBMIT_TIMEOUT)
            .send()
            .await
            .map_err(|error| format!("ark create request failed: {error}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("ark create failed: HTTP {status}: {body}"));
        }
        let remote_id = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| value.get("id").and_then(Value::as_str).map(String::from))
            .ok_or_else(|| "ark create response missing task id".to_string())?;
        let _ = update_status(ctx.cwd, task_id, "running", None);
        {
            let mut task = read_task(ctx.cwd, task_id)
                .ok_or_else(|| "task vanished from ledger".to_string())?
                .as_object()
                .cloned()
                .ok_or_else(|| "task entry is not an object".to_string())?;
            task.insert("remote_id".into(), json!(remote_id));
            let _ = replace_task(ctx.cwd, &Value::Object(task));
        }

        // Poll.
        let query_url = format!("{url}/{remote_id}");
        let deadline = tokio::time::Instant::now() + POLL_BUDGET;
        let video_url = loop {
            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
                _ = cancel.cancelled() => {
                    return Err("cancelled while waiting for video".into());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                // Task stays running remotely; the wizard can recover it.
                return Err(format!(
                    "timed out after {}s; task {task_id} is still running remotely and may complete later",
                    POLL_BUDGET.as_secs()
                ));
            }
            let response = client
                .get(&query_url)
                .header("Authorization", format!("Bearer {}", profile.api_key))
                .timeout(SUBMIT_TIMEOUT)
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    return Err(format!("ark query request failed: {error}"));
                }
            };
            let query_status = response.status();
            let body = response.text().await.unwrap_or_default();
            if !query_status.is_success() {
                return Err(format!("ark query failed: HTTP {query_status}: {body}"));
            }
            let Ok(value) = serde_json::from_str::<Value>(&body) else {
                return Err("ark query returned non-JSON".into());
            };
            let task_status = value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match task_status.as_str() {
                "succeeded" => {
                    let video_url = value
                        .pointer("/content/video_url")
                        .and_then(Value::as_str)
                        .map(String::from);
                    match video_url {
                        Some(url) => break url,
                        None => return Err("ark succeeded but no video_url in response".into()),
                    }
                }
                "failed" | "expired" | "cancelled" => {
                    let reason = value
                        .get("error")
                        .map(|error| error.to_string())
                        .unwrap_or_else(|| task_status.clone());
                    return Err(format!("ark task {task_status}: {reason}"));
                }
                _ => {} // queued / running — keep polling
            }
        };

        // Download.
        let _ = update_status(ctx.cwd, task_id, "downloading", None);
        let bytes = client
            .get(&video_url)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|error| format!("download failed: {error}"))?
            .error_for_status()
            .map_err(|error| format!("download failed: {error}"))?
            .bytes()
            .await
            .map_err(|error| format!("download read failed: {error}"))?;
        let file_path = video_dir(ctx.cwd).join(format!("{task_id}.mp4"));
        std::fs::write(&file_path, &bytes).map_err(|error| format!("write mp4 failed: {error}"))?;
        let mut task = read_task(ctx.cwd, task_id)
            .ok_or_else(|| "task vanished from ledger".to_string())?
            .as_object()
            .cloned()
            .ok_or_else(|| "task entry is not an object".to_string())?;
        task.insert("status".into(), json!("succeeded"));
        task.insert("file".into(), json!(file_path.to_string_lossy()));
        task.insert("updated_at".into(), json!(now_secs()));
        let _ = replace_task(ctx.cwd, &Value::Object(task));
        Ok(file_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_local_id_is_monotonic_and_prefixed() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        assert_eq!(next_local_id(cwd), "vid-1");
        assert_eq!(next_local_id(cwd), "vid-2");
        append_task(cwd, &json!({ "id": "vid-7", "status": "succeeded" })).unwrap();
        assert_eq!(next_local_id(cwd), "vid-8");
    }

    #[test]
    fn ledger_round_trip_latest_wins() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        append_task(cwd, &json!({ "id": "vid-1", "status": "queued" })).unwrap();
        update_status(cwd, "vid-1", "failed", Some("boom")).unwrap();
        let task = read_task(cwd, "vid-1").unwrap();
        assert_eq!(task.get("status").and_then(Value::as_str), Some("failed"));
        assert_eq!(task.get("error").and_then(Value::as_str), Some("boom"));
        // Unknown id reads as None.
        assert!(read_task(cwd, "vid-404").is_none());
    }

    #[test]
    fn payload_text_only_vs_image() {
        let plain = build_payload("m", "p", &[]);
        assert!(plain.get("content").unwrap().as_array().unwrap().len() == 1);
        let data_url = "data:image/png;base64,aGVsbG8=";
        let with_image = build_payload("m", "p", &[data_url.to_string()]);
        let content = with_image.get("content").unwrap().as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(
            content[1].pointer("/image_url/url").and_then(Value::as_str),
            Some(data_url)
        );
    }

    #[test]
    fn profiles_load_from_project_layer_with_env_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::create_dir_all(cwd.join(".nonoclaw")).unwrap();
        std::env::set_var("NONOCLAW_TEST_VID_KEY", "k-123");
        std::fs::write(
            cwd.join(".nonoclaw/settings.json"),
            r#"{"videoModels":[{"name":"m1","baseUrl":"http://x","apiKey":"$NONOCLAW_TEST_VID_KEY"}]}"#,
        )
        .unwrap();
        let profiles = load_profiles(cwd).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].api_key, "k-123");
        std::env::remove_var("NONOCLAW_TEST_VID_KEY");
    }
}
