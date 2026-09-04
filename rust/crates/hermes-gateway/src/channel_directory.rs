//! Port of the read/resolve half of gateway/channel_directory.py.
//!
// Public API is ahead of its callers (the send_message tool wires it).
#![allow(dead_code)]
//!
//! Cached map of reachable channels/contacts per platform. The gateway rebuilds
//! it from live adapters on a timer and saves it to
//! `$HERMES_HOME/channel_directory.json`; the send_message tool reads it for
//! `action="list"` and to resolve human-friendly channel names to numeric IDs.
//!
//! This ports the READ side: loading the cached JSON, overlaying the
//! user-maintained friendly-name aliases (`channel_aliases.json`) on every read,
//! resolving a name to an id, and rendering the list for the model. The adapter-
//! driven BUILD side (`build_channel_directory`) lands with the adapter
//! subsystem.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

fn directory_path(over: Option<&Path>) -> PathBuf {
    over.map(|p| p.to_path_buf())
        .unwrap_or_else(|| crate::config_file::hermes_home().join("channel_directory.json"))
}

fn aliases_path(over: Option<&Path>) -> PathBuf {
    over.map(|p| p.to_path_buf())
        .unwrap_or_else(|| crate::config_file::hermes_home().join("channel_aliases.json"))
}

fn read_json_object(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// Load the friendly-name overlay: `{platform: {chat_id: friendly}}`.
fn load_channel_aliases(over: Option<&Path>) -> Map<String, Value> {
    read_json_object(&aliases_path(over)).unwrap_or_default()
}

/// Overlay friendly names onto directory entries by chat_id, mutating
/// `platforms`. Renames matching entries; injects a placeholder for an aliased
/// id that hasn't been discovered yet (so a fresh group is addressable by name
/// before its first message).
fn apply_channel_aliases(platforms: &mut Map<String, Value>, aliases_over: Option<&Path>) {
    let aliases = load_channel_aliases(aliases_over);
    for (plat_name, id_map) in aliases {
        let Value::Object(id_map) = id_map else {
            continue;
        };
        let entries = platforms
            .entry(plat_name)
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(entries) = entries else {
            continue;
        };
        for (chat_id, friendly) in id_map {
            let Some(friendly) = friendly.as_str().map(str::trim).filter(|f| !f.is_empty()) else {
                continue;
            };
            let mut matched = false;
            for e in entries.iter_mut() {
                if e.get("id").and_then(Value::as_str) == Some(chat_id.as_str()) {
                    if let Value::Object(obj) = e {
                        obj.insert("name".into(), json!(friendly));
                        matched = true;
                    }
                }
            }
            if !matched {
                entries.push(json!({
                    "id": chat_id,
                    "name": friendly,
                    "type": if chat_id.ends_with("@g.us") { "group" } else { "dm" },
                    "thread_id": Value::Null,
                }));
            }
        }
    }
}

fn normalize_channel_query(value: &str) -> String {
    value.trim_start_matches('#').trim().to_lowercase()
}

/// Human-facing target label for a channel entry.
fn channel_target_name(platform_name: &str, channel: &Value) -> String {
    let name = channel.get("name").and_then(Value::as_str).unwrap_or("");
    if platform_name == "discord"
        && channel
            .get("guild")
            .and_then(Value::as_str)
            .is_some_and(|g| !g.is_empty())
    {
        return format!("#{name}");
    }
    if platform_name != "discord" {
        if let Some(t) = channel
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            return format!("{name} ({t})");
        }
    }
    name.to_string()
}

/// Load the cached channel directory from disk, applying the alias overlay on
/// read so friendly names take effect immediately. Always returns an object
/// with `updated_at` and `platforms`.
pub fn load_directory(dir_over: Option<&Path>, aliases_over: Option<&Path>) -> Value {
    let mut data = read_json_object(&directory_path(dir_over)).unwrap_or_else(|| {
        let mut m = Map::new();
        m.insert("updated_at".into(), Value::Null);
        m.insert("platforms".into(), Value::Object(Map::new()));
        m
    });
    let platforms = data
        .entry("platforms")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(platforms) = platforms {
        apply_channel_aliases(platforms, aliases_over);
    }
    Value::Object(data)
}

fn platform_channels<'a>(directory: &'a Value, platform_name: &str) -> &'a [Value] {
    directory
        .get("platforms")
        .and_then(|p| p.get(platform_name))
        .and_then(Value::as_array)
        .map(|v| v.as_slice())
        .unwrap_or(&[])
}

