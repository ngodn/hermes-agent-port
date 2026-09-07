//! Bot Mode profile discovery from tools/bot_mode_probe.py. Protocol text and
//! capability fingerprints consume this roster, including unmanaged profiles.
#![allow(dead_code)]

use serde_json::Value;
use std::path::{Path, PathBuf};

pub const BOT_CHAT_TITLE: &str = "Bot Chat";
pub const PROTOCOL_HEADING: &str = "## Messaging other agents";
pub const EPOCH_PREFIX: &str = "Capability epoch: ";

/// Recompute disk state each time. Config must come from the owning profile's
/// canonical loader; None means that loader failed, not an empty config.
pub fn capability_fingerprint(home: &Path, config: Option<&Value>) -> String {
    use sha2::{Digest, Sha256};
    let mut surface = serde_json::Map::new();
    let strings = |value: &Value| -> Option<Vec<String>> {
        if !crate::python_value::truthy(value) {
            return Some(vec![]);
        }
        match value {
            Value::Array(items) => Some(
                items
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| crate::python_value::python_repr(v))
                    })
                    .collect(),
            ),
            Value::Object(map) => Some(map.keys().cloned().collect()),
            Value::String(s) => Some(s.chars().map(|ch| ch.to_string()).collect()),
            _ => None,
        }
    };
    // Assign in Python order: failure after disabled_skills must retain that
    // field rather than discarding the partially captured config surface.
    let _ = (|| -> Option<()> {
        let config = config?;
        if crate::python_value::truthy(config) && !config.is_object() {
            return None;
        }
        let skills = config
            .get("skills")
            .filter(|v| v.is_object())
            .unwrap_or(&Value::Null);
        let tools = config
            .get("tools")
            .filter(|v| v.is_object())
            .unwrap_or(&Value::Null);
        let mut disabled: Vec<_> = strings(&skills["disabled"])?
            .into_iter()
            .map(|s| s.to_lowercase())
            .collect();
        disabled.sort();
        surface.insert("disabled_skills".into(), serde_json::json!(disabled));
        let mut enabled = strings(&tools["enabled_toolsets"])?;
        enabled.sort();
        surface.insert("enabled_toolsets".into(), serde_json::json!(enabled));
        let mcp = config
            .get("mcp_servers")
            .filter(|v| v.is_object())
            .map(crate::python_value::sorted_json)
            .unwrap_or_default();
        surface.insert("mcp".into(), Value::String(mcp));
        Some(())
    })();
    let soul = home.join("SOUL.md");
    let digest = if soul.is_file() {
        std::fs::read(soul)
            .ok()
            .map(|raw| format!("{:x}", Sha256::digest(raw)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    surface.insert("soul".into(), digest.into());
    let skills = home.join("skills");
    let mut names = Vec::new();
    let mut pending = if skills.is_dir() {
        vec![skills.clone()]
    } else {
        vec![]
    };
    while let Some(directory) = pending.pop() {
        if directory.join("SKILL.md").symlink_metadata().is_ok() {
            let relative = directory.strip_prefix(&skills).unwrap();
            names.push(if relative.as_os_str().is_empty() {
                ".".into()
            } else {
                relative.to_string_lossy().into_owned()
            });
        }
        if let Ok(entries) = std::fs::read_dir(directory) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    pending.push(entry.path());
                }
            }
        }
    }
    names.sort();
    surface.insert("skills".into(), serde_json::json!(names));
    let root = hermes_root(home);
    let mut managed: Vec<_> = roster(&root)
        .into_iter()
        .filter(|(_, path)| is_managed(path))
        .map(|(name, _)| name)
        .collect();
    managed.sort();
    surface.insert("roster".into(), serde_json::json!(managed));
    let mut roles: Vec<_> = roster(&root)
        .into_iter()
        .map(|(name, path)| format!("{name}:{}", profile_role(&path)))
        .collect();
    roles.sort();
    surface.insert("roster_roles".into(), serde_json::json!(roles));
    surface.insert("protocol_version".into(), 2.into());
    surface.insert("peers".into(), serde_json::json!(peers(&root)));
    let mut remote: Vec<_> = read_remote_roster(&root)
        .iter()
        .map(|row| {
            format!(
                "{}:{}:{}",
                row["connection_id"].as_str().unwrap(),
                row["profile"].as_str().unwrap(),
                row["title"].as_str().unwrap()
            )
        })
        .collect();
    remote.sort();
    surface.insert("remote_roster".into(), serde_json::json!(remote));
    let serialized = crate::python_value::sorted_json(&Value::Object(surface));
    format!("{:x}", Sha256::digest(serialized.as_bytes()))[..12].to_owned()
}

