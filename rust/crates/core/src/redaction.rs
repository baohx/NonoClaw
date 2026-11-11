//! Credential redaction for content that is about to leave the machine toward
//! an LLM provider.
//!
//! NonoClaw keeps the provider API key in headers (never in the serialized
//! body), but credentials can still enter the body indirectly: the `Read` /
//! `Grep` / `Bash` tools can pull `.env`, `~/.ssh/id_rsa`, cloud credential
//! files, etc. into a tool result, which the next turn serializes into the
//! request. [`redact_credentials`] is the content-layer backstop that scrubs
//! credential-shaped material out of tool results before they leave the
//! machine, regardless of permission mode (bypass included).
//!
//! The rules are deliberately conservative: they target well-known secret
//! formats (PEM private keys, bearer/`sk-`/`ghp_`/JWT tokens, `KEY=value`
//! lines whose key name is a known secret name) and avoid suffix-matching on
//! arbitrary identifiers — see the `chars_per_token` over-redaction regression
//! that taught us generic key matching can silently corrupt non-secret data.

/// Fixed marker substituted for detected secret material.
pub const REDACTED: &str = "[REDACTED]";

/// Redact credential-shaped material from `text`. Returns `None` when nothing
/// changed (callers can skip rebuilding the message), `Some(clean)` otherwise.
pub fn redact_credentials_opt(text: &str) -> Option<String> {
    let mut changed = false;
    let mut working = text.to_string();

    // 1. PEM / OpenSSH private key blocks (whole block).
    let pem_re = regex::Regex::new(
        r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
    )
    .expect("pem regex is static");
    if pem_re.is_match(&working) {
        working = pem_re
            .replace_all(&working, "[REDACTED PRIVATE KEY]")
            .into_owned();
        changed = true;
    }

    // 2. `KEY=value` / `KEY: value` lines where the key is a known secret
    //    name (case-insensitive). Line-anchored, but tolerant of a leading
    //    cat -n style line number (Read emits `     3\tPASSWORD=...`) so
    //    tool results stay covered.
    let kv_re = regex::Regex::new(
        r"(?im)^([ \t]*(?:\d+[ \t]+)?(?:password|passwd|pwd|secret|client_secret|client_id|api[_-]?key|apikey|token|secret_key|access[_-]?token|refresh[_-]?token|auth[_-]?token|session[_-]?token|private[_-]?key|authorization)[ \t]*[=:][ \t]*)(\S[^\n]{0,400})",
    )
    .expect("kv regex is static");
    if kv_re.is_match(&working) {
        working = kv_re
            .replace_all(&working, |caps: &regex::Captures<'_>| {
                format!("{}{}", &caps[1], REDACTED)
            })
            .into_owned();
        changed = true;
    }

    // 3. Inline token formats (word-bounded, unambiguous prefixes).
    let token_re = regex::Regex::new(
        r"\b(sk-ant-[A-Za-z0-9_-]{8,}|sk-[A-Za-z0-9]{16,}|ghp_[A-Za-z0-9]{16,}|github_pat_[A-Za-z0-9_]{16,}|xox[baprs]-[A-Za-z0-9-]{8,}|AIza[0-9A-Za-z_-]{25,}|AKIA[0-9A-Z]{16}|ya29\.[A-Za-z0-9_-]{20,}|eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,})\b",
    )
    .expect("token regex is static");
    if token_re.is_match(&working) {
        working = token_re.replace_all(&working, REDACTED).into_owned();
        changed = true;
    }

    // 4. `Bearer <token>` (whole match replaced).
    let bearer_re = regex::Regex::new(r"\bBearer[ \t]+[A-Za-z0-9._~+/=-]{12,}")
        .expect("bearer regex is static");
    if bearer_re.is_match(&working) {
        working = bearer_re.replace_all(&working, REDACTED).into_owned();
        changed = true;
    }

    // 5. URL-embedded credentials `scheme://user:pass@host` -> keep user.
    let url_re = regex::Regex::new(r"([a-z][a-z0-9+.-]*://[^:\s/@]+):([^@\s/]+)@")
        .expect("url regex is static");
    if url_re.is_match(&working) {
        working = url_re
            .replace_all(&working, |caps: &regex::Captures<'_>| {
                format!("{}:[REDACTED]@", &caps[1])
            })
            .into_owned();
        changed = true;
    }

    if changed {
        Some(working)
    } else {
        None
    }
}