/// The channel `type` string for `chat_id` (e.g. `channel`, `forum`), or `None`.
pub fn lookup_channel_type_in(
    directory: &Value,
    platform_name: &str,
    chat_id: &str,
) -> Option<String> {
    for ch in platform_channels(directory, platform_name) {
        if ch.get("id").and_then(Value::as_str) == Some(chat_id) {
            return ch.get("type").and_then(Value::as_str).map(str::to_string);
        }
    }
    None
}

pub fn lookup_channel_type(platform_name: &str, chat_id: &str) -> Option<String> {
    lookup_channel_type_in(&load_directory(None, None), platform_name, chat_id)
}

/// Resolve a human-friendly channel name to a numeric ID (case-insensitive,
/// first match wins). See the Python for the exact strategy.
pub fn resolve_channel_name_in(
    directory: &Value,
    platform_name: &str,
    name: &str,
) -> Option<String> {
    let channels = platform_channels(directory, platform_name);
    if channels.is_empty() {
        return None;
    }
    let id_of = |ch: &Value| ch.get("id").and_then(Value::as_str).map(str::to_string);

    // 0. Exact raw ID match (case-sensitive).
    let raw = name.trim();
    for ch in channels {
        if ch.get("id").and_then(Value::as_str) == Some(raw) {
            return id_of(ch);
        }
    }

    let query = normalize_channel_query(name);

    // 1. Exact name match (bare name or the display label).
    for ch in channels {
        let ch_name = ch.get("name").and_then(Value::as_str).unwrap_or("");
        if normalize_channel_query(ch_name) == query
            || normalize_channel_query(&channel_target_name(platform_name, ch)) == query
        {
            return id_of(ch);
        }
    }

    // 2. Guild-qualified match for Discord ("GuildName/channel").
    if let Some((guild_part, ch_part)) = query.rsplit_once('/') {
        for ch in channels {
            let guild = ch
                .get("guild")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_lowercase();
            let ch_name = ch.get("name").and_then(Value::as_str).unwrap_or("");
            if guild == guild_part && normalize_channel_query(ch_name) == ch_part {
                return id_of(ch);
            }
        }
    }

    // 3. Unambiguous prefix match.
    let matches: Vec<&Value> = channels
        .iter()
        .filter(|ch| {
            let ch_name = ch.get("name").and_then(Value::as_str).unwrap_or("");
            normalize_channel_query(ch_name).starts_with(&query)
        })
        .collect();
    if matches.len() == 1 {
        return id_of(matches[0]);
    }
    None
}

pub fn resolve_channel_name(platform_name: &str, name: &str) -> Option<String> {
    resolve_channel_name_in(&load_directory(None, None), platform_name, name)
}

/// Render the channel directory as a human-readable list for the model.
pub fn format_directory_for_display(directory: &Value) -> String {
    let platforms = match directory.get("platforms").and_then(Value::as_object) {
        Some(p) if !p.is_empty() => p,
        _ => return "No messaging platforms connected or no channels discovered yet.".to_string(),
    };

    let mut lines: Vec<String> = vec!["Available messaging targets:\n".to_string()];
    let mut plat_names: Vec<&String> = platforms.keys().collect();
    plat_names.sort();

    for plat_name in plat_names {
        let channels = platforms.get(plat_name).and_then(Value::as_array);
        let channels = match channels.filter(|c| !c.is_empty()) {
            None => {
                lines.push(format!("{}:", title_case(plat_name)));
                lines.push(format!(
                    "  (no channels discovered yet — send directly with {plat_name}:<chat_id>, \
                     or bare '{plat_name}' for the home channel)"
                ));
                lines.push(String::new());
                continue;
            }
            Some(c) => c,
        };

        if plat_name == "discord" {
            let mut guilds: std::collections::BTreeMap<String, Vec<&Value>> = Default::default();
            let mut dms: Vec<&Value> = Vec::new();
            for ch in channels {
                match ch
                    .get("guild")
                    .and_then(Value::as_str)
                    .filter(|g| !g.is_empty())
                {
                    Some(g) => guilds.entry(g.to_string()).or_default().push(ch),
                    None => dms.push(ch),
                }
            }
            for (guild_name, mut guild_channels) in guilds {
                lines.push(format!("Discord ({guild_name}):"));
                guild_channels.sort_by_key(|c| {
                    c.get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                });
                for ch in guild_channels {
                    lines.push(format!("  discord:{}", channel_target_name(plat_name, ch)));
                }
            }
            if !dms.is_empty() {
                lines.push("Discord (DMs):".to_string());
                for ch in dms {
                    lines.push(format!("  discord:{}", channel_target_name(plat_name, ch)));
                }
            }
            lines.push(String::new());
        } else {
            lines.push(format!("{}:", title_case(plat_name)));
            for ch in channels {
                lines.push(format!(
                    "  {plat_name}:{}",
                    channel_target_name(plat_name, ch)
                ));
            }
            lines.push(String::new());
        }
    }

    lines.push("Use these as the \"target\" parameter when sending.".to_string());
    lines.push("Bare platform name (e.g. \"telegram\") sends to home channel.".to_string());
    lines.join("\n")
}

