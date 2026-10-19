//! Chrome DevTools Protocol client for the VM's headless Chrome (port 9222).
//!
//! Transport: plain TCP to the guest (Firecracker: direct guest IP; QEMU:
//! 127.0.0.1 hostfwd port). CDP needs no WebSocket client here — the simple
//! HTTP endpoints (/json/list, /json/new) plus the WS target URL are enough,
//! and we drive the WS with tokio-tungstenite (already in the workspace tree).
//!
//! Computer-use semantics follow the CDP browser automation domain:
//! navigate / screenshot (PNG) / click (Input.dispatchMouseEvent) /
//! type (Input.insertText) / key (Input.dispatchKeyEvent) / scroll / eval.

use std::time::Duration;

use serde_json::{json, Value};

use super::backend::{guest_ip_for, VmInstance, VmBackend};

/// Per-instance CDP endpoint.
fn cdp_endpoint(inst: &VmInstance) -> String {
    match inst.backend {
        VmBackend::Firecracker => format!("http://{}:9222", guest_ip_for(inst.slot)),
        VmBackend::Qemu => format!("http://127.0.0.1:{}", inst.cdp_port),
    }
}

fn ws_target_url(inst: &VmInstance, page_ws: &str) -> String {
    match inst.backend {
        VmBackend::Firecracker => {
            // rewrite guest IP to ws://host:port
            let ip = guest_ip_for(inst.slot);
            page_ws.replacen("localhost", &ip, 1).replacen("(192.168.\\d+\\.\\d+|10\\.0\\.2\\.15)", &ip, 1)
        }
        VmBackend::Qemu => {
            let port = inst.cdp_port;
            // Chrome reports ws://127.0.0.1:9222/... — hostfwd means host port differs
            page_ws.replacen(":9222/", &format!(":{port}/"), 1)
        }
    }
}

/// Low-level: HTTP GET on the CDP http endpoint.
///
/// Builds a client with `.no_proxy()`: the CDP endpoint is always a private
/// VM address (Firecracker guest IP or QEMU loopback hostfwd port). If the
/// host has HTTP_PROXY/HTTPS_PROXY set (e.g. a local v2rayA proxy), reqwest
/// would otherwise route the request through the proxy, which cannot reach the
/// private address and returns an empty body → "EOF while parsing".
async fn http_get(url: &str, timeout: Duration) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("CDP client: {e}"))?;
    let body = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("CDP http: {e}"))?
        .text()
        .await
        .map_err(|e| format!("CDP http body: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("CDP http json: {e}"))
}

/// Discover the first page target.
pub async fn page_target(inst: &VmInstance) -> Result<(String, String), String> {
    let list = http_get(
        &format!("{}/json/list", cdp_endpoint(inst)),
        Duration::from_secs(5),
    )
    .await?;
    let arr = list.as_array().ok_or("CDP /json/list returned non-array")?;
    for t in arr {
        if t.get("type").and_then(Value::as_str) == Some("page") {
            let id = t
                .get("id")
                .and_then(Value::as_str)
                .ok_or("target missing id")?
                .to_string();
            let ws = t
                .get("webSocketDebuggerUrl")
                .and_then(Value::as_str)
                .ok_or("target missing webSocketDebuggerUrl")?
                .to_string();
            return Ok((id, ws));
        }
    }
    Err("no page target in CDP /json/list (Chrome running?)".into())
}

/// Establish a WS connection to the page target.
async fn ws_connect(inst: &VmInstance) -> Result<WsConn, String> {
    let (_, ws_url) = page_target(inst).await?;
    let url = ws_target_url(inst, &ws_url);
    let (stream, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| format!("CDP ws connect: {e}"))?;
    Ok(WsConn {
        ws: stream,
        next_id: std::sync::atomic::AtomicI64::new(1),
    })
}

pub struct WsConn {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: std::sync::atomic::AtomicI64,
}

impl WsConn {
    /// Send a CDP command and await its matching response (single inflight).
    pub async fn send(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let msg = json!({ "id": id, "method": method, "params": params });
        use futures::{SinkExt, StreamExt};
        self.ws
            .send(tokio_tungstenite::tungstenite::Message::Text(msg.to_string()))
            .await
            .map_err(|e| format!("cdp send: {e}"))?;
        let deadline = Duration::from_secs(20);
        let timer = tokio::time::sleep(deadline);
        tokio::pin!(timer);
        loop {
            tokio::select! {
                _ = &mut timer => return Err("CDP response timeout (20s)".into()),
                msg = self.ws.next() => {
                    let msg = match msg {
                        Some(Ok(m)) => m,
                        Some(Err(e)) => return Err(format!("CDP ws error: {e}")),
                        None => return Err("CDP ws closed".into()),
                    };
                    let text = match msg {
                        tokio_tungstenite::tungstenite::Message::Text(t) => t,
                        tokio_tungstenite::tungstenite::Message::Ping(_) |
                        tokio_tungstenite::tungstenite::Message::Pong(_) => continue,
                        tokio_tungstenite::tungstenite::Message::Close(_) => return Err("CDP ws closed by remote".into()),
                        _ => continue,
                    };
                    let v: Value = serde_json::from_str(&text).map_err(|e| format!("cdp json: {e}"))?;
                    if v.get("id").and_then(Value::as_i64) == Some(id) {
                        if let Some(err) = v.get("error") {
                            return Err(format!("CDP error: {err}"));
                        }
                        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                    }
                    // events between our responses — ignore (single-inflight usage)
                }
            }
        }
    }
}

// ---- High-level Computer-Use actions ----

pub async fn navigate(inst: &VmInstance, url: &str) -> Result<Value, String> {
    let mut c = ws_connect(inst).await?;
    c.send(
        "Page.navigate",
        json!({ "url": url }),
    )
    .await
}

pub async fn screenshot(inst: &VmInstance, full_page: bool) -> Result<String, String> {
    let mut c = ws_connect(inst).await?;
    let _ = c.send("Page.enable", json!({})).await;
    if full_page {
        // Best-effort: capture beyond the fold via metrics emulation
        let m = c.send("Page.getLayoutMetrics", json!({})).await?;
        let h = m
            .pointer("/cssSize/height")
            .and_then(Value::as_f64)
            .unwrap_or(900.0) as u32;
        let _ = c
            .send(
                "Emulation.setDeviceMetricsOverride",
                json!({ "width": 1280, "height": h, "deviceScaleFactor": 1, "mobile": false }),
            )
            .await;
    }
    let shot = c
        .send(
            "Page.captureScreenshot",
            json!({ "format": "png", "captureBeyondViewport": full_page }),
        )
        .await?;
    shot.get("data")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| "screenshot missing data".into())
}

