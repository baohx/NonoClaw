//! Canonical, credential-aware construction and caching for provider clients.
//!
//! Cache keys are private and never formatted or serialized. Public diagnostics
//! expose only purpose, model, base URL, and API format; credentials remain in
//! the server-side client and cache key.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use nonoclaw_core::Result;

use crate::{ApiFormat, Client};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientPurpose {
    Conversation,
    Compact,
    Document,
    Subagent,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ClientConfig {
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub auth_token: Option<String>,
    pub api_format: ApiFormat,
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("api_format", &self.api_format)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    base_url: String,
    api_key: Option<String>,
    auth_token: Option<String>,
    api_format: ApiFormat,
}

/// The sole constructor for configured provider clients.
///
/// `Client` is immutable after construction, so clients with identical
/// endpoint credentials and wire format are safe to share across models,
/// purposes, sessions, and concurrent runs.
pub struct ClientFactory {
    clients: Mutex<HashMap<CacheKey, Arc<Client>>>,
    http: Arc<reqwest::Client>,
}

impl fmt::Debug for ClientFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientFactory")
            .field("cached_clients", &self.clients.lock().unwrap().len())
            .finish_non_exhaustive()
    }
}

impl Default for ClientFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientFactory {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            http: Arc::new(
                reqwest::Client::builder()
                    .user_agent(concat!("nonoclaw/", env!("CARGO_PKG_VERSION")))
                    .connect_timeout(std::time::Duration::from_secs(30))
                    .timeout(std::time::Duration::from_secs(300))
                    .build()
                    .expect("build shared HTTP client"),
            ),
        }
    }

    pub fn client(&self, purpose: ClientPurpose, config: ClientConfig) -> Result<Arc<Client>> {
        let key = CacheKey {
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone(),
            auth_token: config.auth_token.clone(),
            api_format: config.api_format,
        };
        let mut clients = self.clients.lock().unwrap();
        if let Some(client) = clients.get(&key).cloned() {
            tracing::debug!(?purpose, model = %config.model, "reusing configured client");
            return Ok(client);
        }

        let client = Arc::new(
            Client::new(config.api_key, config.auth_token, config.base_url)?
                .with_format(config.api_format),
        );
        clients.insert(key, Arc::clone(&client));
        tracing::debug!(?purpose, model = %config.model, "created configured client");
        Ok(client)
    }

    /// Shared credential-neutral HTTP transport for provider-specific document
    /// endpoints (for example Mistral OCR) that do not use the Messages API.
    pub fn http_client(&self) -> Arc<reqwest::Client> {
        Arc::clone(&self.http)
    }

    #[cfg(test)]
    fn cache_len(&self) -> usize {
        self.clients.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(model: &str, key: &str) -> ClientConfig {
        ClientConfig {
            model: model.into(),
            base_url: "https://example.invalid".into(),
            api_key: Some(key.into()),
            auth_token: None,
            api_format: ApiFormat::Anthropic,
        }
    }

    #[test]
    fn reuses_credentials_across_models_and_purposes() {
        let factory = ClientFactory::new();
        let conversation = factory
            .client(ClientPurpose::Conversation, config("main", "secret"))
            .unwrap();
        let compact = factory
            .client(ClientPurpose::Compact, config("compact", "secret"))
            .unwrap();
        assert!(Arc::ptr_eq(&conversation, &compact));
        assert_eq!(factory.cache_len(), 1);
    }

    #[test]
    fn separates_different_credentials_and_redacts_debug() {
        let factory = ClientFactory::new();
        let first = config("main", "first-secret");
        let rendered = format!("{first:?}");
        assert!(!rendered.contains("first-secret"));
        let a = factory.client(ClientPurpose::Conversation, first).unwrap();
        let b = factory
            .client(ClientPurpose::Conversation, config("main", "second-secret"))
            .unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(factory.cache_len(), 2);
    }
}
