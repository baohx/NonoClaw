//! Prefix-stability guard: replay a multi-turn engine session against a
//! loopback SSE fixture and assert the wire payload obeys the prompt-cache
//! contract turn over turn:
//!
//! * system Block 1 (every block except the trailing Block 2) is
//!   byte-identical across every turn of a run;
//! * the tools array is byte-identical across every turn of a run;
//! * request messages form a growing prefix: turn N's messages (with the
//!   rolling `cache_control` marker stripped) are a byte prefix of turn N+1's.
//!
//! Any violation means a mid-run system/tools rewrite or an in-place message
//! edit that silently invalidates the provider's prompt cache. Failures
//! print the first drift offset plus surrounding context.

use std::sync::{Arc, Mutex};

use nonoclaw_api::Client;
use nonoclaw_core::{MessageContent, PermissionMode};
use nonoclaw_engine::{EngineOptions, QueryEngine};
use nonoclaw_tools::register_all;

// ---------------------------------------------------------------------------
// Loopback capture server
// ---------------------------------------------------------------------------

type Captures = Arc<Mutex<Vec<serde_json::Value>>>;

/// Sequential scripted SSE responses. Each accepted connection pops the next
/// response; each captured request body is recorded for later assertions.
async fn spawn_scripted_server(responses: Vec<String>) -> (String, Captures) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captures: Captures = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&captures);
    tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let mut request = Vec::new();
            let header_end = loop {
                let mut chunk = [0_u8; 4096];
                let n = match socket.read(&mut chunk).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };
                request.extend_from_slice(&chunk[..n]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_string();
            let content_length: usize = headers
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse().ok())?
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let mut chunk = [0_u8; 4096];
                match socket.read(&mut chunk).await {
                    Ok(n) if n > 0 => request.extend_from_slice(&chunk[..n]),
                    _ => break,
                }
            }
            let body = request[header_end..header_end + content_length].to_vec();
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) {
                captured.lock().unwrap().push(value);
            }
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (format!("http://{address}"), captures)
}

// ---------------------------------------------------------------------------
// SSE fixture scripts
// ---------------------------------------------------------------------------

fn sse_wrap(events: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{events}"
    )
}

fn text_frame(text: &str) -> String {
    sse_wrap(&format!(
        "event: message_start\ndata: {{\"message\":{{\"id\":\"m\",\"model\":\"fixture\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n\
         event: content_block_start\ndata: {{\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\ndata: {{\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n\
         event: content_block_stop\ndata: {{\"index\":0}}\n\n\
         event: message_delta\ndata: {{\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":1}}}}\n\n\
         event: message_stop\ndata: {{}}\n\n"
    ))
}

fn tool_call_frame(tool: &str, id: &str, input_json: &str) -> String {
    let escaped = input_json.replace('\\', "\\\\").replace('"', "\\\"");
    sse_wrap(&format!(
        "event: message_start\ndata: {{\"message\":{{\"id\":\"m\",\"model\":\"fixture\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n\
         event: content_block_start\ndata: {{\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{tool}\"}}}}\n\n\
         event: content_block_delta\ndata: {{\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{escaped}\"}}}}\n\n\
         event: content_block_stop\ndata: {{\"index\":0}}\n\n\
         event: message_delta\ndata: {{\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"output_tokens\":1}}}}\n\n\
         event: message_stop\ndata: {{}}\n\n"
    ))
}

// ---------------------------------------------------------------------------
// Drift reporting
// ---------------------------------------------------------------------------

fn context(bytes: &[u8], offset: usize) -> String {
    let lo = offset.saturating_sub(64);
    let hi = (offset + 64).min(bytes.len());
    String::from_utf8_lossy(&bytes[lo..hi]).replace(['\n', '\r'], "\\n")
}

fn drift_report(label: &str, prev: &serde_json::Value, next: &serde_json::Value) -> String {
    let prev_bytes = serde_json::to_vec(prev).unwrap_or_default();
    let next_bytes = serde_json::to_vec(next).unwrap_or_default();
    let offset = prev_bytes
        .iter()
        .zip(next_bytes.iter())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| prev_bytes.len().min(next_bytes.len()));
    format!(
        "{label} drifted at byte offset {offset}\n  prev …{}\n  next …{}",
        context(&prev_bytes, offset),
        context(&next_bytes, offset),
    )
}

