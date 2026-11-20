//! Supervised Doubao CLI process transport using its public `stream-json` API.
//!
//! The CLI is not an HTTP wire format and its internal tools are not governed
//! by NonoClaw's permission engine. Every turn therefore runs in a fresh Linux
//! bubblewrap filesystem with an empty workspace and an isolated HOME.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use nonoclaw_core::{ApiErrorKind, Error, MessageContent, Result, Role, StopReason};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::{
    ApiFormat, CapabilityStatus, ProviderError, RequestParams, StreamEvent, StreamFailure,
    TurnOutput,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DoubaoCliConfig {
    pub command: PathBuf,
    pub effort: Option<String>,
    pub timeout: Duration,
}

impl Default for DoubaoCliConfig {
    fn default() -> Self {
        Self {
            command: PathBuf::from("doubao"),
            effort: None,
            timeout: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DoubaoCliClient {
    config: DoubaoCliConfig,
}

impl DoubaoCliClient {
    pub fn new(config: DoubaoCliConfig) -> Result<Self> {
        if !cfg!(target_os = "linux") {
            return Err(Error::Config(
                "doubao_cli requires Linux with bubblewrap; an unsandboxed CLI is never used"
                    .into(),
            ));
        }
        if config.command.as_os_str().is_empty() || config.timeout.is_zero() {
            return Err(Error::Config(
                "doubao_cli requires a non-empty cliCommand and timeoutSecs > 0".into(),
            ));
        }
        if let Some(effort) = &config.effort {
            if effort.is_empty()
                || !effort
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(Error::Config("invalid doubao_cli effort".into()));
            }
        }
        Ok(Self { config })
    }

    async fn execute(
        &self,
        params: &RequestParams,
        on_event: &mut (dyn for<'a> FnMut(&'a StreamEvent) + Send),
        cancel: CancellationToken,
    ) -> std::result::Result<TurnOutput, StreamFailure> {
        if cancel.is_cancelled() {
            return Err(StreamFailure::before_stream(ProviderError::cancelled()));
        }
        let prompt = render_prompt(params).map_err(core_failure)?;
        let executable = executable(&self.config.command).map_err(core_failure)?;
        let workspace = ScratchDir::new("workspace").map_err(core_failure)?;
        let home = ScratchDir::new("home").map_err(core_failure)?;
        let mut command =
            sandbox_command(&executable, workspace.path(), home.path()).map_err(core_failure)?;
        command
            .arg("-C")
            .arg(workspace.path())
            .arg("-p")
            .arg("-")
            .arg("--model")
            .arg(&params.model)
            .arg("--permission-mode")
            .arg("ask_always")
            .arg("--output-format")
            .arg("stream-json");
        if let Some(effort) = &self.config.effort {
            command.arg("--effort").arg(effort);
        }
        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| core_failure(Error::Config(format!("start isolated Doubao CLI: {e}"))))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let input = tokio::spawn(async move {
            let mut stdin = stdin;
            let result = stdin.write_all(prompt.as_bytes()).await;
            let _ = stdin.shutdown().await;
            result
        });
        // Drain stderr to prevent pipe backpressure. Do not echo this private
        // CLI's logs into NonoClaw events: they are not a credential boundary.
        let errors = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut bytes = 0usize;
            while let Ok(Some(line)) = lines.next_line().await {
                bytes = (bytes + line.len()).min(8192);
            }
            bytes
        });
        let mut parser = CliParser::new(params.model.clone());
        let mut lines = BufReader::new(stdout).lines();
        let execution = loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    terminate(&mut child).await;
                    break Err(Error::Cancelled);
                }
                _ = tokio::time::sleep(self.config.timeout) => {
                    terminate(&mut child).await;
                    break Err(Error::Timeout);
                }
                line = lines.next_line() => {
                    match line {
                        Ok(Some(line)) => {
                            if let Err(error) = parser.push(&line, on_event) {
                                terminate(&mut child).await;
                                break Err(error);
                            }
                        }
                        Ok(None) => {
                            break match child.wait().await {
                                Ok(status) if status.success() => parser.finish(),
                                Ok(status) => Err(Error::Network(format!(
                                    "Doubao CLI exited with {status}; private CLI logs are not forwarded"
                                ))),
                                Err(error) => Err(Error::Network(format!("wait for Doubao CLI: {error}"))),
                            };
                        }
                        Err(error) => {
                            terminate(&mut child).await;
                            break Err(Error::Other(format!("read Doubao CLI stream: {error}")));
                        }
                    }
                }
            }
        };
        let _ = input.await;
        let _ = errors.await;
        execution.map_err(|error| {
            let partial = parser.partial();
            let provider_error = match error {
                Error::Cancelled => ProviderError::cancelled(),
                _ => ProviderError::from_core(&error, "execute Doubao CLI turn"),
            };
            StreamFailure {
                error: provider_error,
                partial,
            }
        })
    }
}

