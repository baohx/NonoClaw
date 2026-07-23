//! Static frontend, PWA, and tunnel service.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub(super) fn frontend_dir(cwd: &Path) -> Option<PathBuf> {
    let exe_parent = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let mut candidates = vec![cwd.join("frontend/dist"), cwd.join("../frontend/dist")];
    if let Some(data) = std::env::var_os("NONOCLAW_DATA_DIR") {
        candidates.push(PathBuf::from(data).join("frontend/dist"));
    }
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        candidates.push(PathBuf::from(data).join("nonoclaw/frontend/dist"));
    }
    if let Some(home) = nonoclaw_core::home_dir() {
        candidates.push(home.join(".local/share/nonoclaw/frontend/dist"));
        candidates.push(home.join(".nonoclaw/frontend/dist"));
    }
    if let Some(directory) = exe_parent {
        candidates.push(directory.join("../../../frontend/dist"));
    }
    candidates.into_iter().find(|path| {
        let found = path.join("index.html").exists();
        if found {
            tracing::info!(path = %path.display(), "serving frontend");
        }
        found
    })
}

pub(super) async fn index(index_path: PathBuf) -> Response {
    match tokio::fs::read(index_path).await {
        Ok(content) => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/html; charset=utf-8")
            .body(Body::from(content))
            .expect("static response is valid"),
        Err(_) => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("index.html not found"))
            .expect("static response is valid"),
    }
}

pub(super) async fn serve_manifest() -> impl IntoResponse {
    let icon = "data:image/svg+xml;charset=utf-8,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 192 192'%3E%3Crect width='192' height='192' rx='38' fill='%23070a0f'/%3E%3Ctext x='96' y='124' font-family='serif' font-size='88' font-style='italic' fill='%235eead4' text-anchor='middle'%3ENC%3C/text%3E%3C/svg%3E";
    let body = serde_json::json!({
        "name": "NonoClaw",
        "short_name": "NonoClaw",
        "start_url": "/",
        "display": "standalone",
        "background_color": "#070a0f",
        "theme_color": "#5eead4",
        "icons": [
            { "src": icon, "sizes": "192x192", "type": "image/svg+xml" },
            { "src": icon, "sizes": "512x512", "type": "image/svg+xml" }
        ]
    })
    .to_string();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/manifest+json")
        .body(Body::from(body))
        .expect("static response is valid")
}

pub(super) async fn serve_sw() -> impl IntoResponse {
    let body = r#"const C="nc-v3";self.addEventListener("install",e=>{e.waitUntil(self.skipWaiting())});self.addEventListener("activate",e=>{e.waitUntil((async()=>{await self.clients.claim();const keys=await caches.keys();for(const k of keys){if(k!==C)await caches.delete(k)}})())});self.addEventListener("fetch",e=>{const u=new URL(e.request.url);if(u.pathname.startsWith("/assets/")){e.respondWith(caches.open(C).then(c=>c.match(e.request).then(r=>r||fetch(e.request).then(res=>{c.put(e.request,res.clone());return res}))))}else if(u.pathname==="/ws"){return}else{e.respondWith(fetch(e.request))}});"#;
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/javascript")
        .body(Body::from(body))
        .expect("static response is valid")
}

pub(super) async fn spawn_tunnel(local_addr: &str) -> Option<String> {
    let port = local_addr.rsplit(':').next()?;
    let target = format!("http://127.0.0.1:{port}");
    tracing::info!(%target, %local_addr, "spawning cloudflared tunnel");
    let mut child = match tokio::process::Command::new("cloudflared")
        .args(["tunnel", "--no-autoupdate", "--url", &target])
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%error, "cloudflared not found in PATH");
            return None;
        }
    };
    let stderr = child.stderr.take()?;
    let mut reader = tokio::io::BufReader::new(stderr);
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    let found = loop {
        line.clear();
        match tokio::time::timeout(
            std::time::Duration::from_secs(12),
            reader.read_line(&mut line),
        )
        .await
        {
            Ok(Ok(0)) | Ok(Err(_)) => break None,
            Err(_) => {
                tracing::warn!("timed out waiting for cloudflared URL");
                break None;
            }
            Ok(Ok(_)) => {
                if let Some(position) = line.find("https://") {
                    let url = line[position..]
                        .split_whitespace()
                        .next()
                        .unwrap_or(&line[position..])
                        .trim_end_matches(['.', '|', ' ']);
                    if url.contains("trycloudflare.com") {
                        break Some(url.to_string());
                    }
                }
            }
        }
    };
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut reader.into_inner(), &mut tokio::io::sink()).await;
        let _ = child.wait().await;
    });
    if found.is_some() {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    found
}
