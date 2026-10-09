//! AWS SSO token management for Kiro (AWS CodeWhisperer) authentication.
//!
//! Reads cached credentials from `~/.aws/sso/cache/kiro-auth-token.json`,
//! refreshes expired access tokens via the OIDC refresh_token grant, and
//! persists updated tokens back to the cache file.
//!
//! Kiro uses AWS Builder ID (IAM Identity Center) OAuth. The refresh token
//! in `kiro-auth-token.json` is long-lived (≈90 days); the access token is
//! short-lived (≈1 hour). When the access token expires we call the OIDC
//! token endpoint with the refresh token to get a new pair.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const OIDC_TOKEN_URL: &str = "https://oidc.us-east-1.amazonaws.com/token";
const KIRO_AUTH_CACHE: &str = ".aws/sso/cache/kiro-auth-token.json";

// ── Credential cache types ──────────────────────────────────────────────────

/// On-disk shape of `kiro-auth-token.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroAuthCache {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
    #[serde(rename = "clientIdHash", skip_serializing_if = "Option::is_none")]
    pub client_id_hash: Option<String>,
    #[serde(rename = "authMethod", skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
    #[serde(rename = "provider", skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(rename = "region", skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

/// OIDC token endpoint response (successful refresh).
#[derive(Debug, Deserialize)]
struct OidcTokenResponse {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(rename = "expiresIn")]
    expires_in: Option<u64>,
}

/// OIDC token endpoint error body.
#[derive(Debug, Deserialize)]
struct OidcErrorBody {
    #[serde(rename = "error")]
    code: Option<String>,
    #[serde(rename = "error_description")]
    description: Option<String>,
}

/// In-memory token state with mutex-free refresh.
pub struct KiroTokenManager {
    cache_path: PathBuf,
    http: reqwest::Client,
}

impl KiroTokenManager {
    /// Create a manager that reads from the default `~/.aws/sso/cache/`
    /// location.
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let cache_path = PathBuf::from(home).join(KIRO_AUTH_CACHE);
        Self {
            cache_path,
            http: reqwest::Client::new(),
        }
    }

    /// Override the cache path (for tests or non-standard installs).
    pub fn with_cache_path(path: PathBuf) -> Self {
        Self {
            cache_path: path,
            http: reqwest::Client::new(),
        }
    }

    /// Return a valid access token, refreshing if expired or near expiry.
    ///
    /// Returns an error if the cache file is missing, malformed, or the
    /// refresh grant fails (e.g. refresh token revoked).
    pub async fn access_token(&self) -> Result<String, KiroAuthError> {
        let cache = self.read_cache()?;

        if self.is_expired(&cache) {
            tracing::debug!("kiro: access token expired, refreshing");
            self.refresh(cache).await
        } else {
            Ok(cache.access_token)
        }
    }

    fn read_cache(&self) -> Result<KiroAuthCache, KiroAuthError> {
        let raw = std::fs::read_to_string(&self.cache_path).map_err(|e| {
            KiroAuthError::CacheRead(format!("{}: {e}", self.cache_path.display()))
        })?;
        serde_json::from_str(&raw)
            .map_err(|e| KiroAuthError::CacheParse(format!("{}: {e}", self.cache_path.display())))
    }

    fn is_expired(&self, cache: &KiroAuthCache) -> bool {
        // Treat tokens within 60 s of their stated expiry as expired to
        // avoid a race where the token lapses mid-request.
        let Ok(expiry) = chrono_like_parse(&cache.expires_at) else {
            return true; // unparseable → assume expired
        };
        let now = unix_now();
        expiry <= now + 60
    }

    async fn refresh(&self, cache: KiroAuthCache) -> Result<String, KiroAuthError> {
        let body = serde_json::json!({
            "grantType": "refresh_token",
            "clientId": "kiro",
            "refreshToken": cache.refresh_token,
        });

        let resp = self
            .http
            .post(OIDC_TOKEN_URL)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| KiroAuthError::Refresh(format!("request failed: {e}")))?;

        let status = resp.status();
        let raw = resp
            .text()
            .await
            .map_err(|e| KiroAuthError::Refresh(format!("read body: {e}")))?;

        if !status.is_success() {
            if let Ok(err) = serde_json::from_str::<OidcErrorBody>(&raw) {
                let code = err.code.unwrap_or_default();
                let desc = err.description.unwrap_or_default();
                if code == "invalid_grant" {
                    return Err(KiroAuthError::RefreshTokenExpired);
                }
                return Err(KiroAuthError::Refresh(format!("{code}: {desc}")));
            }
            return Err(KiroAuthError::Refresh(format!("HTTP {status}: {raw}")));
        }

        let token_resp: OidcTokenResponse =
            serde_json::from_str(&raw)
                .map_err(|e| KiroAuthError::Refresh(format!("parse response: {e}")))?;

        // Build updated cache.  Use the new refresh token if provided,
        // otherwise keep the old one.
        let now = unix_now();
        let ttl = token_resp.expires_in.unwrap_or(3600);
        let new_expires = iso8601_from_unix(now + ttl as i64);

        let updated = KiroAuthCache {
            access_token: token_resp.access_token.clone(),
            refresh_token: token_resp
                .refresh_token
                .unwrap_or_else(|| cache.refresh_token.clone()),
            expires_at: new_expires,
            client_id_hash: cache.client_id_hash,
            auth_method: cache.auth_method,
            provider: cache.provider,
            region: cache.region,
        };

        // Persist updated tokens.
        if let Err(e) = self.write_cache(&updated) {
            tracing::warn!("kiro: failed to persist refreshed tokens: {e}");
        }

        Ok(token_resp.access_token)
    }

    fn write_cache(&self, cache: &KiroAuthCache) -> Result<(), KiroAuthError> {
        let json = serde_json::to_string_pretty(cache)
            .map_err(|e| KiroAuthError::CacheWrite(e.to_string()))?;
        std::fs::write(&self.cache_path, json)
            .map_err(|e| KiroAuthError::CacheWrite(format!("{}: {e}", self.cache_path.display())))
    }
}