#[async_trait]
impl crate::TurnClient for DoubaoCliClient {
    async fn run_turn_with_cancel(
        &self,
        params: &RequestParams,
        on_event: &mut (dyn for<'a> FnMut(&'a StreamEvent) + Send),
        cancel: CancellationToken,
    ) -> std::result::Result<TurnOutput, StreamFailure> {
        self.execute(params, on_event, cancel).await
    }

    fn label(&self) -> &'static str {
        "doubao_cli"
    }

    fn api_format(&self) -> Option<ApiFormat> {
        None
    }

    fn capabilities_for_model(&self, _model: &str) -> crate::ProviderCapabilities {
        let unsupported = CapabilityStatus::Unsupported {
            reason: "not exposed by the public Doubao CLI stream-json contract",
        };
        crate::ProviderCapabilities {
            streaming: CapabilityStatus::Supported,
            thinking: unsupported,
            cache_usage: unsupported,
            prompt_caching: unsupported,
            images: unsupported,
            tools: unsupported,
        }
    }
}

fn core_failure(error: Error) -> StreamFailure {
    StreamFailure::before_stream(ProviderError::from_core(&error, "execute Doubao CLI turn"))
}

#[derive(Deserialize)]
struct CliEvent {
    #[serde(rename = "type")]
    kind: String,
    subtype: Option<String>,
    #[serde(default)]
    is_error: bool,
    result: Option<CliResult>,
    message: Option<CliMessage>,
}

#[derive(Deserialize)]
struct CliMessage {
    content: Vec<CliContent>,
}

#[derive(Deserialize)]
struct CliContent {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct CliResult {
    #[serde(rename = "type")]
    kind: String,
    content: Option<String>,
}

struct CliParser {
    message_id: String,
    model: String,
    text: String,
    finished: bool,
}

impl CliParser {
    fn new(model: String) -> Self {
        Self {
            message_id: String::new(),
            model,
            text: String::new(),
            finished: false,
        }
    }

