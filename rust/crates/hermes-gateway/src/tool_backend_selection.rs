//! Read the user's saved tool-provider intent without schema defaults.
//! A missing selection permits legacy discovery; an explicit selection must
//! retain its identity even when its credentials are unavailable.
#![allow(dead_code)]
use serde_json::Value;
use std::path::Path;

pub fn read(path: &Path, section: &str) -> Option<String> {
    from_raw(&crate::config_file::load_config_from(path), section)
}

pub fn from_raw(config: &Value, section: &str) -> Option<String> {
    let raw = config.get(section)?.as_object()?;
    let gateway = raw.get("use_gateway").is_some_and(|value| match value {
        Value::String(text) => matches!(
            text.trim_matches(crate::python_value::python_whitespace)
                .to_lowercase()
                .as_str(),
            "1" | "true" | "yes" | "on"
        ),
        value => crate::python_value::truthy(value),
    });
    if gateway {
        return Some("nous".into());
    }
    let keys: &[&str] = match section {
        "browser" => &["cloud_provider"],
        "web" => &["backend"],
        _ => &["provider", "backend", "cloud_provider"],
    };
    keys.iter()
        .filter_map(|key| raw.get(*key))
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| crate::python_value::python_repr(value))
        })
        .map(|text| {
            text.trim_matches(crate::python_value::python_whitespace)
                .to_lowercase()
        })
        .find(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    #[test]
    fn selection_matches_python_raw_settings() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/tool-selection-goldens.json"))
                .unwrap();
        for case in cases.as_array().unwrap() {
            assert_eq!(
                super::from_raw(&case["config"], case["section"].as_str().unwrap()).as_deref(),
                case["expected"].as_str(),
                "{case}"
            );
        }
    }

    #[test]
    fn file_reads_preserve_absence_and_explicit_local_selection() {
        let path = std::env::temp_dir().join(format!(
            "hermes-selection-{}-{}.yaml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert_eq!(super::read(&path, "stt"), None);
        for (yaml, expected) in [
            ("{}", None),
            ("stt:\n  provider: local\n", Some("local")),
            (
                "stt:\n  provider: openai\n  use_gateway: true\n",
                Some("nous"),
            ),
            ("stt: [broken", None),
        ] {
            std::fs::write(&path, yaml).unwrap();
            assert_eq!(super::read(&path, "stt").as_deref(), expected);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), yaml);
        }
        std::fs::remove_file(path).unwrap();
    }
}
