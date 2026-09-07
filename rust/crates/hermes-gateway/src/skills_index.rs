//! Skills-index rendering from agent/prompt_builder.py. Entries are already
//! resolved and filtered by the owning profile's loader before this stage.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

/// Normalize and shorten index descriptions by Unicode characters, matching
/// skill_utils.extract_skill_description rather than truncating UTF-8 bytes.
pub fn extract_description(frontmatter: &serde_json::Value) -> String {
    let Some(value) = frontmatter
        .get("description")
        .filter(|v| crate::python_value::truthy(v))
    else {
        return String::new();
    };
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| crate::python_value::python_repr(value));
    let text = text
        .trim_matches(crate::python_value::python_whitespace)
        .trim_matches(['\'', '"']);
    if text.chars().count() > 60 {
        format!("{}...", text.chars().take(57).collect::<String>())
    } else {
        text.to_owned()
    }
}

pub fn config_string_list(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(text) => {
            let stripped = text.trim_matches(crate::python_value::python_whitespace);
            if stripped.starts_with('[') {
                if let Some(values) = crate::python_literal::list_strings(stripped) {
                    return values;
                }
            }
            vec![text.clone()]
        }
        serde_json::Value::Array(values) => values
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| crate::python_value::python_repr(v))
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Consume a raw owning-home config snapshot and explicitly captured platform
/// sources. Platform keys are exact and globally disabled names remain disabled.
pub fn disabled_names(
    config: &serde_json::Value,
    platform: Option<&str>,
    env_platform: Option<&str>,
    session_platform: Option<&str>,
) -> Result<BTreeSet<String>, &'static str> {
    disabled_names_raw(
        config,
        &serde_json::json!(platform),
        &serde_json::json!(env_platform),
        &serde_json::json!(session_platform),
    )
}

pub fn disabled_names_raw(
    config: &serde_json::Value,
    platform: &serde_json::Value,
    env_platform: &serde_json::Value,
    session_platform: &serde_json::Value,
) -> Result<BTreeSet<String>, &'static str> {
    if !crate::python_value::truthy(config) {
        return Ok(BTreeSet::new());
    }
    let config = config.as_object().ok_or("config must be an object")?;
    let Some(skills) = config.get("skills").and_then(serde_json::Value::as_object) else {
        return Ok(BTreeSet::new());
    };
    let names = |value: &serde_json::Value| -> BTreeSet<String> {
        config_string_list(value)
            .into_iter()
            .map(|name| {
                name.trim_matches(crate::python_value::python_whitespace)
                    .to_owned()
            })
            .filter(|name| !name.is_empty())
            .collect()
    };
    let mut disabled = names(skills.get("disabled").unwrap_or(&serde_json::Value::Null));
    let platform = [platform, env_platform, session_platform]
        .into_iter()
        .find(|p| crate::python_value::truthy(p));
    if let Some(platform) = platform {
        if platform.is_array() || platform.is_object() {
            return Err("platform is unhashable");
        }
        if let Some(per_platform) = skills
            .get("platform_disabled")
            .filter(|v| crate::python_value::truthy(v))
        {
            let per_platform = per_platform
                .as_object()
                .ok_or("platform_disabled must be an object")?;
            if let Some(value) = platform
                .as_str()
                .and_then(|platform| per_platform.get(platform))
            {
                disabled.extend(names(value));
            }
        }
    }
    disabled.remove("hermes-agent");
    Ok(disabled)
}

fn normalized_tag(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| crate::python_value::python_repr(value))
        .to_lowercase()
        .trim_matches(crate::python_value::python_whitespace)
        .to_owned()
}

/// Host compatibility is independent of the messaging platform. Termux's
/// Linux userland accepts Linux skills even when Python reports Android.
pub fn matches_platform(platforms: &serde_json::Value, host: &str, termux: bool) -> bool {
    if !crate::python_value::truthy(platforms) {
        return true;
    }
    let values = platforms
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| std::slice::from_ref(platforms));
    values.iter().any(|value| {
        let tag = normalized_tag(value);
        let mapped = match tag.as_str() {
            "macos" => "darwin",
            "windows" => "win32",
            tag => tag,
        };
        host.starts_with(mapped) || (termux && matches!(mapped, "linux" | "termux" | "android"))
    })
}