    fn push(
        &mut self,
        line: &str,
        on_event: &mut (dyn for<'a> FnMut(&'a StreamEvent) + Send),
    ) -> Result<()> {
        let event: CliEvent = serde_json::from_str(line)
            .map_err(|e| Error::Other(format!("invalid Doubao CLI stream-json event: {e}")))?;
        match event.kind.as_str() {
            "system" if event.subtype.as_deref() == Some("init") => {
                on_event(&StreamEvent::MessageStart {
                    message_id: String::new(),
                    model: self.model.clone(),
                    usage: Default::default(),
                });
            }
            // The CLI is itself an agent: it runs its own tools and replays
            // them as `assistant` (tool_use) / `user` (tool_result) frames.
            // NonoClaw only consumes the streamed text deltas and the final
            // result; skip these opaque internal frames.
            "assistant" | "user" => {}
            "stream_event" => {
                #[derive(Deserialize)]
                struct Envelope {
                    message_id: Option<String>,
                    event: serde_json::Value,
                }
                let envelope: Envelope = serde_json::from_str(line).map_err(|e| {
                    Error::Other(format!("invalid Doubao CLI stream envelope: {e}"))
                })?;
                if let Some(id) = envelope.message_id {
                    self.message_id = id;
                }
                if envelope.event.get("type").and_then(|v| v.as_str())
                    == Some("content_block_delta")
                {
                    let delta = envelope
                        .event
                        .get("delta")
                        .and_then(|v| v.as_object())
                        .ok_or_else(|| Error::Other("missing Doubao CLI text delta".into()))?;
                    match delta.get("type").and_then(|v| v.as_str()) {
                        Some("text_delta") => {
                            let text =
                                delta.get("text").and_then(|v| v.as_str()).ok_or_else(|| {
                                    Error::Other("missing Doubao CLI delta text".into())
                                })?;
                            self.text.push_str(text);
                            on_event(&StreamEvent::TextDelta { text: text.into() });
                        }
                        // Internal thinking/tool deltas are not part of the
                        // final text; skip them.
                        _ => {}
                    }
                }
            }
            "result" => {
                if event.is_error || event.subtype.as_deref() != Some("success") {
                    return Err(Error::Api {
                        status: 400,
                        kind: ApiErrorKind::NonRetryable,
                        message: format!(
                            "Doubao CLI task failed (result: {})",
                            event.subtype.as_deref().unwrap_or("unknown")
                        ),
                    });
                }
                let result = event
                    .result
                    .ok_or_else(|| Error::Other("missing Doubao CLI result".into()))?;
                if result.kind != "text" {
                    return Err(Error::Config(format!(
                        "Doubao CLI result type `{}` is not supported",
                        result.kind
                    )));
                }
                if self.text.is_empty() {
                    self.text = result.content.unwrap_or_default();
                    on_event(&StreamEvent::TextDelta {
                        text: self.text.clone(),
                    });
                }
                self.finished = true;
                on_event(&StreamEvent::MessageStop);
            }
            _ => {
                return Err(Error::Other(format!(
                    "unknown Doubao CLI event type `{}`",
                    event.kind
                )));
            }
        }
        Ok(())
    }

    fn partial(&self) -> TurnOutput {
        TurnOutput {
            message_id: self.message_id.clone(),
            model: self.model.clone(),
            content: if self.text.is_empty() {
                vec![]
            } else {
                vec![nonoclaw_core::ContentBlock::text(self.text.clone())]
            },
            stop_reason: self.finished.then_some(StopReason::EndTurn),
            usage: Default::default(),
        }
    }

    fn finish(&self) -> Result<TurnOutput> {
        if !self.finished {
            return Err(Error::Other(
                "Doubao CLI exited without a successful result event".into(),
            ));
        }
        Ok(self.partial())
    }
}

fn render_prompt(params: &RequestParams) -> Result<String> {
    if !params.tools.is_empty() {
        return Err(Error::Config(
            "doubao_cli does not support NonoClaw tools; no tool is forwarded".into(),
        ));
    }
    if params.thinking.is_some() {
        return Err(Error::Config(
            "doubao_cli does not support NonoClaw thinking requests".into(),
        ));
    }
    let mut prompt = String::from(
        "The following is an explicit stateless conversation supplied by NonoClaw. Do not use \
         tools, read files, execute commands, or resume an earlier CLI session. Answer only the \
         latest user message.\n",
    );
    for block in &params.system {
        prompt.push_str("\n<system>\n");
        prompt.push_str(&block.text);
        prompt.push_str("\n</system>\n");
    }
    for message in &params.messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        prompt.push_str(&format!("\n<{role}>\n"));
        match &message.content {
            MessageContent::Text(text) => prompt.push_str(text),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        nonoclaw_core::ContentBlock::Text { text, .. } => prompt.push_str(text),
                        nonoclaw_core::ContentBlock::ToolUse { name, .. } => {
                            prompt.push_str(&format!("\n[Prior tool call omitted: {name}]\n"));
                        }
                        nonoclaw_core::ContentBlock::ToolResult { content, .. } => {
                            prompt.push_str("\n[Prior tool result; the tool is not re-executed]\n");
                            match content {
                                nonoclaw_core::ToolResultContent::Text(text) => {
                                    prompt.push_str(text)
                                }
                                nonoclaw_core::ToolResultContent::Blocks(blocks) => {
                                    for block in blocks {
                                        if let nonoclaw_core::ContentBlock::Text { text, .. } =
                                            block
                                        {
                                            prompt.push_str(text);
                                        }
                                    }
                                }
                            }
                        }
                        nonoclaw_core::ContentBlock::Thinking { .. }
                        | nonoclaw_core::ContentBlock::Image { .. } => {}
                    }
                }
            }
        }
        prompt.push_str(&format!("\n</{role}>\n"));
    }
    enforce_prompt_budget(&mut prompt);
    Ok(prompt)
}