impl Default for KiroTokenManager {
    fn default() -> Self {
        Self::new()
    }
}

// ── Error type ───────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum KiroAuthError {
    #[error("cannot read kiro auth cache: {0}")]
    CacheRead(String),

    #[error("cannot parse kiro auth cache: {0}")]
    CacheParse(String),

    #[error("cannot write kiro auth cache: {0}")]
    CacheWrite(String),

    #[error("token refresh failed: {0}")]
    Refresh(String),

    #[error("refresh token expired — re-authenticate by opening Kiro IDE")]
    RefreshTokenExpired,
}

impl KiroAuthError {
    /// Map to a user-actionable message.
    pub fn user_message(&self) -> String {
        match self {
            KiroAuthError::CacheRead(path) => {
                format!("Kiro auth cache not found at {path}. Open Kiro IDE once to log in.")
            }
            KiroAuthError::CacheParse(path) => {
                format!("Kiro auth cache at {path} is malformed. Delete it and re-open Kiro IDE.")
            }
            KiroAuthError::CacheWrite(msg) => {
                format!("Could not save refreshed Kiro tokens: {msg}")
            }
            KiroAuthError::Refresh(msg) => {
                format!("Failed to refresh Kiro access token: {msg}")
            }
            KiroAuthError::RefreshTokenExpired => {
                "Kiro refresh token has expired. Open Kiro IDE to re-authenticate.".to_string()
            }
        }
    }
}

// ── Time helpers (no external chrono dependency) ────────────────────────────

/// Parse an ISO 8601 / RFC 3339 timestamp like `2026-10-09T09:01:24.030Z`
/// into seconds since Unix epoch.
fn chrono_like_parse(s: &str) -> Result<i64, ()> {
    // Minimal RFC 3339 parser for the AWS SSO cache format.
    // Format: YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]
    let s = s.trim();
    if s.len() < 20 {
        return Err(());
    }

    let year: i64 = s[0..4].parse().map_err(|_| ())?;
    let month: i64 = s[5..7].parse().map_err(|_| ())?;
    let day: i64 = s[8..10].parse().map_err(|_| ())?;
    let hour: i64 = s[11..13].parse().map_err(|_| ())?;
    let min: i64 = s[14..16].parse().map_err(|_| ())?;
    let sec: i64 = s[17..19].parse().map_err(|_| ())?;

    // Days from civil epoch algorithm (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;

    Ok(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn iso8601_from_unix(secs: i64) -> String {
    // Inverse of chrono_like_parse.  Only needs to be accurate to the
    // second and in UTC — the AWS cache uses millisecond precision.
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;

    // civil_from_days
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!(
        "{year:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}.000Z"
    )
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iso8601_utc() {
        // 2026-10-09T09:01:24Z → verify round-trip
        let secs = chrono_like_parse("2026-10-09T09:01:24.030Z").unwrap();
        let back = iso8601_from_unix(secs);
        assert_eq!(back, "2026-10-09T09:01:24.000Z");
        // Just verify it parsed to something reasonable (year 2026)
        assert!(secs > 1_700_000_000);
    }

    #[test]
    fn parse_iso8601_no_millis() {
        let secs = chrono_like_parse("2026-01-01T00:00:00Z").unwrap();
        assert!(secs > 1_700_000_000);
    }

    #[test]
    fn iso8601_round_trip() {
        let now = unix_now();
        let iso = iso8601_from_unix(now);
        let parsed = chrono_like_parse(&iso).unwrap();
        assert_eq!(parsed, now);
    }

    #[test]
    fn expired_token_detection() {
        let mgr = KiroTokenManager::with_cache_path(PathBuf::from("/nonexistent"));
        let cache = KiroAuthCache {
            access_token: "x".into(),
            refresh_token: "y".into(),
            expires_at: "2020-01-01T00:00:00.000Z".into(),
            client_id_hash: None,
            auth_method: None,
            provider: None,
            region: None,
        };
        assert!(mgr.is_expired(&cache));

        let future = iso8601_from_unix(unix_now() + 3600);
        let cache2 = KiroAuthCache {
            access_token: "x".into(),
            refresh_token: "y".into(),
            expires_at: future,
            client_id_hash: None,
            auth_method: None,
            provider: None,
            region: None,
        };
        assert!(!mgr.is_expired(&cache2));
    }
}