/// Offer-time relevance only. Explicit skill loads must bypass this filter.
/// Detection stays lazy and caller-scoped, since kanban ownership can differ
/// between a worker and a delegated task within the same process.
pub fn matches_environment(
    environments: &serde_json::Value,
    mut detect: impl FnMut(&str) -> bool,
) -> bool {
    if !crate::python_value::truthy(environments) {
        return true;
    }
    let values = environments
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| std::slice::from_ref(environments));
    for value in values {
        let tag = normalized_tag(value);
        if tag.is_empty() {
            continue;
        }
        if !matches!(tag.as_str(), "kanban" | "docker" | "s6") || detect(&tag) {
            return true;
        }
    }
    false
}

/// Keep malformed condition values intact. Python tolerates malformed parent
/// metadata but lets invalid condition iterables fail at the visibility gate.
pub fn extract_conditions(frontmatter: &serde_json::Value) -> serde_json::Value {
    let hermes = frontmatter
        .get("metadata")
        .and_then(serde_json::Value::as_object)
        .and_then(|metadata| metadata.get("hermes"))
        .and_then(serde_json::Value::as_object);
    serde_json::Value::Object(
        [
            "fallback_for_toolsets",
            "requires_toolsets",
            "fallback_for_tools",
            "requires_tools",
            "session_platforms",
        ]
        .into_iter()
        .map(|key| {
            (
                key.into(),
                hermes
                    .and_then(|h| h.get(key))
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!([])),
            )
        })
        .collect(),
    )
}

fn iterable(value: &serde_json::Value) -> Result<Vec<serde_json::Value>, &'static str> {
    match value {
        serde_json::Value::Array(values) => Ok(values.clone()),
        serde_json::Value::Object(values) => Ok(values
            .keys()
            .cloned()
            .map(serde_json::Value::String)
            .collect()),
        serde_json::Value::String(value) => Ok(value
            .chars()
            .map(|c| serde_json::Value::String(c.to_string()))
            .collect()),
        _ => Err("condition value is not iterable"),
    }
}

