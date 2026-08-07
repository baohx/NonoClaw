//! Canonical session registration and revisioned peer synchronization.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use nonoclaw_engine::session::CumulativeUsageWire;
use nonoclaw_engine::{ResolvedConfig, Session, SessionService};
use tokio::sync::Mutex;

use super::protocol::{messages_loaded, send_msg_ok, Tx};

#[derive(Clone)]
pub(super) struct SessionHandle {
    pub session: Session,
}

pub(super) type SharedHandle = Arc<Mutex<Option<SessionHandle>>>;

struct SharedEntry {
    handle: SharedHandle,
    peers: Vec<Tx>,
}

#[derive(Default)]
pub(super) struct SessionHub {
    entries: Mutex<HashMap<String, SharedEntry>>,
    /// Fast in-memory cache of cumulative token usage per session.  Backed by
    /// durable storage: every write also calls `session.write_usage()` so the
    /// values survive server restarts (reloaded from the session JSONL on
    /// reconnect).
    cumulative_usages: Mutex<HashMap<String, CumulativeUsageWire>>,
}

impl SessionHub {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) async fn register_existing(
        &self,
        service: &SessionService,
        cwd: &Path,
        session_id: &str,
        tx: &Tx,
    ) {
        {
            let mut entries = self.entries.lock().await;
            if let Some(entry) = entries.get_mut(session_id) {
                if !entry.peers.iter().any(|peer| Arc::ptr_eq(peer, tx)) {
                    entry.peers.push(tx.clone());
                }
                return;
            }
        }
        // Session loading may touch disk, so it happens outside the hub lock.
        let handle = resume_session(service, cwd, session_id).ok();
        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.get_mut(session_id) {
            if !entry.peers.iter().any(|peer| Arc::ptr_eq(peer, tx)) {
                entry.peers.push(tx.clone());
            }
        } else {
            entries.insert(
                session_id.to_string(),
                SharedEntry {
                    handle: Arc::new(Mutex::new(handle)),
                    peers: vec![tx.clone()],
                },
            );
        }
    }

    pub(super) async fn handle(&self, session_id: &str) -> Option<SharedHandle> {
        self.entries
            .lock()
            .await
            .get(session_id)
            .map(|entry| Arc::clone(&entry.handle))
    }

    pub(super) async fn move_registration(
        &self,
        previous_session_id: Option<&str>,
        handle: &SessionHandle,
        tx: &Tx,
    ) {
        let next_session_id = handle.session.id();
        let mut entries = self.entries.lock().await;
        if let Some(previous) = previous_session_id.filter(|id| *id != next_session_id) {
            let remove = if let Some(entry) = entries.get_mut(previous) {
                entry.peers.retain(|peer| !Arc::ptr_eq(peer, tx));
                entry.peers.is_empty()
            } else {
                false
            };
            if remove {
                entries.remove(previous);
            }
        }
        if let Some(entry) = entries.get_mut(next_session_id) {
            if !entry.peers.iter().any(|peer| Arc::ptr_eq(peer, tx)) {
                entry.peers.push(tx.clone());
            }
        } else {
            entries.insert(
                next_session_id.to_string(),
                SharedEntry {
                    handle: Arc::new(Mutex::new(Some(handle.clone()))),
                    peers: vec![tx.clone()],
                },
            );
        }
    }

    pub(super) async fn sync(&self, session_id: &str, exclude: &Tx) {
        let (handle, peers) = {
            let entries = self.entries.lock().await;
            let Some(entry) = entries.get(session_id) else {
                return;
            };
            (Arc::clone(&entry.handle), entry.peers.clone())
        };
        let session = handle
            .lock()
            .await
            .as_ref()
            .map(|handle| handle.session.clone());
        let Some(session) = session else {
            return;
        };
        let Ok(snapshot) = session.snapshot().await else {
            return;
        };
        let message = messages_loaded(
            session_id,
            snapshot,
            self.cumulative_usage_json(session_id).await,
        );
        let mut dead = Vec::new();
        for peer in peers {
            if Arc::ptr_eq(&peer, exclude) {
                continue;
            }
            if !send_msg_ok(&peer, &message).await {
                dead.push(peer);
            }
        }
        if !dead.is_empty() {
            let mut entries = self.entries.lock().await;
            if let Some(entry) = entries.get_mut(session_id) {
                entry
                    .peers
                    .retain(|peer| !dead.iter().any(|closed| Arc::ptr_eq(peer, closed)));
            }
        }
    }

    pub(super) async fn peers(&self, session_id: &str) -> Vec<Tx> {
        self.entries
            .lock()
            .await
            .get(session_id)
            .map(|entry| entry.peers.clone())
            .unwrap_or_default()
    }

    pub(super) async fn remove_dead(&self, session_id: &str, dead: &[Tx]) {
        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.get_mut(session_id) {
            entry
                .peers
                .retain(|peer| !dead.iter().any(|closed| Arc::ptr_eq(peer, closed)));
        }
    }

    pub(super) async fn disconnect(&self, session_id: &str, tx: &Tx) {
        let mut entries = self.entries.lock().await;
        let remove = if let Some(entry) = entries.get_mut(session_id) {
            entry.peers.retain(|peer| !Arc::ptr_eq(peer, tx));
            entry.peers.is_empty()
        } else {
            false
        };
        if remove {
            entries.remove(session_id);
        }
    }

    /// Accumulate real API token usage for a session (called when a run completes).
    /// Updates the in-memory cache and persists to the session JSONL file so the
    /// value survives server restarts.
    pub(super) async fn accumulate_usage(
        &self,
        session_id: &str,
        usage: &nonoclaw_core::Usage,
    ) {
        // 1. Update in-memory cache.
        let wire = {
            let mut cum = self.cumulative_usages.lock().await;
            let entry = cum.entry(session_id.to_string()).or_insert_with(|| CumulativeUsageWire {
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            });
            entry.input_tokens += usage.input_tokens;
            entry.output_tokens += usage.output_tokens;
            entry.cache_creation_input_tokens += usage.cache_creation_input_tokens;
            entry.cache_read_input_tokens += usage.cache_read_input_tokens;
            entry.clone()
        };

        // 2. Persist to session file (best-effort – the in-memory cache is
        //    authoritative during this server session; the disk write ensures
        //    the next server session can recover from the snapshot).
        if let Some(handle) = self.handle(session_id).await {
            if let Some(session_handle) = handle.lock().await.as_ref() {
                if let Err(e) = session_handle.session.write_usage(&wire).await {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "failed to persist cumulative usage to session file"
                    );
                }
            }
        }
    }

    /// Read the cumulative usage for a session, serialized for the WS protocol.
    /// Tries the in-memory cache first, then falls back to the session snapshot
    /// (handles server restart without active clients).
    pub(super) async fn cumulative_usage_json(&self, session_id: &str) -> serde_json::Value {
        // Fast path: in-memory cache.
        {
            let cum = self.cumulative_usages.lock().await;
            if let Some(wire) = cum.get(session_id) {
                return serde_json::to_value(wire).unwrap_or_default();
            }
        }
        // Fallback: read from session snapshot (survives server restart).
        if let Some(handle) = self.handle(session_id).await {
            if let Some(session_handle) = handle.lock().await.as_ref() {
                if let Ok(snapshot) = session_handle.session.snapshot().await {
                    if let Some(cu) = &snapshot.cumulative_usage {
                        // Seed the in-memory cache for subsequent fast reads.
                        let mut cum = self.cumulative_usages.lock().await;
                        cum.entry(session_id.to_string())
                            .or_insert_with(|| cu.clone());
                        return serde_json::to_value(cu).unwrap_or_default();
                    }
                }
            }
        }
        serde_json::json!({})
    }
}

pub(super) fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

pub(super) fn create_new_session(
    service: &SessionService,
    cwd: &Path,
    config: &ResolvedConfig,
) -> Option<SessionHandle> {
    service
        .create(
            cwd,
            nonoclaw_engine::new_session_id(),
            config.active_model.value.clone(),
        )
        .map(|session| SessionHandle { session })
        .map_err(|error| tracing::warn!(%error, "failed to create session actor"))
        .ok()
}

pub(super) fn resume_session(
    service: &SessionService,
    cwd: &Path,
    id: &str,
) -> Result<SessionHandle, String> {
    if !valid_session_id(id) {
        return Err(format!("invalid session id: {id}"));
    }
    service
        .resume(cwd, id)
        .map(|session| SessionHandle { session })
        .map_err(|error| format!("load failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_reject_path_traversal() {
        assert!(valid_session_id("abc-123"));
        assert!(!valid_session_id("../abc"));
        assert!(!valid_session_id(""));
    }
}