/// Hard stdin limit of the Doubao CLI (`error.message_too_long` beyond this).
const MAX_PROMPT_BYTES: usize = 1024 * 1024;
/// The CLI also caps a single user message at this many characters
/// (`消息过长：…/262144 个字符`).
const MAX_MESSAGE_CHARS: usize = 262_144;
/// Headroom (bytes) reserved for the truncation marker and tags.
const BUDGET_HEADROOM: usize = 1024;

/// The Doubao CLI enforces two input limits, both reported as
/// `message_too_long`: the whole stdin payload must stay under 1 MiB of
/// bytes and under 262,144 characters (the CLI treats stdin as a single
/// user message). Long NonoClaw sessions would otherwise fail every turn
/// with a generic provider error. Drop the oldest content — preferring
/// message boundaries, falling back to a UTF-8 boundary inside a huge
/// single message — and mark the cut.
fn enforce_prompt_budget(prompt: &mut String) {
    let max_bytes = MAX_PROMPT_BYTES - BUDGET_HEADROOM;
    let max_chars = MAX_MESSAGE_CHARS - BUDGET_HEADROOM;
    let len = prompt.len();
    let chars = prompt.chars().count();
    if len <= max_bytes && chars <= max_chars {
        return;
    }
    // Byte offset from which the remaining suffix fits the character budget.
    let char_start = if chars > max_chars {
        let mut remaining = max_chars;
        let mut idx = 0;
        for (i, _) in prompt.char_indices().rev() {
            if remaining == 0 {
                idx = i;
                break;
            }
            remaining -= 1;
        }
        idx
    } else {
        0
    };
    let byte_start = len.saturating_sub(max_bytes);
    // Start from the stricter of the two budgets, then prefer the earliest
    // message boundary at or after it so whole recent messages survive.
    let start = byte_start.max(char_start).max(1);
    let mut cut = None;
    let mut i = start;
    while i < len {
        if prompt.is_char_boundary(i)
            && (prompt[i..].starts_with("\n<user>\n") || prompt[i..].starts_with("\n<assistant>\n"))
        {
            cut = Some(i);
            break;
        }
        i += 1;
    }
    // A single oversized message has no internal boundary: cut it at a UTF-8
    // boundary instead so its tail (which contains the latest question) is
    // preserved.
    let cut = cut.unwrap_or_else(|| {
        let mut i = start;
        while i < len && !prompt.is_char_boundary(i) {
            i += 1;
        }
        i.min(len)
    });
    let dropped_bytes = cut;
    let dropped_chars = prompt[..cut].chars().count();
    let rest = prompt.split_off(cut);
    prompt.clear();
    prompt.push_str(&format!(
        "\n<system>\n[Earlier conversation truncated by NonoClaw: {dropped_bytes} bytes / \
         {dropped_chars} characters dropped to fit the Doubao CLI input limits (1 MiB / \
         262144 chars).]\n</system>\n"
    ));
    prompt.push_str(&rest);
}

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(label: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "nonoclaw-doubao-cli-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&path)
            .map_err(|e| Error::Network(format!("create Doubao CLI {label}: {e}")))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            tracing::warn!(%error, "failed to remove Doubao CLI scratch directory");
        }
    }
}

