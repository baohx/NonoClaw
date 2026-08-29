//! Sensitive-path detection: filesystem locations that routinely hold
//! credentials and should not be read into model context by the `Read` tool.
//!
//! The sandbox restricts *writes* to the workspace but allows reads of the
//! whole disk, so `.env`, `~/.ssh/id_rsa`, and cloud credential files would
//! otherwise flow straight into a tool result and (next turn) into the
//! provider body. The redaction layer in `engine` is the universal backstop;
//! this denylist is the first gate that refuses the read up front with a
//! clear error.

/// True when `path` names a location that typically holds secrets.
///
/// Conservative by design: false positives merely force the agent to use a
/// scrubbed alternative (env vars, `cat` via Bash, a non-secret copy), while
/// a false negative ships credentials to the provider.
pub fn is_sensitive_path(path: &std::path::Path) -> bool {
    let canon = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => {
            // Missing file: fall back to an absolute form so the check still
            // sees the requested location.
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()
                    .map(|c| c.join(path))
                    .unwrap_or_else(|_| path.to_path_buf())
            }
        }
    };

    let parts: Vec<String> = canon
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();

    // Credential-bearing directories (home-scoped or anywhere).
    const SENSITIVE_DIRS: &[&str] = &[
        ".ssh",
        ".aws",
        ".azure",
        ".gcloud",
        ".kube",
        ".docker",
        ".gnupg",
        ".password-store",
    ];
    for (i, part) in parts.iter().enumerate() {
        if SENSITIVE_DIRS.contains(&part.as_str()) {
            return true;
        }
        // `.config` alone is too broad; only the gcloud subdir is sensitive.
        if part == ".config" && parts.get(i + 1).map(|s| s == "gcloud").unwrap_or(false) {
            return true;
        }
    }

    let name = parts.last().map(String::as_str).unwrap_or("");
    let lower = name.to_lowercase();

    // Dotenv family: `.env`, `.env.local`, `.env.production`, …
    if lower.starts_with(".env") {
        return true;
    }

    // Exact credential file names.
    const SENSITIVE_FILES: &[&str] = &[
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        "credentials",
        "credentials.json",
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".pgpass",
        "dockerconfigjson",
    ];
    if SENSITIVE_FILES.contains(&lower.as_str()) {
        return true;
    }

    // Private-key material by extension.
    const SENSITIVE_EXT: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".jks", ".keystore"];
    if SENSITIVE_EXT.iter().any(|ext| lower.ends_with(ext)) {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(s)
    }

    #[test]
    fn home_ssh_and_cloud_locations_are_sensitive() {
        assert!(is_sensitive_path(&p("~/.ssh/id_rsa")));
        assert!(is_sensitive_path(&p("~/.ssh/id_ed25519")));
        assert!(is_sensitive_path(&p("~/.aws/credentials")));
        assert!(is_sensitive_path(&p("~/.azure/azureProfile.json")));
        assert!(is_sensitive_path(&p("~/.config/gcloud/credentials.json")));
        assert!(is_sensitive_path(&p("~/.kube/config")));
        assert!(is_sensitive_path(&p("/var/secrets/.env")));
        assert!(is_sensitive_path(&p("/etc/ssl/private/example.key")));
        assert!(is_sensitive_path(&p("/tmp/project/secret.pem")));
    }

    #[test]
    fn benign_paths_are_allowed() {
        assert!(!is_sensitive_path(&p("/etc/passwd")));
        assert!(!is_sensitive_path(&p("/home/user/NonoClaw/src/main.rs")));
        assert!(!is_sensitive_path(&p("/tmp/project/hello_world.py")));
        assert!(!is_sensitive_path(&p("/etc/hosts")));
        assert!(!is_sensitive_path(&p("/home/user/NonoClaw/README.md")));
        assert!(!is_sensitive_path(&p("/tmp/project/config.json")));
    }

    #[test]
    fn public_keys_and_config_dirs_are_not_blocked() {
        // `.pub` files are not secret but sit under `.ssh/`; the dir rule
        // conservatively blocks them too (harmless — the agent can still read
        // public keys via Bash). Generic user config dirs must NOT be blocked.
        assert!(!is_sensitive_path(&p("~/.config/Code/settings.json")));
        assert!(!is_sensitive_path(&p("~/.config/nvim/init.vim")));
        assert!(!is_sensitive_path(&p("/home/user/project/tls_test.pub")));
    }
}