/// Redact credential-shaped material from `text` (always allocates).
pub fn redact_credentials(text: &str) -> String {
    redact_credentials_opt(text).unwrap_or_else(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pem_private_key_block_is_scrubbed() {
        let text = "Here is the key:\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA...\n-----END RSA PRIVATE KEY-----\ndone";
        let out = redact_credentials(text);
        assert!(!out.contains("PRIVATE KEY-----"));
        assert!(out.contains("[REDACTED PRIVATE KEY]"));
        assert!(out.starts_with("Here is the key:"));
        assert!(out.ends_with("done"));
    }

    #[test]
    fn openssh_private_key_is_scrubbed() {
        let text = "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----";
        let out = redact_credentials(text);
        assert!(out.contains("[REDACTED PRIVATE KEY]"));
        assert!(!out.contains("OPENSSH PRIVATE KEY-----"));
    }

    #[test]
    fn dotenv_style_secret_lines_are_scrubbed() {
        let text = "PORT=8080\nPASSWORD=hunter2\nTOKEN=abc123tokenvalue\nNORMAL_KEY=keepme";
        let out = redact_credentials(text);
        assert!(out.contains("PORT=8080"));
        assert!(out.contains("PASSWORD=[REDACTED]"));
        assert!(out.contains("TOKEN=[REDACTED]"));
        assert!(
            out.contains("NORMAL_KEY=keepme"),
            "non-secret keys untouched"
        );
        assert!(!out.contains("hunter2"));
        assert!(!out.contains("abc123tokenvalue"));
    }

    #[test]
    fn read_line_number_prefix_does_not_defeat_kv_rule() {
        // Read emits `     3\tPASSWORD=...`; the line-number prefix must not
        // hide secret lines from the kv rule (regression: found via E2E where
        // PASSWORD survived while API_KEY was caught by the token rule).
        let text = "     1\t# sample config\n     2\tSERVER=prod01\n     3\tPASSWORD=superSecretValue123\n     4\tAPI_KEY=sk-ant-api03-abcdEFGH1234xyz\n     5\tEND=tail\n";
        let out = redact_credentials(text);
        assert!(
            !out.contains("superSecretValue123"),
            "PASSWORD value leaked: {out}"
        );
        assert!(!out.contains("sk-ant-api03"), "API key leaked: {out}");
        assert!(
            out.contains("PASSWORD=[REDACTED]"),
            "PASSWORD line redacted: {out}"
        );
        assert!(out.contains("SERVER=prod01"), "benign lines survive: {out}");
        assert!(out.contains("END=tail"), "benign lines survive: {out}");
    }

    #[test]
    fn token_rule_preserves_key_prefix_when_kv_misses() {
        // A token on its own line gets fully replaced; a `KEY=token` line keeps
        // `KEY=` visible so the surrounding context stays readable.
        let out = redact_credentials("SECRET_VALUE=sk-ant-api03-abcdefghij1234567890");
        assert!(!out.contains("sk-ant-api03"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn json_style_and_prose_are_not_corrupted() {
        let text = r#"{"model":"x","max_tokens":1024,"password":"hunter2"}"#;
        // JSON on one line does not match the line-anchored kv rule; prose stays.
        let out = redact_credentials(text);
        assert_eq!(out, text);

        let prose = "The secret of life is 42. Bearer of good news arrived. password: the door";
        assert_eq!(redact_credentials(prose), prose);
    }

    #[test]
    fn well_known_token_prefixes_are_scrubbed() {
        let text = "key=sk-ant-api03-abcdEFGH1234xyz\nghp_abcdefghijklmnopqrstuvwxyz012345\nAIzaSyD2x-abcdefghijklmnopqrstuvwxyz123456";
        let out = redact_credentials(text);
        assert!(!out.contains("sk-ant"));
        assert!(!out.contains("ghp_"));
        assert!(!out.contains("AIza"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn bearer_token_is_scrubbed_but_short_words_survive() {
        let text = "Authorization: Bearer abcDEF1234567890xyz\nBearer short";
        let out = redact_credentials(text);
        assert!(out.contains("[REDACTED]"));
        assert!(
            out.contains("Bearer short"),
            "short token (prose) untouched"
        );
    }

    #[test]
    fn url_embedded_password_is_scrubbed_user_kept() {
        let text = "postgres://admin:s3cr3t@db.internal:5432/app";
        let out = redact_credentials(text);
        assert!(out.contains("postgres://admin:[REDACTED]@db.internal"));
        assert!(!out.contains("s3cr3t"));
    }

    #[test]
    fn unchanged_text_returns_none() {
        assert!(redact_credentials_opt("plain prose, no secrets here").is_none());
        assert!(redact_credentials_opt("turns: 12\nfiles: 3\n").is_none());
    }
}