async fn terminate(child: &mut Child) {
    #[cfg(unix)]
    if let Some(id) = child.id() {
        // SAFETY: signal only the process group created for this child.
        unsafe { libc::kill(-(id as i32), libc::SIGKILL) };
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn executable(command: &Path) -> Result<PathBuf> {
    let path = if command.is_absolute() {
        command.to_path_buf()
    } else if command.components().count() > 1 {
        std::env::current_dir()
            .map_err(|e| Error::Config(format!("resolve cliCommand: {e}")))?
            .join(command)
    } else {
        std::env::var_os("PATH")
            .and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|path| path.join(command))
                    .find(|path| path.is_file())
            })
            .ok_or_else(|| Error::Config("doubao_cli command was not found in PATH".into()))?
    };
    path.canonicalize()
        .map_err(|e| Error::Config(format!("resolve doubao_cli cliCommand: {e}")))
}

fn sandbox_command(command: &Path, workspace: &Path, home: &Path) -> Result<Command> {
    let host_home = PathBuf::from(std::env::var_os("HOME").ok_or_else(|| {
        Error::Config("doubao_cli requires HOME for its existing login state".into())
    })?);
    // The public CLI waits for its private IPC directory before it can emit
    // even stream-json init. Give it a fresh directory rather than host /tmp.
    #[cfg(unix)]
    let ipc = home.join(format!("dcli-{}", unsafe { libc::getuid() }));
    #[cfg(unix)]
    std::fs::create_dir(&ipc)
        .map_err(|e| Error::Network(format!("prepare Doubao CLI IPC directory: {e}")))?;
    let mut process = Command::new("bwrap");
    process
        .arg("--unshare-all")
        .arg("--share-net")
        .arg("--die-with-parent")
        .arg("--new-session")
        .arg("--clearenv")
        .arg("--ro-bind")
        .arg("/usr")
        .arg("/usr")
        .arg("--symlink")
        .arg("usr/lib")
        .arg("/lib")
        .arg("--symlink")
        .arg("usr/lib64")
        .arg("/lib64")
        .arg("--symlink")
        .arg("usr/bin")
        .arg("/bin")
        .arg("--symlink")
        .arg("usr/sbin")
        .arg("/sbin")
        .arg("--proc")
        .arg("/proc")
        .arg("--dev")
        .arg("/dev")
        .arg("--tmpfs")
        .arg("/tmp")
        .arg("--bind")
        .arg(&ipc)
        .arg(Path::new("/tmp").join(ipc.file_name().expect("IPC directory name")))
        .arg("--bind")
        .arg(home)
        .arg(home)
        .arg("--bind")
        .arg(workspace)
        .arg(workspace)
        .arg("--chdir")
        .arg(workspace)
        .arg("--setenv")
        .arg("HOME")
        .arg(home)
        .arg("--setenv")
        .arg("CI")
        .arg("1")
        .arg("--setenv")
        .arg("DOUBAO_CLI_NO_UPDATE_CHECK")
        .arg("1");

    let data = host_home.join(".local/share/doubao_cli");
    if data.is_dir() {
        let target = home.join(".local/share/doubao_cli");
        std::fs::create_dir_all(&target)
            .map_err(|e| Error::Network(format!("prepare isolated Doubao CLI state: {e}")))?;
        process.arg("--bind").arg(data).arg(target);
    }
    let config = host_home.join(".config/doubao_cli");
    if config.is_dir() {
        let target = home.join(".config/doubao_cli");
        std::fs::create_dir_all(&target)
            .map_err(|e| Error::Network(format!("prepare isolated Doubao CLI config: {e}")))?;
        process.arg("--bind").arg(config).arg(target);
    }

    bind_executable(&mut process, command)?;
    let mut node_path = None;
    if let Some(node) = executable(Path::new("node"))
        .ok()
        .filter(|p| !p.starts_with("/usr"))
    {
        // A version-manager installation needs its libraries and node_modules,
        // not just the node binary. Mount only this Node version, never HOME.
        let root = node
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| Error::Config("cannot determine the Node runtime root".into()))?;
        process.arg("--ro-bind").arg(root).arg(root);
        node_path = node.parent().map(Path::to_path_buf);
    }
    let path = node_path
        .map(|path| format!("{}:/usr/local/bin:/usr/bin:/bin", path.display()))
        .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    process.arg("--setenv").arg("PATH").arg(path);

    for path in ["/etc/resolv.conf", "/etc/hosts", "/etc/ssl/certs"] {
        if Path::new(path).exists() {
            process.arg("--ro-bind").arg(path).arg(path);
        }
    }
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "LANG",
        "LC_ALL",
        "TZ",
    ] {
        if let Some(value) = std::env::var_os(name) {
            process.arg("--setenv").arg(name).arg(value);
        }
    }
    process.arg("--").arg(command);
    Ok(process)
}

