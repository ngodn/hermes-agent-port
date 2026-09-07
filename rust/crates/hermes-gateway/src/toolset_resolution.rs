//! Composed toolset resolution from toolsets.py. A captured registry view is
//! immutable for one resolution pass; callers replace it after registry changes.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

#[derive(Default, serde::Deserialize)]
pub struct Toolset {
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub includes: Vec<String>,
}

#[derive(Default, serde::Deserialize)]
pub struct ToolsetResolver {
    #[serde(rename = "static")]
    pub definitions: BTreeMap<String, Toolset>,
    pub registry: Option<BTreeMap<String, Vec<String>>>,
    pub aliases: Vec<(String, String)>,
    pub platforms: BTreeSet<String>,
    pub core: Vec<String>,
}

impl ToolsetResolver {
    /// Match get_toolset_names: expose the first non-static alias for each
    /// plugin toolset. Alias iteration order is registration order.
    fn names(&self) -> BTreeSet<String> {
        let mut names: BTreeSet<_> = self.definitions.keys().cloned().collect();
        if let Some(registry) = &self.registry {
            for name in registry
                .keys()
                .filter(|name| !self.definitions.contains_key(*name))
            {
                let alias = self.aliases.iter().find(|(alias, target)| {
                    target == name && !self.definitions.contains_key(alias)
                });
                names.insert(alias.map(|(alias, _)| alias).unwrap_or(name).clone());
            }
        }
        names
    }

