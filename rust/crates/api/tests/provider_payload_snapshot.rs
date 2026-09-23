//! Provider payload field-whitelist snapshot tests.
//!
//! `Message` is a dual-purpose structure: JSONL session persistence AND the
//! source of provider request bodies. Persistence-only fields (e.g. `ts`)
//! must never reach a provider payload — strict schemas (Anthropic) return
//! 400, and tolerant providers see prompt-cache bytes shift every turn.
//! The 2026-08-28 `ts` incident (session 5b3db3ae) is the motivating case.
//!
//! These tests capture the REAL wire payload through a loopback TCP server,
//! then assert the serialized field sets match the provider protocol
//! whitelist. Adding a persistence-only field to `Message`/`ContentBlock`
//! without stripping it in serialization fails here.

use nonoclaw_api::{ApiFormat, Client, RequestParams, SystemBlock, ToolChoice, ToolSchema};
use nonoclaw_core::{ContentBlock, Message, MessageContent, Role, ToolResultContent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Accept one HTTP request, return it, and answer with `response`.
async fn capture_one_request(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        // Read until headers are complete, then until the body (per
        // Content-Length) has fully arrived.
        let header_end = loop {
            let mut chunk = [0_u8; 4096];
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                panic!("client closed before sending headers");
            }
            request.extend_from_slice(&chunk[..n]);
            if let Some(pos) = find_header_end(&request) {
                break pos;
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
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..n]);
        }
        let body = request[header_end..header_end + content_length].to_vec();
        socket.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8(body).expect("request body is UTF-8 JSON")
    });
    (format!("http://{address}"), task)
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

const ANTHROPIC_SSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
event: message_start\ndata: {\"message\":{\"id\":\"m\",\"model\":\"fixture\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n\
event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n\
event: content_block_stop\ndata: {\"index\":0}\n\n\
event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n\
event: message_stop\ndata: {}\n\n";

const OPENAI_SSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"model\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"model\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n";

/// Messages exercising every content-block type, with persistence-only
/// `ts` populated exactly as a JSONL-loaded conversation would have.
fn kitchen_sink_messages() -> Vec<Message> {
    vec![
        Message {
            role: Role::User,
            content: MessageContent::from_text("list the files"),
            ts: Some(1_700_000_000_0111),
        },
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Thinking {
                    thinking: "checking the directory".into(),
                    signature: Some("sig-fixture".into()),
                },
                ContentBlock::Text {
                    text: "running bash".into(),
                    cache_control: None,
                },
                ContentBlock::ToolUse {
                    id: "toolu_fixture_1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                    cache_control: None,
                },
            ]),
            ts: Some(1_700_000_000_0222),
        },
        Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_fixture_1".into(),
                content: ToolResultContent::Text("file_a\nfile_b".into()),
                is_error: Some(false),
                cache_control: None,
            }]),
            ts: Some(1_700_000_000_0333),
        },
    ]
}

fn request_params() -> RequestParams {
    RequestParams {
        model: "fixture-model".into(),
        max_tokens: 64,
        system: vec![SystemBlock {
            kind: "text".into(),
            text: "you are a fixture".into(),
            cache_control: None,
        }],
        messages: kitchen_sink_messages(),
        tools: vec![ToolSchema {
            name: "bash".into(),
            description: "run a command".into(),
            input_schema: serde_json::json!({"type":"object"}),
            cache_control: None,
        }],
        tool_choice: Some(ToolChoice::Auto),
        thinking: None,
        temperature: None,
        betas: vec![],
        extra_body: None,
        trace_label: Some("payload-snapshot".into()),
        session_id: None,
    }
}

/// Recursively assert no JSON object anywhere carries a forbidden key.
fn assert_no_keys(value: &serde_json::Value, forbidden: &[&str]) {
    match value {
        serde_json::Value::Object(map) => {
            for key in map.keys() {
                assert!(
                    !forbidden.contains(&key.as_str()),
                    "persistence-only field `{key}` leaked into provider payload"
                );
            }
            for v in map.values() {
                assert_no_keys(v, forbidden);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                assert_no_keys(item, forbidden);
            }
        }
        _ => {}
    }
}

