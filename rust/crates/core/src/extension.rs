//! Shared extension identity, provenance, precedence, and diagnostics.
//!
//! Skills, profiles, plugins, and MCP retain separate runtimes, but expose the
//! same descriptor contract to discovery callers and the Insight UI.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionKind {
    Skill,
    Profile,
    Plugin,
    Mcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionSourceKind {
    Bundled,
    User,
    Project,
    Plugin,
    Explicit,
    Dynamic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionStatus {
    Active,
    Pending,
    Shadowed,
    Failed,
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionDescriptor {
    pub kind: ExtensionKind,
    pub name: String,
    /// Human-readable path or stable source label. This must not contain secrets.
    pub source: String,
    pub source_kind: ExtensionSourceKind,
    /// Higher values win. Equal values are resolved by lexical source order.
    pub precedence: u32,
    pub version: Option<String>,
    pub status: ExtensionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadowed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ExtensionDescriptor {
    pub fn new(
        kind: ExtensionKind,
        name: impl Into<String>,
        source: impl Into<String>,
        source_kind: ExtensionSourceKind,
        precedence: u32,
    ) -> Self {
        Self {
            kind,
            name: name.into(),
            source: source.into(),
            source_kind,
            precedence,
            version: None,
            status: ExtensionStatus::Active,
            shadowed_by: None,
            detail: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionDiagnosticSeverity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionDiagnostic {
    pub severity: ExtensionDiagnosticSeverity,
    pub code: String,
    pub kind: ExtensionKind,
    pub name: Option<String>,
    pub source: Option<String>,
    pub message: String,
    pub suggestion: String,
}

/// Mark duplicate `(kind, name)` descriptors using deterministic precedence.
/// Returns every descriptor (including shadowed and failed entries) plus one
/// actionable diagnostic per shadowed candidate.
pub fn resolve_extension_conflicts(
    mut descriptors: Vec<ExtensionDescriptor>,
) -> (Vec<ExtensionDescriptor>, Vec<ExtensionDiagnostic>) {
    descriptors.sort_by(|a, b| {
        a.kind
            .as_str()
            .cmp(b.kind.as_str())
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| b.precedence.cmp(&a.precedence))
            .then_with(|| a.source.cmp(&b.source))
    });

    let mut diagnostics = Vec::new();
    let mut winner: Option<(ExtensionKind, String, String)> = None;
    for descriptor in &mut descriptors {
        if descriptor.status == ExtensionStatus::Failed {
            continue;
        }
        let same_name = winner
            .as_ref()
            .map(|(kind, name, _)| *kind == descriptor.kind && *name == descriptor.name)
            .unwrap_or(false);
        if same_name {
            let winner_source = winner.as_ref().expect("winner exists").2.clone();
            descriptor.status = ExtensionStatus::Shadowed;
            descriptor.shadowed_by = Some(winner_source.clone());
            diagnostics.push(ExtensionDiagnostic {
                severity: ExtensionDiagnosticSeverity::Warning,
                code: "extension_name_conflict".into(),
                kind: descriptor.kind,
                name: Some(descriptor.name.clone()),
                source: Some(descriptor.source.clone()),
                message: format!(
                    "{} `{}` from {} is shadowed by {}",
                    descriptor.kind.as_str(),
                    descriptor.name,
                    descriptor.source,
                    winner_source
                ),
                suggestion: "rename the extension or remove the lower-precedence copy".into(),
            });
        } else {
            winner = Some((
                descriptor.kind,
                descriptor.name.clone(),
                descriptor.source.clone(),
            ));
        }
    }
    (descriptors, diagnostics)
}

impl ExtensionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Profile => "profile",
            Self::Plugin => "plugin",
            Self::Mcp => "mcp",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_and_source_tie_break_are_deterministic_for_all_input_orders() {
        let candidates = [
            ExtensionDescriptor::new(
                ExtensionKind::Skill,
                "same",
                "project",
                ExtensionSourceKind::Project,
                100,
            ),
            ExtensionDescriptor::new(
                ExtensionKind::Skill,
                "same",
                "plugin",
                ExtensionSourceKind::Plugin,
                300,
            ),
            ExtensionDescriptor::new(
                ExtensionKind::Skill,
                "same",
                "user",
                ExtensionSourceKind::User,
                200,
            ),
        ];
        for order in [vec![0, 1, 2], vec![2, 0, 1], vec![1, 2, 0], vec![2, 1, 0]] {
            let input = order.into_iter().map(|i| candidates[i].clone()).collect();
            let (resolved, diagnostics) = resolve_extension_conflicts(input);
            let active = resolved
                .iter()
                .filter(|d| d.status == ExtensionStatus::Active)
                .collect::<Vec<_>>();
            assert_eq!(active.len(), 1);
            assert_eq!(active[0].source, "plugin");
            assert_eq!(diagnostics.len(), 2);
        }
    }

    #[test]
    fn failed_candidate_does_not_shadow_a_healthy_extension() {
        let mut failed = ExtensionDescriptor::new(
            ExtensionKind::Plugin,
            "x",
            "bad",
            ExtensionSourceKind::Project,
            500,
        );
        failed.status = ExtensionStatus::Failed;
        let healthy = ExtensionDescriptor::new(
            ExtensionKind::Plugin,
            "x",
            "good",
            ExtensionSourceKind::User,
            100,
        );
        let (resolved, _) = resolve_extension_conflicts(vec![failed, healthy]);
        assert!(resolved
            .iter()
            .any(|d| d.source == "good" && d.status == ExtensionStatus::Active));
    }
}