    fn walk(
        &self,
        name: &str,
        include_registry: bool,
        visited: &mut BTreeSet<String>,
    ) -> BTreeSet<String> {
        if matches!(name, "all" | "*") {
            return self
                .names()
                .iter()
                .flat_map(|name| self.walk(name, include_registry, &mut visited.clone()))
                .collect();
        }
        if !visited.insert(name.into()) {
            return BTreeSet::new();
        }
        let registry = self.registry.as_ref().filter(|_| include_registry);
        if let Some(definition) = self.definitions.get(name) {
            let mut tools: BTreeSet<_> = definition.tools.iter().cloned().collect();
            if let Some(registered) = registry.and_then(|r| r.get(name)) {
                tools.extend(registered.iter().cloned());
            }
            for included in &definition.includes {
                tools.extend(self.walk(included, include_registry, visited));
            }
            return tools;
        }
        if let Some(registry) = registry {
            if let Some(tools) = registry.get(name) {
                return tools.iter().cloned().collect();
            }
            if let Some((_, target)) = self.aliases.iter().find(|(alias, _)| alias == name) {
                return registry
                    .get(target)
                    .into_iter()
                    .flatten()
                    .cloned()
                    .collect();
            }
        }
        if include_registry {
            if let Some(platform) = name
                .strip_prefix("hermes-")
                .filter(|p| self.platforms.contains(*p))
            {
                let mut tools: BTreeSet<_> = self.core.iter().cloned().collect();
                tools.extend(
                    registry
                        .and_then(|r| r.get(platform))
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
                return tools;
            }
        }
        BTreeSet::new()
    }

    pub fn resolve(&self, name: &str, include_registry: bool) -> Vec<String> {
        self.walk(name, include_registry, &mut BTreeSet::new())
            .into_iter()
            .collect()
    }

    /// Shared policy for provider tool exposure and external-memory prompt
    /// sections. An explicit disabled memory toolset always takes precedence.
    pub fn memory_provider_enabled(
        &self,
        enabled: Option<&[String]>,
        disabled: &[String],
        memory_tool_present: bool,
    ) -> bool {
        if disabled.iter().any(|name| name == "memory") {
            return false;
        }
        if memory_tool_present {
            return true;
        }
        let Some(enabled) = enabled else {
            return true;
        };
        enabled.iter().any(|name| name == "memory")
            || enabled
                .iter()
                .any(|name| self.resolve(name, true).iter().any(|tool| tool == "memory"))
    }

    /// Inspect the captured tool schemas before applying the shared memory gate.
    /// Python raises on a present non-object `function`, but stops inspecting
    /// once it finds memory. Preserve both behaviors rather than hiding a bad
    /// schema behind an explicit toolset setting.
    pub fn memory_provider_exposed(
        &self,
        tools: &serde_json::Value,
        enabled: Option<&[String]>,
        disabled: &[String],
    ) -> Result<bool, &'static str> {
        let mut memory_present = false;
        if let Some(tools) = tools.as_array() {
            for tool in tools {
                let Some(tool) = tool.as_object() else {
                    continue;
                };
                let Some(function) = tool.get("function") else {
                    continue;
                };
                let function = function
                    .as_object()
                    .ok_or("tool function must be an object")?;
                if function.get("name").and_then(serde_json::Value::as_str) == Some("memory") {
                    memory_present = true;
                    break;
                }
            }
        }
        Ok(self.memory_provider_enabled(enabled, disabled, memory_present))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_actual_python_registry_fixtures() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../../tools/toolset-resolution-goldens.json"
        ))
        .unwrap();
        for (index, case) in cases.iter().enumerate() {
            let resolver: ToolsetResolver = serde_json::from_value(case.clone()).unwrap();
            let resolved = resolver.resolve(
                case["name"].as_str().unwrap(),
                case["include_registry"].as_bool().unwrap(),
            );
            assert_eq!(
                serde_json::json!(resolved),
                case["expected"],
                "case {index}"
            );
        }
    }

    #[test]
    fn memory_schema_exposure_preserves_short_circuit_and_error_order() {
        let resolver = ToolsetResolver::default();
        let memory = serde_json::json!({"function":{"name":"memory"}});
        let malformed = serde_json::json!({"function":null});
        assert_eq!(
            resolver.memory_provider_exposed(
                &serde_json::json!([null, {}, memory, malformed]),
                Some(&[]),
                &[]
            ),
            Ok(true)
        );
        assert!(resolver
            .memory_provider_exposed(
                &serde_json::json!([malformed, memory]),
                None,
                &["memory".into()]
            )
            .is_err());
        assert_eq!(
            resolver.memory_provider_exposed(
                &serde_json::json!([memory]),
                None,
                &["memory".into()]
            ),
            Ok(false)
        );
        assert_eq!(
            resolver.memory_provider_exposed(
                &serde_json::json!({"function":{"name":"memory"}}),
                Some(&[]),
                &[]
            ),
            Ok(false)
        );
    }

    #[test]
    fn composed_memory_gate_honors_overlays_cycles_aliases_and_disabled_precedence() {
        let resolver: ToolsetResolver = serde_json::from_value(serde_json::json!({
            "static":{"base":{"tools":["read_file"],"includes":["left","right"]},
                      "left":{"tools":[],"includes":["shared"]},"right":{"tools":[],"includes":["shared"]},
                      "shared":{"tools":["memory"],"includes":["base"]}},
            "registry":{"shared":["plugin_tool"],"mcp_docs":["search_docs"]},
            "aliases":[["docs","mcp_docs"]],"platforms":["custom"],"core":["terminal"]
        })).unwrap();
        assert_eq!(
            resolver.resolve("base", true),
            vec!["memory", "plugin_tool", "read_file"]
        );
        assert_eq!(resolver.resolve("base", false), vec!["memory", "read_file"]);
        assert_eq!(resolver.resolve("docs", true), vec!["search_docs"]);
        assert!(resolver.resolve("docs", false).is_empty());
        assert_eq!(resolver.resolve("hermes-custom", true), vec!["terminal"]);
        assert!(resolver.resolve("hermes-custom", false).is_empty());
        assert!(resolver.memory_provider_enabled(Some(&["base".into()]), &[], false));
        assert!(!resolver.memory_provider_enabled(
            Some(&["base".into()]),
            &["memory".into()],
            true
        ));
        assert!(!resolver.memory_provider_enabled(Some(&[]), &[], false));
        assert!(resolver.memory_provider_enabled(None, &[], false));
    }
}