/// Click at viewport coordinates.
pub async fn click(inst: &VmInstance, x: i64, y: i64) -> Result<(), String> {
    let mut c = ws_connect(inst).await?;
    for (t, b) in [("mousePressed", "left"), ("mouseReleased", "left")] {
        c.send(
            "Input.dispatchMouseEvent",
            json!({ "type": t, "x": x, "y": y, "button": b, "clickCount": 1 }),
        )
        .await?;
    }
    Ok(())
}

/// Type text into the focused element.
pub async fn type_text(inst: &VmInstance, text: &str, submit: bool) -> Result<(), String> {
    let mut c = ws_connect(inst).await?;
    // Per-char key events keep site handlers (keydown filters) working.
    for ch in text.chars() {
        if ch == '\n' {
            c.send(
                "Input.dispatchKeyEvent",
                json!({ "type": "keyDown", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13 }),
            )
            .await?;
            c.send(
                "Input.dispatchKeyEvent",
                json!({ "type": "keyUp", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13 }),
            )
            .await?;
            continue;
        }
        c.send(
            "Input.dispatchKeyEvent",
            json!({ "type": "char", "text": ch.to_string() }),
        )
        .await?;
    }
    if submit {
        c.send(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyDown", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13 }),
        )
        .await?;
        c.send(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13 }),
        )
        .await?;
    }
    Ok(())
}

/// Scroll the page.
pub async fn scroll(inst: &VmInstance, x: i64, y: i64, dx: i64, dy: i64) -> Result<(), String> {
    let mut c = ws_connect(inst).await?;
    c.send(
        "Input.dispatchMouseEvent",
        json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy }),
    )
    .await?;
    Ok(())
}

/// Evaluate JS in the page, return the JSON value.
pub async fn eval(inst: &VmInstance, expr: &str) -> Result<Value, String> {
    let mut c = ws_connect(inst).await?;
    let r = c
        .send(
            "Runtime.evaluate",
            json!({ "expression": expr, "returnByValue": true, "awaitPromise": true }),
        )
        .await?;
    if let Some(err) = r.pointer("/exceptionDetails") {
        return Err(format!("eval exception: {err}"));
    }
    Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
}

/// Open a new tab (via HTTP endpoint — simpler than Target domain on WS).
pub async fn new_tab(inst: &VmInstance, url: &str) -> Result<Value, String> {
    http_get(
        &format!(
            "{}/json/new?{}",
            cdp_endpoint(inst),
            urlencode(url)
        ),
        Duration::from_secs(5),
    )
    .await
}

pub async fn list_tabs(inst: &VmInstance) -> Result<Value, String> {
    http_get(&format!("{}/json/list", cdp_endpoint(inst)), Duration::from_secs(5)).await
}

/// Minimal %-encoder for the /json/new query string.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