/// One cache per host runtime. Hold the lock through discovery so simultaneous
/// prompt builds cannot populate the same home from different roster snapshots.
#[derive(Default)]
pub struct ProtocolCache {
    entries: std::sync::Mutex<std::collections::HashMap<std::ffi::OsString, String>>,
}

impl ProtocolCache {
    pub fn section(&self, home: &Path, force_refresh: bool) -> String {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // OsString preserves spelling differences that Path equality normalizes.
        let key = home.as_os_str().to_owned();
        if force_refresh || !entries.contains_key(&key) {
            entries.insert(key.clone(), build_live_section(home));
        }
        entries[&key].clone()
    }

    /// Invoke only for a canonical Bot Chat session. Existing epoch text,
    /// even malformed, suppresses migration just as it does in Python.
    pub fn stored_prompt_needs_upgrade(&self, stored: &str, home: &Path) -> bool {
        !stored.contains(EPOCH_PREFIX)
            && !stored.contains(PROTOCOL_HEADING)
            && !self.section(home, false).is_empty()
    }
}

pub fn epoch_line(fingerprint: &str) -> String {
    format!("{EPOCH_PREFIX}{fingerprint}")
}

/// Compare the first stamp only. A failed fingerprint probe is represented by
/// None and cannot invalidate a cached prompt on every turn.
pub fn stored_prompt_capability_stale(stored: &str, current: Option<&str>) -> bool {
    static EPOCH: std::sync::OnceLock<fancy_regex::Regex> = std::sync::OnceLock::new();
    let pattern =
        EPOCH.get_or_init(|| fancy_regex::Regex::new(r"Capability epoch: ([0-9a-f]{12})").unwrap());
    let Ok(Some(captures)) = pattern.captures(stored) else {
        return false;
    };
    current
        .is_some_and(|value| value != "unavailable" && captures.get(1).unwrap().as_str() != value)
}

fn text(value: Option<&Value>) -> String {
    value
        .filter(|v| crate::python_value::truthy(v))
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| crate::python_value::python_repr(v))
        })
        .unwrap_or_default()
}

/// Normalize Desktop roster rows before exposing them in prompts. Preserve
/// only a real boolean liveness field, including explicit false.
pub fn normalize_remote_row(row: &Value) -> Option<Value> {
    let row = row.as_object()?;
    let trim = |key| {
        text(row.get(key))
            .trim_matches(crate::python_value::python_whitespace)
            .to_owned()
    };
    let profile = trim("profile");
    let connection = trim("connection_id");
    let raw_handle = trim("handle");
    let mut tag = raw_handle.trim_start_matches('@').to_owned();
    if tag.is_empty() {
        tag = handle(&profile).to_owned();
    }
    let valid = |s: &str| {
        !s.is_empty()
            && s.len() <= 64
            && s.as_bytes()[0].is_ascii_alphanumeric()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    if !valid(&profile) || !valid(&connection) || !valid(&tag) {
        return None;
    }
    let description = text(row.get("description"))
        .split(crate::python_value::python_whitespace)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect::<String>();
    let mut output = serde_json::json!({"profile":profile,"handle":tag,"connection_id":connection,
        "connection_label":trim("connection_label").chars().take(80).collect::<String>(),
        "title":trim("title").chars().take(120).collect::<String>(),"description":description});
    if let Some(value @ Value::Bool(_)) = row.get("online") {
        output["online"] = value.clone();
    }
    Some(output)
}

/// Reading does not sort or deduplicate. The Desktop writer normally does,
/// but hand-written and older roster files retain their original row order.
pub fn read_remote_roster(root: &Path) -> Vec<Value> {
    let Ok(raw) = std::fs::read_to_string(root.join("bot_relay/roster.json")) else {
        return vec![];
    };
    let Ok(data) = serde_json::from_str::<Value>(&raw) else {
        return vec![];
    };
    data.get("agents")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(normalize_remote_row).collect())
        .unwrap_or_default()
}