/// Field set of every JSON object at any depth (top level, messages, blocks).
/// Does NOT descend into free-form containers (tool `input`, schemas, tool
/// `arguments`) — their keys are user content, not protocol surface.
fn collect_keys(value: &serde_json::Value, keys: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                if !keys.iter().any(|e| e == k) {
                    keys.push(k.clone());
                }
                if !matches!(
                    k.as_str(),
                    "input" | "input_schema" | "parameters" | "arguments"
                ) {
                    collect_keys(v, keys);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_keys(item, keys);
            }
        }
        _ => {}
    }
}

/// Persistence-only fields that must never appear in any provider payload.
const FORBIDDEN: &[&str] = &["ts"];

/// Anthropic protocol whitelist: everything `serialize_body_anthropic` may
/// legitimately emit. Extend this list ONLY for provider protocol fields.
const ANTHROPIC_ALLOWED: &[&str] = &[
    // request envelope
    "model",
    "max_tokens",
    "stream",
    "messages",
    "system",
    "tools",
    "tool_choice",
    // message envelope
    "role",
    "content",
    // content blocks
    "type",
    "text",
    "thinking",
    "signature",
    "id",
    "name",
    "input",
    "tool_use_id",
    "is_error",
    "cache_control",
    "source",
    "media_type",
    "data",
    // tools / tool_choice / system block internals
    "description",
    "input_schema",
];

/// OpenAI Chat Completions whitelist for `serialize_body_openai`.
const OPENAI_ALLOWED: &[&str] = &[
    // request envelope
    "model",
    "max_tokens",
    "stream",
    "messages",
    "tools",
    "tool_choice",
    "temperature",
    "stream_options",
    "include_usage",
    // message envelope
    "role",
    "content",
    "tool_calls",
    "tool_call_id",
    "name",
    // tool_calls internals
    "id",
    "type",
    "function",
    "arguments",
    // tools internals
    "description",
    "parameters",
];

#[tokio::test]
async fn anthropic_payload_contains_only_protocol_fields() {
    let (base_url, server) = capture_one_request(ANTHROPIC_SSE).await;
    let client = Client::new(Some("fixture-key".into()), None, base_url).unwrap();
    let params = request_params();
    let _ = client.run_turn(&params, |_event| {}).await;
    let body = server.await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_no_keys(&parsed, FORBIDDEN);

    let mut keys = Vec::new();
    collect_keys(&parsed, &mut keys);
    let unexpected: Vec<String> = keys
        .iter()
        .filter(|k| !ANTHROPIC_ALLOWED.contains(&k.as_str()))
        .cloned()
        .collect();
    assert!(
        unexpected.is_empty(),
        "anthropic payload has non-protocol fields {unexpected:?} — \
         if these are new persistence-only fields, strip them in serialization; \
         if they are new provider protocol fields, extend ANTHROPIC_ALLOWED"
    );

    // All four message turns survived the trip with roles intact.
    let messages = parsed["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[2]["role"], "user");
}

#[tokio::test]
async fn openai_payload_contains_only_protocol_fields() {
    let (base_url, server) = capture_one_request(OPENAI_SSE).await;
    let client = Client::new(Some("fixture-key".into()), None, base_url)
        .unwrap()
        .with_format(ApiFormat::OpenAI);
    let params = request_params();
    let _ = client.run_turn(&params, |_event| {}).await;
    let body = server.await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_no_keys(&parsed, FORBIDDEN);

    let mut keys = Vec::new();
    collect_keys(&parsed, &mut keys);
    let unexpected: Vec<String> = keys
        .iter()
        .filter(|k| !OPENAI_ALLOWED.contains(&k.as_str()))
        .cloned()
        .collect();
    assert!(
        unexpected.is_empty(),
        "openai payload has non-protocol fields {unexpected:?} — \
         if these are new persistence-only fields, strip them in serialization; \
         if they are new provider protocol fields, extend OPENAI_ALLOWED"
    );

    // The tool_use round-trips as assistant tool_calls + role:tool result.
    let messages = parsed["messages"].as_array().unwrap();
    let tool_call_msg = messages
        .iter()
        .find(|m| m["tool_calls"].is_array())
        .expect("assistant tool_calls message present");
    assert_eq!(tool_call_msg["tool_calls"][0]["function"]["name"], "bash");
    let tool_result = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("role:tool result message present");
    assert_eq!(tool_result["tool_call_id"], "toolu_fixture_1");
}
