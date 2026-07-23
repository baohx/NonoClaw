//! Canonical session registration and revisioned peer synchronization.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

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
        let message = messages_loaded(session_id, snapshot);
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
