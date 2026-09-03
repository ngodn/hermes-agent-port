//! Hermes config-file loading.
//!
// Public API is ahead of some callers while gateway subsystems are ported.
#![allow(dead_code)]
//!
//! A focused slice of `hermes_cli/config.py` + the `gateway/config.py` reader:
//! it resolves `$HERMES_HOME/config.yaml` and parses it into a
//! `serde_json::Value`, which is exactly the shape the ported resolvers
//! (`display_config`, `slash_access`) already consume. The rest of the Python
//! config layer (secret scope, profiles, plugins, auth, save/migrate) is
//! deferred until those subsystems are ported.
//!
//! Failure policy mirrors the Python fallback: a missing file or a parse error
//! yields an empty object (defaults), never a hard error, so config problems
//! degrade gracefully instead of taking the gateway down.

use std::path::PathBuf;

use serde_json::Value;
use tracing::warn;

/// Resolve the Hermes home directory. `HERMES_HOME` wins; otherwise the
/// per-platform default (`%LOCALAPPDATA%\hermes` on Windows, else `~/.hermes`).
pub fn hermes_home() -> PathBuf {
    if let Ok(val) = std::env::var("HERMES_HOME") {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    #[cfg(windows)]
    {
        if let Ok(val) = std::env::var("LOCALAPPDATA") {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return PathBuf::from(trimmed).join("hermes");
            }
        }
    }
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join(".hermes");
        }
    }
    PathBuf::from(".hermes")
}

/// The main config file path: `$HERMES_HOME/config.yaml` (matches
/// `hermes_cli.config.get_config_path`).
pub fn config_path() -> PathBuf {
    hermes_home().join("config.yaml")
}

/// Load and parse the user config into a JSON `Value`.
///
/// Returns an empty object when the file is absent or unreadable, and (after a
/// warning) when it fails to parse, so a broken config degrades to defaults
/// exactly like the Python loader's fallback path.
pub fn load_config() -> Value {
    load_config_from(&config_path())
}

/// Same as [`load_config`] but against an explicit path (used by tests).
pub fn load_config_from(path: &std::path::Path) -> Value {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return empty_object(),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "config: unreadable; using defaults");
            return empty_object();
        }
    };
    parse_config(&text, &path.display().to_string())
}

/// Parse YAML config text into a JSON `Value`. An empty document parses to an
/// empty object; a parse error warns and yields an empty object.
pub fn parse_config(text: &str, source: &str) -> Value {
    if text.trim().is_empty() {
        return empty_object();
    }
    match serde_yaml_ng::from_str::<Value>(text) {
        // A top-level scalar/sequence is not a valid config mapping; treat as none.
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(Value::Null) => empty_object(),
        Ok(_) => {
            warn!(source, "config: top-level is not a mapping; using defaults");
            empty_object()
        }
        Err(e) => {
            warn!(source, error = %e, "config: parse failed; using defaults");
            empty_object()
        }
    }
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_cfg_test_{}_{}_{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        p
    }

    #[test]
    fn parses_nested_yaml_into_json_value() {
        let yaml = "
display:
  tool_progress: all
  platforms:
    telegram:
      tool_progress: 'off'
gateway:
  strict: true
";
        let v = parse_config(yaml, "test");
        assert_eq!(v["display"]["tool_progress"], serde_json::json!("all"));
        assert_eq!(
            v["display"]["platforms"]["telegram"]["tool_progress"],
            serde_json::json!("off")
        );
        assert_eq!(v["gateway"]["strict"], serde_json::json!(true));
    }

    #[test]
    fn missing_file_is_empty_object() {
        let v = load_config_from(&temp_path("nope"));
        assert_eq!(v, empty_object());
    }

    #[test]
    fn empty_and_malformed_degrade_to_empty_object() {
        assert_eq!(parse_config("", "t"), empty_object());
        assert_eq!(parse_config("   \n  ", "t"), empty_object());
        // A bare scalar is not a config mapping.
        assert_eq!(parse_config("42", "t"), empty_object());
        // Broken YAML.
        assert_eq!(parse_config("a: [1, 2\nb: {", "t"), empty_object());
    }

    #[test]
    fn round_trips_through_a_real_file() {
        let path = temp_path("real");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(
                f,
                "gateway:\n  media_delivery_allow_dirs:\n    - /a\n    - /b"
            )
            .unwrap();
        }
        let v = load_config_from(&path);
        assert_eq!(
            v["gateway"]["media_delivery_allow_dirs"],
            serde_json::json!(["/a", "/b"])
        );
        let _ = std::fs::remove_file(&path);
    }
}