fn bind_executable(process: &mut Command, command: &Path) -> Result<()> {
    if command.starts_with("/usr") {
        return Ok(());
    }
    // The official launcher is inside the package. A custom command may be a
    // standalone executable, so only that file is exposed.
    let package_marker = Path::new("node_modules/@bytedance");
    let package_root = command
        .ancestors()
        .find(|path| path.ends_with(package_marker))
        .map(Path::to_path_buf);
    let source = package_root.as_deref().unwrap_or(command);
    process.arg("--ro-bind").arg(source).arg(source);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TurnClient;
    use nonoclaw_core::{ContentBlock, Message, Usage};

    fn text(output: &TurnOutput) -> &str {
        match &output.content[0] {
            ContentBlock::Text { text, .. } => text,
            _ => panic!("expected text output"),
        }
    }

    fn params() -> RequestParams {
        RequestParams {
            model: "4".into(),
            max_tokens: 100,
            system: vec![],
            messages: vec![Message::user(MessageContent::from_text("hello"))],
            tools: vec![],
            tool_choice: None,
            thinking: None,
            temperature: None,
            betas: vec![],
            extra_body: None,
            trace_label: None,
            session_id: None,
        }
    }

    #[test]
    fn folds_public_delta_assistant_and_result_without_usage_invention() {
        let mut parser = CliParser::new("4".into());
        let mut events = vec![];
        let mut on_event = |event: &StreamEvent| events.push(format!("{event:?}"));
        parser
            .push(
                r#"{"type":"system","subtype":"init","session_id":"private"}"#,
                &mut on_event,
            )
            .unwrap();
        parser
            .push(r#"{"type":"stream_event","message_id":"m1","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"OK"}}}"#, &mut on_event)
            .unwrap();
        parser
            .push(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"OK"}]}}"#,
                &mut on_event,
            )
            .unwrap();
        parser
            .push(r#"{"type":"result","subtype":"success","is_error":false,"result":{"type":"text","content":"OK"}}"#, &mut on_event)
            .unwrap();
        let output = parser.finish().unwrap();
        assert_eq!(output.message_id, "m1");
        assert_eq!(output.model, "4");
        assert_eq!(output.usage, Usage::default());
        assert_eq!(text(&output), "OK");
        assert_eq!(events.len(), 3, "assistant must not duplicate the delta");
    }

    #[test]
    fn prompt_budget_keeps_recent_history_and_marks_dropped_prefix() {
        let filler = "x".repeat(64 * 1024);
        let mut prompt = String::new();
        for i in 0..40 {
            prompt.push_str(&format!("\n<user>\nmsg-{i} {filler}\n</user>\n"));
        }
        prompt.push_str("\n<user>\nfinal question\n</user>\n");
        assert!(prompt.len() > MAX_PROMPT_BYTES);
        enforce_prompt_budget(&mut prompt);
        assert!(prompt.len() <= MAX_PROMPT_BYTES, "cut must respect the CLI limit");
        assert!(
            prompt.chars().count() <= MAX_MESSAGE_CHARS,
            "cut must respect the per-message character limit"
        );
        assert!(
            prompt.contains("final question"),
            "the latest user message must survive the cut"
        );
        assert!(
            prompt.contains("dropped to fit the Doubao CLI"),
            "dropped prefix must be marked"
        );
        assert!(!prompt.contains("msg-0"), "oldest messages are dropped");
    }

    #[test]
    fn prompt_budget_leaves_short_prompts_untouched() {
        let mut prompt = String::from("\n<user>\nhello\n</user>\n");
        let before = prompt.clone();
        enforce_prompt_budget(&mut prompt);
        assert_eq!(prompt, before);
    }

    #[test]
    fn prompt_budget_cuts_a_single_oversized_message_at_utf8_boundary() {
        let mut prompt = String::from("\n<user>\n");
        prompt.push_str(&"词".repeat(600_000)); // 1.8 MB inside one message
        prompt.push_str("\n最终问题：请只回答 OK\n</user>\n");
        assert!(prompt.len() > MAX_PROMPT_BYTES);
        enforce_prompt_budget(&mut prompt);
        assert!(prompt.len() <= MAX_PROMPT_BYTES, "must fit the CLI limit");
        assert!(
            prompt.chars().count() <= MAX_MESSAGE_CHARS,
            "must fit the character limit"
        );
        assert!(
            prompt.contains("请只回答 OK"),
            "the tail of the single message must survive"
        );
        assert!(prompt.contains("dropped to fit the Doubao CLI"));
    }

    #[test]
    fn prompt_budget_respects_char_limit_when_bytes_fit() {
        // ~750 KB of Chinese text: under the 1 MiB byte cap but ~260k chars
        // would still exceed... make it clearly over 262,144 chars while
        // staying under 1 MiB: 300k chars × 3 bytes = 900 KB.
        let mut prompt = String::from("\n<user>\n");
        prompt.push_str(&"词".repeat(300_000));
        prompt.push_str("\n最后请只回答 OK\n</user>\n");
        assert!(prompt.len() < MAX_PROMPT_BYTES, "bytes must already fit");
        assert!(prompt.chars().count() > MAX_MESSAGE_CHARS);
        enforce_prompt_budget(&mut prompt);
        assert!(prompt.chars().count() <= MAX_MESSAGE_CHARS, "chars must fit");
        assert!(prompt.len() <= MAX_PROMPT_BYTES);
        assert!(
            prompt.contains("请只回答 OK"),
            "the latest question must survive"
        );
        assert!(prompt.contains("characters dropped"));
    }

    #[test]
    fn rejects_tools_and_internal_tool_output() {
        let mut request = params();
        request.tools.push(crate::ToolSchema {
            name: "Read".into(),
            description: "read".into(),
            input_schema: serde_json::json!({"type":"object"}),
            cache_control: None,
        });
        assert!(
            render_prompt(&request)
                .unwrap_err()
                .to_string()
                .contains("tools")
        );
        // The CLI runs its own internal tools (assistant tool_use → user
        // tool_result frames). These are opaque noise: the parser must skip
        // them and still surface the streamed text.
        let mut parser = CliParser::new("4".into());
        parser
            .push(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#, &mut |_| {})
            .unwrap();
        parser
            .push(r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"x","content":[{"type":"text","text":"ls"}]}]}}"#, &mut |_| {})
            .unwrap();
        parser
            .push(r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}}"#, &mut |_| {})
            .unwrap();
        parser
            .push(r#"{"type":"result","subtype":"success","is_error":false,"result":{"type":"text","content":"done"}}"#, &mut |_| {})
            .unwrap();
        let output = parser
            .finish()
            .unwrap();
        assert_eq!(text(&output), "done");
    }

    #[test]
    fn replays_explicit_text_history_without_session_identifiers() {
        let mut request = params();
        request.messages.insert(
            0,
            Message::assistant(MessageContent::from_text("previous answer")),
        );
        let prompt = render_prompt(&request).unwrap();
        assert!(prompt.contains("previous answer"));
        assert!(prompt.contains("hello"));
        assert!(!prompt.contains("--resume"));
    }

    #[cfg(target_os = "linux")]
    fn fixture(body: &str) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doubao-fixture");
        std::fs::write(&path, format!("#!/usr/bin/python3\n{body}")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        (dir, path)
    }

    #[cfg(target_os = "linux")]
    fn client(path: PathBuf, timeout: Duration) -> DoubaoCliClient {
        DoubaoCliClient::new(DoubaoCliConfig {
            command: path,
            effort: Some("low".into()),
            timeout,
        })
        .unwrap()
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires a locally authenticated public Doubao CLI and network access"]
    async fn official_cli_smoke_returns_text_without_usage() {
        let mut request = params();
        request.messages[0].content = MessageContent::from_text("只输出两个字符：OK");
        let client = client(PathBuf::from("doubao"), Duration::from_secs(90));
        let mut events = vec![];
        let output = client
            .run_turn(&request, &mut |event| events.push(format!("{event:?}")))
            .await
            .unwrap();
        assert_eq!(text(&output), "OK");
        assert_eq!(output.usage, Usage::default());
        assert_eq!(events.len(), 3);
        assert!(!events.iter().any(|event| event.contains("MessageDelta")));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandboxed_process_reads_stdin_and_cannot_read_the_project() {
        let project_file = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
        let (_dir, path) = fixture(&format!(
            r#"import json, pathlib, sys
args = sys.argv[1:]
assert args[args.index('-p') + 1] == '-'
assert '--resume' not in args
prompt = sys.stdin.read()
assert 'previous answer' in prompt and 'hello' in prompt
try:
    pathlib.Path({project_file:?}).read_text()
    project_readable = True
except OSError:
    project_readable = False
result = {{'workspace_empty': not any(pathlib.Path.cwd().iterdir()), 'project_readable': project_readable}}
print(json.dumps({{'type': 'system', 'subtype': 'init'}}), flush=True)
print(json.dumps({{'type': 'stream_event', 'event': {{'type': 'content_block_delta', 'delta': {{'type': 'text_delta', 'text': json.dumps(result)}}}}}}), flush=True)
print(json.dumps({{'type': 'assistant', 'message': {{'content': [{{'type': 'text', 'text': json.dumps(result)}}]}}}}), flush=True)
print(json.dumps({{'type': 'result', 'subtype': 'success', 'result': {{'type': 'text', 'content': json.dumps(result)}}}}), flush=True)
"#,
            project_file = project_file.to_string_lossy()
        ));
        let mut request = params();
        request.messages.insert(
            0,
            Message::assistant(MessageContent::from_text("previous answer")),
        );
        let output = client(path, Duration::from_secs(10))
            .run_turn(&request, &mut |_| {})
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(text(&output)).unwrap();
        assert_eq!(result["workspace_empty"], true);
        assert_eq!(result["project_readable"], false);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn process_failure_preserves_partial_output() {
        let (_dir, path) = fixture(
            r#"import json, sys
sys.stdin.read()
print(json.dumps({'type': 'stream_event', 'event': {'type': 'content_block_delta', 'delta': {'type': 'text_delta', 'text': 'usable'}}}), flush=True)
print(json.dumps({'type': 'result', 'subtype': 'error', 'is_error': True, 'errors': ['task failed']}), flush=True)
"#,
        );
        let failure = client(path, Duration::from_secs(10))
            .run_turn(&params(), &mut |_| {})
            .await
            .unwrap_err();
        assert_eq!(text(&failure.partial), "usable");
        assert!(!failure.error.retryable);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancellation_and_timeout_terminate_the_process() {
        let body = r#"import sys, time
sys.stdin.read()
time.sleep(30)
"#;
        let (_cancel_dir, path) = fixture(body);
        let cancel = CancellationToken::new();
        let guard = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            guard.cancel();
        });
        let failure = client(path, Duration::from_secs(10))
            .run_turn_with_cancel(&params(), &mut |_| {}, cancel)
            .await
            .unwrap_err();
        assert_eq!(failure.error.code, crate::ProviderErrorCode::Cancelled);
        let (_timeout_dir, path) = fixture(body);
        let started = std::time::Instant::now();
        let failure = client(path, Duration::from_millis(100))
            .run_turn(&params(), &mut |_| {})
            .await
            .unwrap_err();
        assert_eq!(failure.error.code, crate::ProviderErrorCode::Timeout);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
