//! Shared discovery adapters for extension kinds that do not own a runtime.
//! Runtime behavior remains in `skills`, `agents`, and `nonoclaw-tools::mcp`.

use std::path::Path;

use nonoclaw_core::{
    resolve_extension_conflicts, ExtensionDescriptor, ExtensionDiagnostic,
    ExtensionDiagnosticSeverity, ExtensionKind, ExtensionSourceKind, ExtensionStatus,
};

#[derive(Debug, Clone, Default)]
pub struct ExtensionDiscovery {
    pub descriptors: Vec<ExtensionDescriptor>,
    pub diagnostics: Vec<ExtensionDiagnostic>,
}

impl ExtensionDiscovery {
    pub fn merge(&mut self, other: Self) {
        self.descriptors.extend(other.descriptors);
        self.diagnostics.extend(other.diagnostics);
    }
}

/// Discover project and user profiles. The compatibility loader still accepts
/// project profiles by name; this report makes every source and parse failure
/// visible without making one bad profile fatal.
pub fn discover_profiles(cwd: &Path) -> ExtensionDiscovery {
    let mut roots = Vec::new();
    if let Some(home) = nonoclaw_core::nonoclaw_data_dir() {
        roots.push((home.join("agents"), ExtensionSourceKind::User, 100));
    }
    roots.push((
        cwd.join(".nonoclaw").join("agents"),
        ExtensionSourceKind::Project,
        200,
    ));

    let mut discovery = ExtensionDiscovery::default();
    for (root, source_kind, precedence) in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut paths = entries
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let fallback = path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("unnamed")
                .to_string();
            match std::fs::read_to_string(&path).ok().and_then(|raw| {
                let rest = raw.trim_start().strip_prefix("---")?;
                let end = rest.find("\n---")?;
                let value: serde_yaml::Value = serde_yaml::from_str(&rest[..end]).ok()?;
                Some(
                    value
                        .get("name")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| fallback.clone()),
                )
            }) {
                Some(name) => discovery.descriptors.push(ExtensionDescriptor::new(
                    ExtensionKind::Profile,
                    name,
                    path.display().to_string(),
                    source_kind,
                    precedence,
                )),
                None => {
                    let mut descriptor = ExtensionDescriptor::new(
                        ExtensionKind::Profile,
                        fallback.clone(),
                        path.display().to_string(),
                        source_kind,
                        precedence,
                    );
                    descriptor.status = ExtensionStatus::Failed;
                    descriptor.detail = Some("unreadable or invalid profile frontmatter".into());
                    discovery.descriptors.push(descriptor);
                    discovery.diagnostics.push(load_failure(
                        ExtensionKind::Profile,
                        fallback,
                        &path,
                    ));
                }
            }
        }
    }
    let (descriptors, conflicts) = resolve_extension_conflicts(discovery.descriptors);
    discovery.descriptors = descriptors;
    discovery.diagnostics.extend(conflicts);
    discovery
}

/// Discover installed plugin packages. Project installations override user
/// installations with the same package name; either can contribute Skills.
pub fn discover_plugins(cwd: &Path) -> ExtensionDiscovery {
    let mut roots = Vec::new();
    if let Some(home) = nonoclaw_core::nonoclaw_data_dir() {
        roots.push((home.join("plugins"), ExtensionSourceKind::User, 100));
    }
    roots.push((
        cwd.join(".nonoclaw").join("plugins"),
        ExtensionSourceKind::Project,
        200,
    ));
    let mut descriptors = Vec::new();
    for (root, source_kind, precedence) in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut entries = entries.flatten().collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            descriptors.push(ExtensionDescriptor::new(
                ExtensionKind::Plugin,
                name,
                path.display().to_string(),
                source_kind,
                precedence,
            ));
        }
    }
    let (descriptors, diagnostics) = resolve_extension_conflicts(descriptors);
    ExtensionDiscovery {
        descriptors,
        diagnostics,
    }
}

fn load_failure(kind: ExtensionKind, name: String, path: &Path) -> ExtensionDiagnostic {
    ExtensionDiagnostic {
        severity: ExtensionDiagnosticSeverity::Error,
        code: "extension_load_failed".into(),
        kind,
        name: Some(name),
        source: Some(path.display().to_string()),
        message: format!("failed to load {} from {}", kind.as_str(), path.display()),
        suggestion: "fix or remove this extension; unrelated extensions remain available".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_profile_is_isolated_from_healthy_profile() {
        let root = std::env::temp_dir().join(format!("nonoclaw-profiles-{}", uuid::Uuid::new_v4()));
        let dir = root.join(".nonoclaw/agents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("good.md"), "---\nname: good\n---\nbody").unwrap();
        std::fs::write(dir.join("bad.md"), "not frontmatter").unwrap();
        let report = discover_profiles(&root);
        assert!(report
            .descriptors
            .iter()
            .any(|d| d.name == "good" && d.status == ExtensionStatus::Active));
        assert!(report
            .descriptors
            .iter()
            .any(|d| d.name == "bad" && d.status == ExtensionStatus::Failed));
    }
}