pub fn remote_paragraph(root: &Path) -> String {
    let rows = read_remote_roster(root);
    if rows.is_empty() {
        return String::new();
    }
    let mut counts = std::collections::HashMap::new();
    for row in &rows {
        *counts
            .entry(row["handle"].as_str().unwrap().to_lowercase())
            .or_insert(0) += 1;
    }
    let lines: Vec<_> = rows
        .iter()
        .map(|row| {
            let get = |key| row[key].as_str().unwrap();
            let tag = get("handle");
            let form = if counts[&tag.to_lowercase()] > 1 {
                format!("{tag}@{}", get("connection_id"))
            } else {
                tag.to_owned()
            };
            let location = if get("connection_label").is_empty() {
                get("connection_id")
            } else {
                get("connection_label")
            };
            let role = [get("title"), get("description")]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" \u{2014} ");
            let mut line = format!("- `@{form}` \u{2014} on {location}");
            if !role.is_empty() {
                line.push_str(&format!(" \u{2014} {role}"));
            }
            line
        })
        .collect();
    "\n\nTeammates on OTHER connected machines (reachable through the Desktop relay \u{2014} message them with message_agent exactly like local teammates; replies arrive as completion notifications the same way):\n".to_owned() + &lines.join("\n")
}

pub fn peers(root: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read(root.join("config.yaml")) else {
        return vec![];
    };
    let raw = String::from_utf8_lossy(&raw);
    if !raw.contains("bot_peers") {
        return vec![];
    }
    let Ok(data) = serde_yaml_ng::from_str::<Value>(&raw) else {
        return vec![];
    };
    let mut names: Vec<_> = data
        .get("bot_peers")
        .and_then(Value::as_object)
        .map(|peers| {
            peers
                .keys()
                .filter(|name| {
                    !name
                        .trim_matches(crate::python_value::python_whitespace)
                        .is_empty()
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

pub fn build_live_section(home: &Path) -> String {
    let root = hermes_root(home);
    build_section(home, &remote_paragraph(&root), &peers(&root))
}

pub fn hermes_root(home: &Path) -> PathBuf {
    if home
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "profiles")
    {
        home.parent()
            .and_then(Path::parent)
            .unwrap_or(Path::new("."))
            .to_owned()
    } else {
        home.to_owned()
    }
}

pub fn profile_name(home: &Path) -> String {
    if home
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "profiles")
    {
        home.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    } else {
        "default".into()
    }
}

pub fn handle(name: &str) -> &str {
    if name == "default" {
        "hermes"
    } else {
        name
    }
}

fn metadata(directory: &Path) -> Value {
    let path = directory.join("profile.yaml");
    if !path.is_file() {
        return Value::Null;
    }
    let Ok(raw) = std::fs::read(path) else {
        return Value::Null;
    };
    serde_yaml_ng::from_str(&String::from_utf8_lossy(&raw)).unwrap_or(Value::Null)
}

pub fn is_managed(directory: &Path) -> bool {
    metadata(directory)
        .get("ui_meta")
        .and_then(|meta| meta.get("hermes-bots"))
        .is_some_and(Value::is_object)
}

pub fn soul_has_protocol(directory: &Path) -> bool {
    let path = directory.join("SOUL.md");
    path.is_file()
        && std::fs::read(path)
            .is_ok_and(|raw| String::from_utf8_lossy(&raw).contains(PROTOCOL_HEADING))
}

/// Default first, then sorted directory entries. Hidden/unmanaged directories
/// remain teammates; management only gates whether this install uses Bot Mode.
pub fn roster(root: &Path) -> Vec<(String, PathBuf)> {
    let mut entries = vec![("default".into(), root.to_owned())];
    if let Ok(children) = std::fs::read_dir(root.join("profiles")) {
        let mut children: Vec<_> = children
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        children.sort();
        entries.extend(
            children
                .into_iter()
                .filter(|path| path.is_dir())
                .map(|path| {
                    (
                        path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        path,
                    )
                }),
        );
    }
    entries
}

pub fn is_bot_mode_managed(home: &Path) -> bool {
    roster(&hermes_root(home))
        .iter()
        .any(|(_, path)| is_managed(path))
}

pub fn profile_role(directory: &Path) -> String {
    let data = metadata(directory);
    let mut parts = Vec::new();
    for value in [
        data.get("ui_meta")
            .and_then(|m| m.get("hermes-bots"))
            .and_then(|b| b.get("title")),
        data.get("description"),
    ] {
        if let Some(value) = value.filter(|v| crate::python_value::truthy(v)) {
            let text = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| crate::python_value::python_repr(value));
            let text = text.trim_matches(crate::python_value::python_whitespace);
            if !text.is_empty() {
                parts.push(text.to_owned());
            }
        }
    }
    parts
        .join(" \u{2014} ")
        .split(crate::python_value::python_whitespace)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect()
}

pub fn roster_lines(root: &Path, me: &str) -> Vec<String> {
    roster(root)
        .into_iter()
        .filter(|(name, _)| name != me)
        .map(|(name, path)| {
            let role = profile_role(&path);
            let mut line = format!("- `@{}`", handle(&name));
            if !role.is_empty() {
                line.push_str(&format!(" \u{2014} {role}"));
            }
            line
        })
        .collect()
}

/// Render from explicit remote/peer snapshots. Remote discovery has its own
/// failure boundary upstream; local roster/SOUL reads remain profile-scoped.
pub fn build_section(home: &Path, remote_paragraph: &str, peers: &[String]) -> String {
    let root = hermes_root(home);
    if !is_bot_mode_managed(home) || soul_has_protocol(home) {
        return String::new();
    }
    let me = profile_name(home);
    let lines = roster_lines(&root, &me);
    let roster = if lines.is_empty() {
        "- (no teammates yet)".into()
    } else {
        lines.join("\n")
    };
    static TEMPLATE: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let template = TEMPLATE.get_or_init(|| {
        serde_json::from_str(include_str!("../../../tools/bot-protocol-template.json"))
            .expect("Python protocol template")
    });
    // Concatenate slots rather than substituting into already-inserted text:
    // profile names and roles may themselves contain placeholder-like strings.
    let mut section = format!(
        "{}{}{}{}{}",
        template[0],
        handle(&me),
        template[1],
        roster,
        template[2]
    );
    section.push_str(remote_paragraph);
    if !peers.is_empty() {
        let listed = peers
            .iter()
            .map(|peer| format!("`{peer}`"))
            .collect::<Vec<_>>()
            .join(", ");
        section.push_str(&format!("\n\nTeammates on OTHER machines: this install also has peer gateways registered ({listed}). Message an agent on a peer the same way \u{2014} message_agent with target \"<peer>/<agent-name>\" (or \"<peer>\" alone for the peer's main agent). Run `hermes peer list` for the live peer list."));
    }
    section
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_actual_python_filesystem_cases() {
        let base =
            std::env::temp_dir().join(format!("hermes-fingerprint-oracle-{}", std::process::id()));
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../tools/bot-fingerprint-goldens.json"))
                .unwrap();
        for (index, case) in cases.iter().enumerate() {
            let home = base.join(index.to_string());
            std::fs::create_dir_all(&home).unwrap();
            for (relative, contents) in case["files"].as_object().unwrap() {
                let path = home.join(relative);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, contents.as_str().unwrap()).unwrap();
            }
            let config = (!case["config"].is_null()).then_some(&case["config"]);
            assert_eq!(
                capability_fingerprint(&home, config),
                case["expected"].as_str().unwrap(),
                "case {index}: {case}"
            );
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn fingerprint_tracks_capability_changes_without_protocol_cache_refresh() {
        let root =
            std::env::temp_dir().join(format!("hermes-capability-state-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let config = serde_json::json!({});
        let initial = capability_fingerprint(&root, Some(&config));
        assert_eq!(initial, "8a34bbff4d61"); // Actual Python empty-home digest.
        assert_eq!(capability_fingerprint(&root, Some(&config)), initial);
        std::fs::write(root.join("SOUL.md"), "Persona").unwrap();
        let soul = capability_fingerprint(&root, Some(&config));
        assert_ne!(soul, initial);
        std::fs::create_dir_all(root.join("skills/nested/tool")).unwrap();
        std::fs::write(root.join("skills/nested/tool/SKILL.md"), "First text").unwrap();
        let skill = capability_fingerprint(&root, Some(&config));
        assert_ne!(skill, soul);
        std::fs::write(root.join("skills/nested/tool/SKILL.md"), "Changed text").unwrap();
        assert_eq!(capability_fingerprint(&root, Some(&config)), skill);
        assert!(stored_prompt_capability_stale(
            &epoch_line(&initial),
            Some(&skill)
        ));
        assert_ne!(capability_fingerprint(&root, None), skill);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn epoch_comparison_matches_python() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../tools/bot-epoch-goldens.json")).unwrap();
        for case in cases {
            assert_eq!(
                stored_prompt_capability_stale(
                    case["stored"].as_str().unwrap(),
                    case["current"].as_str()
                ),
                case["expected"].as_bool().unwrap(),
                "{case}"
            );
        }
    }

    #[test]
    fn protocol_cache_is_home_scoped_and_refreshes_only_when_requested() {
        let root = std::env::temp_dir().join(format!("hermes-bot-cache-{}", std::process::id()));
        let red = root.join("red");
        let blue = root.join("blue");
        std::fs::create_dir_all(&red).unwrap();
        std::fs::create_dir_all(&blue).unwrap();
        let cache = ProtocolCache::default();
        assert!(cache.section(&red, false).is_empty());
        for home in [&red, &blue] {
            std::fs::write(home.join("profile.yaml"), "ui_meta:\n  hermes-bots: {}\n").unwrap();
        }
        assert!(cache.section(&red, false).is_empty());
        assert!(!cache.section(&blue, false).is_empty());
        assert!(!cache.section(&red, true).is_empty());
        assert!(cache.stored_prompt_needs_upgrade("legacy", &red));
        assert!(!cache.stored_prompt_needs_upgrade("Capability epoch: broken", &red));
        assert!(!cache.stored_prompt_needs_upgrade(PROTOCOL_HEADING, &red));
        std::fs::write(red.join("SOUL.md"), PROTOCOL_HEADING).unwrap();
        assert!(!cache.section(&red, false).is_empty());
        assert!(cache.section(&red, true).is_empty());
        assert!(!cache.stored_prompt_needs_upgrade("legacy", &red));
        assert!(!cache.section(&blue, false).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remote_files_and_live_protocol_match_python() {
        let root = std::env::temp_dir().join(format!("hermes-bot-remote-{}", std::process::id()));
        std::fs::create_dir_all(root.join("bot_relay")).unwrap();
        std::fs::write(root.join("profile.yaml"), "ui_meta:\n  hermes-bots: {}\n").unwrap();
        std::fs::write(
            root.join("config.yaml"),
            "bot_peers:\n  beta: {}\n  alpha: {}\n",
        )
        .unwrap();
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../tools/bot-remote-goldens.json")).unwrap();
        for case in cases {
            std::fs::write(
                root.join("bot_relay/roster.json"),
                serde_json::json!({"agents":case["rows"]}).to_string(),
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(read_remote_roster(&root)).unwrap(),
                case["normalized"]
            );
            assert_eq!(
                build_live_section(&root),
                case["expected"].as_str().unwrap()
            );
        }
        std::fs::write(root.join("bot_relay/roster.json"), b"\xff").unwrap();
        assert!(read_remote_roster(&root).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn protocol_matches_python_with_real_profile_roster() {
        let root = std::env::temp_dir().join(format!("hermes-bot-protocol-{}", std::process::id()));
        let worker = root.join("profiles/worker");
        std::fs::create_dir_all(&worker).unwrap();
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../tools/bot-protocol-goldens.json")).unwrap();
        for case in cases {
            std::fs::write(
                worker.join("profile.yaml"),
                if case["managed"] == true {
                    "ui_meta:\n  hermes-bots: {}\ndescription: Worker role\n"
                } else {
                    "description: Worker role\n"
                },
            )
            .unwrap();
            let home = if case["name"] == "default" {
                &root
            } else {
                &worker
            };
            std::fs::write(
                home.join("SOUL.md"),
                if case["legacy"] == true {
                    PROTOCOL_HEADING
                } else {
                    "Persona"
                },
            )
            .unwrap();
            let peers: Vec<String> = serde_json::from_value(case["peers"].clone()).unwrap();
            assert_eq!(
                build_section(home, case["remote"].as_str().unwrap(), &peers),
                case["expected"].as_str().unwrap(),
                "{case}"
            );
            std::fs::remove_file(home.join("SOUL.md")).unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn roster_includes_unmanaged_teammates_and_legacy_soul_keeps_tool_gate() {
        let root = std::env::temp_dir().join(format!("hermes-bot-roster-{}", std::process::id()));
        let me = root.join("profiles/worker");
        std::fs::create_dir_all(&me).unwrap();
        std::fs::create_dir_all(root.join("profiles/.hidden")).unwrap();
        std::fs::write(root.join("profiles/not-a-profile"), "file").unwrap();
        assert!(!is_bot_mode_managed(&me));
        std::fs::write(
            me.join("profile.yaml"),
            "ui_meta:\n  hermes-bots:\n    title: 'Researcher'\ndescription: 'Checks facts'\n",
        )
        .unwrap();
        std::fs::write(me.join("SOUL.md"), PROTOCOL_HEADING).unwrap();
        assert!(is_bot_mode_managed(&me));
        assert!(soul_has_protocol(&me));
        assert_eq!(hermes_root(&me), root);
        assert_eq!(profile_name(&me), "worker");
        assert_eq!(profile_role(&me), "Researcher \u{2014} Checks facts");
        assert_eq!(
            roster_lines(&root, "worker"),
            vec!["- `@hermes`", "- `@.hidden`"]
        );
        assert!(roster_lines(&root, "default")[1].contains("@worker"));
        std::fs::write(me.join("profile.yaml"), "ui_meta: [broken\n").unwrap();
        assert!(!is_bot_mode_managed(&me));
        assert!(profile_role(&me).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
