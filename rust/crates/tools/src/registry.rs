//! Tool registry. Mirrors `src/tools.ts` (registration) and the
//! `findToolByName` / `toolMatchesName` helpers in `src/Tool.ts`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::tool::{matches_name, Tool, ToolDefinition};
use nonoclaw_core::{ExtensionDescriptor, ExtensionDiagnostic};

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    extension_descriptors: Vec<ExtensionDescriptor>,
    extension_diagnostics: Vec<ExtensionDiagnostic>,
    /// Map from lowercased name/alias -> index into `tools`. Names are matched
    /// case-sensitively against the stored name; this index is a fast path.
    index: HashMap<String, usize>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let i = self.tools.len();
        if !tool.is_enabled() {
            return;
        }
        self.index.insert(tool.name().to_string(), i);
        for a in tool.aliases() {
            self.index.insert((*a).to_string(), i);
        }
        self.tools.push(tool);
    }

    pub fn find(&self, name: &str) -> Option<Arc<dyn Tool>> {
        // Case-sensitive name/alias first (mirrors TS lookup), then a
        // case-insensitive fallback for ergonomics.
        if let Some(&i) = self.index.get(name) {
            return Some(Arc::clone(&self.tools[i]));
        }
        for t in &self.tools {
            if matches_name(t.name(), t.aliases(), name) {
                return Some(Arc::clone(t));
            }
        }
        let lower = name.to_lowercase();
        if let Some(&i) = self.index.get(&lower) {
            return Some(Arc::clone(&self.tools[i]));
        }
        for t in &self.tools {
            if t.name().eq_ignore_ascii_case(name) {
                return Some(Arc::clone(t));
            }
        }
        None
    }

    pub fn all(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// Tool definitions to send to the API for this turn. Optionally filtered
    /// to a tool name allowlist.
    pub fn definitions(&self, allowlist: Option<&[String]>) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|t| match allowlist {
                None => true,
                Some(names) => names.iter().any(|n| {
                    matches_name(t.name(), t.aliases(), n)
                        || n.split('(')
                            .next()
                            .map(|p| t.name() == p.trim())
                            .unwrap_or(false)
                }),
            })
            .map(|t| t.definition())
            .collect()
    }

    /// Like `definitions()` but excludes deferred tools (where `should_defer()`
    /// returns true). These tools are discoverable via ToolSearch.
    pub fn active_definitions(&self, allowlist: Option<&[String]>) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|t| !t.should_defer())
            .filter(|t| match allowlist {
                None => true,
                Some(names) => names.iter().any(|n| {
                    matches_name(t.name(), t.aliases(), n)
                        || n.split('(')
                            .next()
                            .map(|p| t.name() == p.trim())
                            .unwrap_or(false)
                }),
            })
            .map(|t| t.definition())
            .collect()
    }

    /// Definitions for an explicit progressive-disclosure visibility set.
    /// Execution permissions remain independent and are still enforced by the
    /// PermissionGate.
    pub fn definitions_for_names(
        &self,
        visible: &HashSet<String>,
        allowlist: Option<&[String]>,
    ) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|tool| visible.contains(tool.name()))
            .filter(|tool| match allowlist {
                None => true,
                Some(names) => names.iter().any(|name| {
                    matches_name(tool.name(), tool.aliases(), name)
                        || name
                            .split('(')
                            .next()
                            .map(|prefix| tool.name() == prefix.trim())
                            .unwrap_or(false)
                }),
            })
            .map(|tool| tool.definition())
            .collect()
    }

    /// Build ToolSearch entries for all registered tools (including deferred).
    pub fn search_entries(&self) -> Vec<crate::builtin::tool_search::ToolSearchEntry> {
        self.tools
            .iter()
            .map(|t| crate::builtin::tool_search::ToolSearchEntry {
                name: t.name().to_string(),
                description: t.description().to_string(),
                search_hint: t.search_hint().unwrap_or("").to_string(),
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn add_extension_descriptor(&mut self, descriptor: ExtensionDescriptor) {
        self.extension_descriptors.push(descriptor);
    }

    pub fn add_extension_diagnostic(&mut self, diagnostic: ExtensionDiagnostic) {
        self.extension_diagnostics.push(diagnostic);
    }

    pub fn extension_descriptors(&self) -> &[ExtensionDescriptor] {
        &self.extension_descriptors
    }

    pub fn extension_diagnostics(&self) -> &[ExtensionDiagnostic] {
        &self.extension_diagnostics
    }

    /// Build a new registry containing the same tools except those whose name
    /// matches an entry in `exclude` (used to keep subagents from recursing).
    pub fn filtered(&self, exclude: &[&str]) -> ToolRegistry {
        let mut out = ToolRegistry::new();
        out.extension_descriptors = self.extension_descriptors.clone();
        out.extension_diagnostics = self.extension_diagnostics.clone();
        for t in &self.tools {
            if exclude.contains(&t.name()) {
                continue;
            }
            // Re-register via the public path so aliases are indexed too.
            out.register(Arc::clone(t));
        }
        out
    }

    /// Clone a registry while retaining only tools named by `include`.
    /// This is a visibility boundary, not a permission allowlist: retained
    /// tools still pass through the caller's unchanged PermissionGate.
    pub fn restricted_to(&self, include: &[String]) -> ToolRegistry {
        let mut out = ToolRegistry::new();
        out.extension_descriptors = self.extension_descriptors.clone();
        out.extension_diagnostics = self.extension_diagnostics.clone();
        for tool in &self.tools {
            let included = include.iter().any(|name| {
                matches_name(tool.name(), tool.aliases(), name)
                    || name
                        .split('(')
                        .next()
                        .map(|prefix| tool.name() == prefix.trim())
                        .unwrap_or(false)
            });
            if included {
                out.register(Arc::clone(tool));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolCtx;
    use nonoclaw_core::{PermissionResult, Result};
    use serde_json::json;

    struct DummyTool {
        name: &'static str,
        read_only: bool,
    }

    #[async_trait::async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn prompt(&self) -> &'static str {
            "dummy"
        }
        fn description(&self) -> &'static str {
            "dummy tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type":"object","properties":{}})
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            self.read_only
        }
        fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
            true
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolCtx<'_>,
        ) -> PermissionResult {
            PermissionResult::allow()
        }
        async fn call(
            &self,
            _: serde_json::Value,
            _: &ToolCtx<'_>,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<crate::tool::ToolResult> {
            Ok(crate::tool::ToolResult::ok("ok"))
        }
    }

    #[test]
    fn register_and_find_by_name_and_alias() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool {
            name: "Read",
            read_only: true,
        }));
        assert!(reg.find("Read").is_some());
        assert!(reg.find("Nope").is_none());
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn disabled_tools_not_registered() {
        struct Disabled;
        #[async_trait::async_trait]
        impl Tool for Disabled {
            fn name(&self) -> &'static str {
                "Disabled"
            }
            fn is_enabled(&self) -> bool {
                false
            }
            fn prompt(&self) -> &'static str {
                ""
            }
            fn description(&self) -> &'static str {
                ""
            }
            fn input_schema(&self) -> serde_json::Value {
                json!({})
            }
            fn is_read_only(&self, _: &serde_json::Value) -> bool {
                true
            }
            fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
                true
            }
            async fn check_permissions(
                &self,
                _: &serde_json::Value,
                _: &ToolCtx<'_>,
            ) -> PermissionResult {
                PermissionResult::allow()
            }
            async fn call(
                &self,
                _: serde_json::Value,
                _: &ToolCtx<'_>,
                _: tokio_util::sync::CancellationToken,
            ) -> Result<crate::tool::ToolResult> {
                Ok(crate::tool::ToolResult::ok(""))
            }
        }
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(Disabled));
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn definitions_for_names_only_advertises_visible_and_allowed_tools() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool {
            name: "Read",
            read_only: true,
        }));
        reg.register(Arc::new(DummyTool {
            name: "DeferredTool",
            read_only: true,
        }));

        let visible = HashSet::from(["DeferredTool".to_string()]);
        let definitions = reg.definitions_for_names(&visible, None);
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "DeferredTool");

        let allowlist = vec!["Read".to_string()];
        assert!(reg
            .definitions_for_names(&visible, Some(&allowlist))
            .is_empty());
    }
}