/// Python `str.title()` for a single platform word (first letter upper).
fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_chandir_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(path: &Path, v: &Value) {
        std::fs::write(path, v.to_string()).unwrap();
    }

    fn sample_directory() -> Value {
        json!({
            "updated_at": "2026-09-04T00:00:00Z",
            "platforms": {
                "telegram": [
                    {"id": "111", "name": "Team Chat", "type": "group"},
                    {"id": "222", "name": "Alice", "type": "dm"}
                ],
                "discord": [
                    {"id": "d1", "name": "general", "guild": "MyGuild"},
                    {"id": "d2", "name": "random", "guild": "MyGuild"}
                ]
            }
        })
    }

    #[test]
    fn resolve_exact_id_and_name() {
        let dir = sample_directory();
        // Raw id.
        assert_eq!(
            resolve_channel_name_in(&dir, "telegram", "111").as_deref(),
            Some("111")
        );
        // Name (case-insensitive), including the display label "Alice (dm)".
        assert_eq!(
            resolve_channel_name_in(&dir, "telegram", "alice").as_deref(),
            Some("222")
        );
        assert_eq!(
            resolve_channel_name_in(&dir, "telegram", "Alice (dm)").as_deref(),
            Some("222")
        );
    }

    #[test]
    fn resolve_discord_guild_qualified_and_hash() {
        let dir = sample_directory();
        assert_eq!(
            resolve_channel_name_in(&dir, "discord", "#general").as_deref(),
            Some("d1")
        );
        assert_eq!(
            resolve_channel_name_in(&dir, "discord", "MyGuild/random").as_deref(),
            Some("d2")
        );
    }

    #[test]
    fn resolve_prefix_only_if_unambiguous() {
        let dir = sample_directory();
        // "gen" uniquely prefixes "general".
        assert_eq!(
            resolve_channel_name_in(&dir, "discord", "gen").as_deref(),
            Some("d1")
        );
        // "r" would prefix "random" only (general doesn't start with r) -> d2.
        assert_eq!(
            resolve_channel_name_in(&dir, "discord", "r").as_deref(),
            Some("d2")
        );
        // A nonexistent name -> None.
        assert_eq!(resolve_channel_name_in(&dir, "discord", "nope"), None);
    }

    #[test]
    fn lookup_type() {
        let dir = sample_directory();
        assert_eq!(
            lookup_channel_type_in(&dir, "telegram", "111").as_deref(),
            Some("group")
        );
        assert_eq!(lookup_channel_type_in(&dir, "telegram", "999"), None);
    }

    #[test]
    fn load_applies_aliases_and_injects_placeholder() {
        let home = temp_dir("alias");
        let dir_path = home.join("channel_directory.json");
        let alias_path = home.join("channel_aliases.json");
        write(&dir_path, &sample_directory());
        // Rename 111 -> "Standup" and add a not-yet-discovered group by name.
        write(
            &alias_path,
            &json!({"telegram": {"111": "Standup", "555@g.us": "Ops Room"}}),
        );
        let dir = load_directory(Some(&dir_path), Some(&alias_path));
        // Existing entry renamed.
        assert_eq!(
            resolve_channel_name_in(&dir, "telegram", "Standup").as_deref(),
            Some("111")
        );
        // Placeholder injected for the aliased-but-undiscovered group.
        assert_eq!(
            resolve_channel_name_in(&dir, "telegram", "Ops Room").as_deref(),
            Some("555@g.us")
        );
        assert_eq!(
            lookup_channel_type_in(&dir, "telegram", "555@g.us").as_deref(),
            Some("group")
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn missing_directory_is_empty_object() {
        let home = temp_dir("missing");
        let dir = load_directory(
            Some(&home.join("nope.json")),
            Some(&home.join("noalias.json")),
        );
        assert!(dir
            .get("platforms")
            .and_then(Value::as_object)
            .unwrap()
            .is_empty());
        assert_eq!(
            format_directory_for_display(&dir),
            "No messaging platforms connected or no channels discovered yet."
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn display_groups_discord_by_guild() {
        let out = format_directory_for_display(&sample_directory());
        assert!(out.contains("Discord (MyGuild):"));
        assert!(out.contains("discord:#general"));
        assert!(out.contains("Telegram:"));
        assert!(out.contains("telegram:Team Chat (group)"));
    }
}