/// The rolling cache breakpoint hops to the last message each turn, so the
/// same message gains/loses a `cache_control` field between turns. Strip it
/// from every content block before comparing message prefixes.
fn messages_without_cache_markers(request: &serde_json::Value) -> Vec<serde_json::Value> {
    let mut messages: Vec<serde_json::Value> =
        request["messages"].as_array().cloned().unwrap_or_default();
    for message in messages.iter_mut() {
        let Some(content) = message
            .as_object_mut()
            .and_then(|m| m.get_mut("content"))
            .and_then(|c| c.as_array_mut())
        else {
            continue;
        };
        for block in content.iter_mut() {
            if let Some(obj) = block.as_object_mut() {
                obj.remove("cache_control");
            }
        }
    }
    messages
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_payload_prefix_stable_across_turns() {
    // Pin session/memory storage away from the developer's real home so the
    // engine writes nothing outside the temp sandbox and nothing read from
    // it can vary between runs of this test.
    let _env = TestEnv::pin();
    let sandbox = SandboxDir::new();

    // Script: three tool turns then a final text answer — 4 provider
    // requests, exercising tool round-trips (the shape that historically
    // mutated history in place) and TodoWrite (whose recap message is
    // appended mid-run).
    let responses = vec![
        tool_call_frame(
            "TodoWrite",
            "toolu_prefix_1",
            r#"{"todos":[{"content":"verify prefix stability","status":"completed"},{"content":"cleanup","status":"pending"}]}"#,
        ),
        tool_call_frame(
            "TodoWrite",
            "toolu_prefix_2",
            r#"{"todos":[{"content":"verify prefix stability","status":"completed"},{"content":"cleanup","status":"completed"}]}"#,
        ),
        tool_call_frame(
            "TodoWrite",
            "toolu_prefix_3",
            r#"{"todos":[{"content":"verify prefix stability","status":"completed"}]}"#,
        ),
        text_frame("all done"),
    ];
    let (base_url, captures) = spawn_scripted_server(responses).await;

    let client = Arc::new(Client::new(Some("fixture-key".into()), None, base_url).unwrap());
    let (registry, todos) = register_all();
    let mut engine = QueryEngine::new(
        client,
        Arc::new(registry),
        todos,
        EngineOptions {
            permission_mode: PermissionMode::Auto,
            is_non_interactive: true,
            max_turns: 10,
            ..EngineOptions::default()
        },
    );

    let result = engine
        .run(
            MessageContent::from_text("track these two tasks"),
            &sandbox.path,
            |_| {},
        )
        .await
        .expect("engine run completes against fixture");

    assert_eq!(result.turns, 4, "fixture script must drive 4 turns");
    let requests: Vec<serde_json::Value> = captures.lock().unwrap().clone();
    assert_eq!(requests.len(), 4, "one provider request per turn");

    // --- system: every block except the trailing Block 2 is byte-stable ---
    // Block 2 (date/git/skills) refreshes every turn by design and is always
    // appended last, so prefix blocks must never drift.
    for (index, request) in requests.iter().enumerate().skip(1) {
        let prev = &requests[index - 1]["system"];
        let next = &request["system"];
        let prev_blocks = prev
            .as_array()
            .unwrap_or_else(|| panic!("turn {index}: system is not a block array: {prev}"));
        let next_blocks = next
            .as_array()
            .unwrap_or_else(|| panic!("turn {index}: system is not a block array: {next}"));
        let stable = prev_blocks.len().saturating_sub(1);
        assert!(
            next_blocks.len() >= stable,
            "turn {index}: system Block 1 shrank ({} -> {} blocks)",
            prev_blocks.len(),
            next_blocks.len()
        );
        for block in 0..stable {
            assert!(
                prev_blocks[block] == next_blocks[block],
                "{}",
                drift_report(
                    &format!("system block {block} (turn {index})"),
                    &prev_blocks[block],
                    &next_blocks[block],
                )
            );
        }
    }

    // --- tools: byte-identical across every turn (append-only contract) ----
    for (index, request) in requests.iter().enumerate().skip(1) {
        let prev = &requests[index - 1]["tools"];
        let next = &request["tools"];
        assert!(
            prev == next,
            "{}",
            drift_report(&format!("tools (turn {index})"), prev, next)
        );
    }

    // --- messages: element-wise stable (rolling cache marker stripped) -----
    // A serialized array's closing `]` can never be a byte-prefix of the
    // longer form, so compare per-message: every message from an earlier
    // turn must serialize identically (modulo the hopping cache_control) in
    // every later turn. History must only grow.
    for index in 1..requests.len() {
        let prev_messages = messages_without_cache_markers(&requests[index - 1]);
        let next_messages = messages_without_cache_markers(&requests[index]);
        assert!(
            next_messages.len() >= prev_messages.len(),
            "messages history shrank between turns {index} -> {}: {} -> {} messages",
            index + 1,
            prev_messages.len(),
            next_messages.len()
        );
        for (position, (earlier, later)) in
            prev_messages.iter().zip(next_messages.iter()).enumerate()
        {
            assert!(
                earlier == later,
                "{}",
                drift_report(
                    &format!("messages[{position}] (turn {index} -> {})", index + 1),
                    earlier,
                    later,
                )
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Pin `NONOCLAW_HOME` and `HOME` to a private temp dir for the test's
/// lifetime (restored on drop).
struct TestEnv {
    prev_home: Option<std::ffi::OsString>,
    prev_nonoclaw: Option<std::ffi::OsString>,
    _dir: SandboxDir,
}

impl TestEnv {
    fn pin() -> Self {
        let dir = SandboxDir::new();
        let prev_home = std::env::var_os("HOME");
        let prev_nonoclaw = std::env::var_os("NONOCLAW_HOME");
        std::env::set_var("NONOCLAW_HOME", dir.path.as_os_str());
        std::env::set_var("HOME", dir.path.as_os_str());
        Self {
            prev_home,
            prev_nonoclaw,
            _dir: dir,
        }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        match self.prev_nonoclaw.take() {
            Some(v) => std::env::set_var("NONOCLAW_HOME", v),
            None => std::env::remove_var("NONOCLAW_HOME"),
        }
        match self.prev_home.take() {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

/// Temp directory removed on drop (no tempfile dep; std + uuid).
struct SandboxDir {
    path: std::path::PathBuf,
}

impl SandboxDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nonoclaw-prefix-stability-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).expect("create sandbox dir");
        Self { path }
    }
}

impl Drop for SandboxDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