pub fn should_show(
    conditions: &serde_json::Value,
    tools: Option<&BTreeSet<String>>,
    toolsets: Option<&BTreeSet<String>>,
    platform: Option<&str>,
) -> Result<bool, &'static str> {
    let conditions = conditions
        .as_object()
        .ok_or("conditions must be an object")?;
    let empty = serde_json::json!([]);
    let platforms = conditions
        .get("session_platforms")
        .filter(|v| crate::python_value::truthy(v))
        .unwrap_or(&empty);
    let wanted: Vec<_> = iterable(platforms)?
        .iter()
        .map(|p| {
            p.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| crate::python_value::python_repr(p))
                .trim_matches(crate::python_value::python_whitespace)
                .to_lowercase()
        })
        .filter(|p| !p.is_empty())
        .collect();
    if let Some(platform) = platform.filter(|p| !p.is_empty()) {
        if !wanted.is_empty()
            && !wanted.iter().any(|p| {
                p == &platform
                    .trim_matches(crate::python_value::python_whitespace)
                    .to_lowercase()
            })
        {
            return Ok(false);
        }
    }
    if tools.is_none() && toolsets.is_none() {
        return Ok(true);
    }
    for (key, available, required) in [
        ("fallback_for_toolsets", toolsets, false),
        ("fallback_for_tools", tools, false),
        ("requires_toolsets", toolsets, true),
        ("requires_tools", tools, true),
    ] {
        for name in iterable(conditions.get(key).unwrap_or(&empty))? {
            if name.is_array() || name.is_object() {
                return Err("condition member is unhashable");
            }
            let present = name
                .as_str()
                .is_some_and(|name| available.is_some_and(|set| set.contains(name)));
            if present != required {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

#[derive(serde::Deserialize)]
struct Template {
    prefix: String,
    suffix: String,
    compact_note: String,
}

#[derive(Clone, Default, serde::Deserialize)]
pub struct Index {
    pub categories: BTreeMap<String, Vec<(String, String)>>,
    pub descriptions: BTreeMap<String, String>,
    pub compact: BTreeSet<String>,
    pub tools: Option<BTreeSet<String>>,
}

impl Index {
    pub fn render(&self) -> String {
        static TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
            serde_json::from_str(include_str!("../../../tools/skills-index-template.json"))
                .expect("generated Python skills template")
        });
        if self.categories.is_empty() {
            return String::new();
        }
        let mut lines = Vec::new();
        let mut has_compact = false;
        for (category, skills) in &self.categories {
            let top = category.split('/').next().unwrap();
            if self.compact.contains(top) {
                has_compact = true;
                let names: BTreeSet<_> = skills.iter().map(|(name, _)| name.as_str()).collect();
                lines.push(format!(
                    "  {category} [names only]: {}",
                    names.into_iter().collect::<Vec<_>>().join(", ")
                ));
                continue;
            }
            let description = self
                .descriptions
                .get(category)
                .map(String::as_str)
                .unwrap_or("");
            lines.push(if description.is_empty() {
                format!("  {category}:")
            } else {
                format!("  {category}: {description}")
            });
            // Stable sorting keeps the first description when the loader
            // supplies duplicate names. Sorting the complete tuple would not.
            let mut ordered: Vec<_> = skills.iter().collect();
            ordered.sort_by(|a, b| a.0.cmp(&b.0));
            let mut seen = BTreeSet::new();
            for (name, description) in ordered {
                if !seen.insert(name) {
                    continue;
                }
                lines.push(if description.is_empty() {
                    format!("    - {name}")
                } else {
                    format!("    - {name}: {description}")
                });
            }
        }
        let prefix = if self
            .tools
            .as_ref()
            .is_some_and(|tools| !tools.contains("web_search"))
        {
            TEMPLATE
                .prefix
                .replace("web_search or terminal", "terminal")
        } else {
            TEMPLATE.prefix.clone()
        };
        format!(
            "{prefix}{}{}{}",
            lines.join("\n"),
            TEMPLATE.suffix,
            if has_compact {
                TEMPLATE.compact_note.as_str()
            } else {
                ""
            }
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn descriptions_preserve_python_normalization_and_character_budget() {
        use serde_json::json;
        for (input, expected) in [
            (json!(null), "".to_owned()),
            (json!(false), "".to_owned()),
            (json!(true), "True".to_owned()),
            (json!(["word"]), "['word']".to_owned()),
            (json!(" \" inner \" "), " inner ".to_owned()),
            (json!("界".repeat(60)), "界".repeat(60)),
            (json!("界".repeat(61)), format!("{}...", "界".repeat(57))),
        ] {
            assert_eq!(
                super::extract_description(&json!({"description": input})),
                expected
            );
        }
    }
    use super::*;

    #[test]
    fn disabled_configuration_matches_python_fixtures() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../tools/disabled-skills-goldens.json"))
                .unwrap();
        for (number, case) in cases.iter().enumerate() {
            let env = match &case["env_platform"] {
                serde_json::Value::Null => serde_json::Value::Null,
                serde_json::Value::String(value) => serde_json::json!(value),
                value => serde_json::json!(crate::python_value::python_repr(value)),
            };
            let result = disabled_names_raw(
                &case["config"],
                &case["platform"],
                &env,
                &case["session_platform"],
            );
            let actual = match result {
                Ok(names) => serde_json::json!(names),
                Err(_) => serde_json::json!("error"),
            };
            assert_eq!(actual, case["expected"], "case {number}");
        }
    }

    #[test]
    fn disabled_names_merge_platform_lists_and_preserve_essential_skill() {
        let config = serde_json::json!({"skills":{"disabled":"[' global ', 'hermes-agent', 'Hermes-Agent']", "platform_disabled":{"telegram":["chat", "hermes-agent"], "cli":"['local']"}}});
        assert_eq!(
            disabled_names(&config, Some("telegram"), Some("cli"), None).unwrap(),
            ["Hermes-Agent".into(), "global".into(), "chat".into()].into()
        );
        assert_eq!(
            disabled_names(&config, Some(""), Some("cli"), None).unwrap(),
            ["Hermes-Agent".into(), "global".into(), "local".into()].into()
        );
        assert_eq!(
            disabled_names(&config, Some(" Telegram "), Some("cli"), None).unwrap(),
            ["Hermes-Agent".into(), "global".into()].into()
        );
        assert_eq!(config_string_list(&serde_json::json!("[true]")), ["[true]"]);
        assert!(disabled_names(
            &serde_json::json!({"skills":{"platform_disabled":"bad"}}),
            Some("cli"),
            None,
            None
        )
        .is_err());
    }

    #[test]
    fn offer_filters_match_python_including_lazy_environment_detection() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../tools/skill-offer-goldens.json")).unwrap();
        for (number, case) in cases.iter().enumerate() {
            assert_eq!(
                matches_platform(
                    &case["value"],
                    case["host"].as_str().unwrap(),
                    case["termux"].as_bool().unwrap()
                ),
                case["platform"].as_bool().unwrap(),
                "platform {number}"
            );
            let active: BTreeSet<String> = serde_json::from_value(case["active"].clone()).unwrap();
            let mut calls = Vec::new();
            let result = matches_environment(&case["value"], |tag| {
                calls.push(tag.to_owned());
                active.contains(tag)
            });
            assert_eq!(
                result,
                case["environment"].as_bool().unwrap(),
                "environment {number}"
            );
            assert_eq!(serde_json::json!(calls), case["calls"], "calls {number}");
        }
    }

    #[test]
    fn visibility_matches_actual_python_conditions_including_errors() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../tools/skill-conditions-goldens.json"))
                .unwrap();
        for (number, case) in cases.iter().enumerate() {
            let conditions = extract_conditions(&case["frontmatter"]);
            assert_eq!(conditions, case["conditions"], "extraction {number}");
            let tools: Option<BTreeSet<String>> =
                serde_json::from_value(case["tools"].clone()).unwrap();
            let toolsets: Option<BTreeSet<String>> =
                serde_json::from_value(case["toolsets"].clone()).unwrap();
            let result = should_show(
                &conditions,
                tools.as_ref(),
                toolsets.as_ref(),
                case["platform"].as_str(),
            );
            let actual = match result {
                Ok(value) => serde_json::json!(value),
                Err(_) => serde_json::json!("error"),
            };
            assert_eq!(actual, case["expected"], "case {number}");
        }
    }

    #[test]
    fn prompt_assembly_gates_skills_and_uses_the_session_tool_surface() {
        let index = Index {
            categories: [(
                "general".into(),
                vec![("workflow".into(), "instructions".into())],
            )]
            .into(),
            tools: Some(["web_search".into()].into()),
            ..Default::default()
        };
        for tool in ["skills_list", "skill_view", "skill_manage"] {
            let mut sections = crate::system_prompt::ResolvedPromptSections::default();
            sections.set_skills_index(&index, &[tool.into()]);
            let text = sections.skills.as_ref().unwrap();
            assert!(text.contains("basic tools like terminal."));
            assert!(!text.contains("web_search"));
            sections.set_skills_index(&index, &[tool.into(), "web_search".into()]);
            assert!(sections
                .skills
                .as_ref()
                .unwrap()
                .contains("web_search or terminal"));
            sections.set_skills_index(&index, &["terminal".into()]);
            assert!(sections.skills.is_none());
        }
    }

    #[test]
    fn rendered_index_matches_actual_python_cases() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../tools/skills-index-goldens.json")).unwrap();
        for (number, case) in cases.iter().enumerate() {
            let index: Index = serde_json::from_value(case.clone()).unwrap();
            assert_eq!(
                index.render(),
                case["expected"].as_str().unwrap(),
                "case {number}"
            );
        }
    }
}
