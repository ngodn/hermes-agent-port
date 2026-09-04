//! Port of gateway/status_phrases.py.
//!
// Public API is ahead of its callers (the status/streaming surface wires it).
#![allow(dead_code)]
//!
//! Human-friendly generic gateway status phrases. These turn Hermes' long-
//! running gateway status surface into short status lines suitable for chat,
//! without ever relaying raw model scratch text, tool args, or reasoning.
//!
//! Built-in defaults are embedded from `gateway/assets/status_phrases.yaml`.
//! Users can extend them with profile-relative catalogs under `HERMES_HOME`,
//! either at conventional paths (`status_phrases.yaml`, `status_phrases/*.yaml`)
//! or via `display.status_phrases.path` / `.paths`. Absolute paths and `..`
//! escapes are ignored on purpose so config stays profile-portable and never
//! reads arbitrary files.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Hermes UI surfaces (not app/vendor/domain buckets). Long-running-only:
/// regular tool/thinking chatter is intentionally not rewritten.
const STATUS_SURFACES: [&str; 2] = ["status", "generic"];
const MAX_CUSTOM_PHRASES_PER_SURFACE: usize = 80;
const MAX_PHRASE_CHARS: usize = 160;
const CONVENTIONAL_RELATIVE_PATHS: [&str; 2] = ["status_phrases.yaml", "status_phrases"];

/// The built-in catalog YAML, embedded at build time.
const BUILTIN_CATALOG_YAML: &str = include_str!("../assets/status_phrases.yaml");

type Catalog = HashMap<String, Vec<String>>;

fn fallback_catalog() -> Catalog {
    let mut c = Catalog::new();
    c.insert(
        "status".to_string(),
        vec![
            "still on it".to_string(),
            "still working through it".to_string(),
            "waiting for the result".to_string(),
        ],
    );
    c.insert(
        "generic".to_string(),
        vec![
            "on it".to_string(),
            "one sec".to_string(),
            "checking that now".to_string(),
        ],
    );
    c
}

/// Parse a YAML document into a `serde_json::Value` (uniform with config).
fn parse_yaml(text: &str) -> Option<Value> {
    serde_yaml_ng::from_str::<Value>(text).ok()
}

/// Clean a raw phrase list: cap count, trim, drop empties/overlong, dedupe
/// (order-preserving, first occurrence wins).
fn clean_phrase_list(value: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    let mut cleaned = Vec::new();
    let mut seen = HashSet::new();
    for item in items.iter().take(MAX_CUSTOM_PHRASES_PER_SURFACE) {
        // str(item or "") — only strings carry a phrase; other scalars in the
        // upstream would stringify, but real catalogs are string lists.
        let phrase = match item {
            Value::String(s) => s.trim().to_string(),
            _ => String::new(),
        };
        if phrase.is_empty() || phrase.chars().count() > MAX_PHRASE_CHARS || seen.contains(&phrase)
        {
            continue;
        }
        seen.insert(phrase.clone());
        cleaned.push(phrase);
    }
    cleaned
}

/// Merge one phrase-mapping section into `catalog`, honoring append/replace.
fn merge_phrase_mapping(catalog: &mut Catalog, section: &Value, inherited_mode: Option<&str>) {
    let mode = section
        .get("mode")
        .and_then(Value::as_str)
        .or(inherited_mode)
        .unwrap_or("append")
        .trim()
        .to_lowercase();
    let replace = mode == "replace";

    // phrases sub-map if present, else the section itself.
    let phrase_map = match section.get("phrases") {
        Some(v @ Value::Object(_)) => v,
        _ => section,
    };

    for surface in STATUS_SURFACES {
        let phrases = clean_phrase_list(phrase_map.get(surface));
        if phrases.is_empty() {
            continue;
        }
        if replace {
            catalog.insert(surface.to_string(), phrases);
        } else {
            catalog
                .entry(surface.to_string())
                .or_default()
                .extend(phrases);
        }
    }
}

fn merge_phrase_file(catalog: &mut Catalog, path: &Path, inherited_mode: Option<&str>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    if let Some(loaded @ Value::Object(_)) = parse_yaml(&text) {
        merge_phrase_mapping(catalog, &loaded, inherited_mode);
    }
}

/// Resolve a config path relative to `base_dir`, rejecting absolute paths and
/// `..` escapes so it can never read outside the profile.
fn relative_path_under(base_dir: &Path, raw_path: &Value) -> Option<PathBuf> {
    let raw = raw_path.as_str().unwrap_or("").trim();
    if raw.is_empty() {
        return None;
    }
    let candidate = expanduser(raw);
    if candidate.is_absolute() || candidate.components().any(is_parent_dir) {
        return None;
    }
    let base = base_dir
        .canonicalize()
        .unwrap_or_else(|_| base_dir.to_path_buf());
    let joined = base.join(&candidate);
    let resolved = joined.canonicalize().unwrap_or(joined);
    if resolved.starts_with(&base) {
        Some(resolved)
    } else {
        None
    }
}

fn is_parent_dir(c: std::path::Component<'_>) -> bool {
    matches!(c, std::path::Component::ParentDir)
}

/// Minimal `~` / `~/...` expansion (Python `Path.expanduser`). Anything else is
/// returned unchanged.
fn expanduser(raw: &str) -> PathBuf {
    if raw == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

/// The YAML files a resolved config path points at: the file itself if it is a
/// `.yaml`/`.yml`, else every such file directly inside a directory (sorted).
fn iter_phrase_files(path: &Path) -> Vec<PathBuf> {
    let is_yaml = |p: &Path| {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                let e = e.to_lowercase();
                e == "yaml" || e == "yml"
            })
            .unwrap_or(false)
    };
    if path.is_file() && is_yaml(path) {
        return vec![path.to_path_buf()];
    }
    if path.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && is_yaml(p))
            .collect();
        files.sort();
        return files;
    }
    Vec::new()
}

/// Merge every YAML file reachable from `paths` (one path or a list).
fn merge_phrase_paths(
    catalog: &mut Catalog,
    paths: Option<&Value>,
    base_dir: &Path,
    inherited_mode: Option<&str>,
) {
    let Some(paths) = paths else { return };
    let raw_paths: Vec<&Value> = match paths {
        Value::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    for raw_path in raw_paths {
        let Some(resolved) = relative_path_under(base_dir, raw_path) else {
            continue;
        };
        for phrase_file in iter_phrase_files(&resolved) {
            merge_phrase_file(catalog, &phrase_file, inherited_mode);
        }
    }
}

/// Built-in catalog: fallback phrases with the embedded asset merged in
/// (mode replace, so the asset defines the real defaults).
fn load_builtin_catalog() -> Catalog {
    let mut catalog = fallback_catalog();
    if let Some(loaded @ Value::Object(_)) = parse_yaml(BUILTIN_CATALOG_YAML) {
        merge_phrase_mapping(&mut catalog, &loaded, Some("replace"));
    }
    catalog
}

/// Merge one `display.status_phrases`-style section (inline mapping and/or
/// path/paths references) into `catalog`.
fn merge_phrase_config(catalog: &mut Catalog, section: Option<&Value>, base_dir: Option<&Path>) {
    let Some(section @ Value::Object(_)) = section else {
        return;
    };
    let mode = section
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("append")
        .trim()
        .to_lowercase();
    if let Some(base_dir) = base_dir {
        merge_phrase_paths(catalog, section.get("path"), base_dir, Some(&mode));
        merge_phrase_paths(catalog, section.get("paths"), base_dir, Some(&mode));
    }
    merge_phrase_mapping(catalog, section, None);
}

/// Resolve built-in + user-configured generic status phrases.
///
/// Order mirrors gateway display settings: built-ins, conventional profile-
/// relative user files, global `display.status_phrases` (or the legacy
/// `generic_status_phrases` alias), then
/// `display.platforms.<platform>.status_phrases`.
pub fn resolve_status_phrase_catalog(user_config: &Value, platform_key: Option<&str>) -> Catalog {
    let mut catalog = load_builtin_catalog();
    let hermes_home = crate::config_file::hermes_home();

    let conventional = Value::Array(
        CONVENTIONAL_RELATIVE_PATHS
            .iter()
            .map(|p| Value::String((*p).to_string()))
            .collect(),
    );
    merge_phrase_paths(&mut catalog, Some(&conventional), &hermes_home, None);

    let Some(display @ Value::Object(_)) = user_config.get("display") else {
        return catalog;
    };

    merge_phrase_config(
        &mut catalog,
        display.get("generic_status_phrases"),
        Some(&hermes_home),
    );
    merge_phrase_config(
        &mut catalog,
        display.get("status_phrases"),
        Some(&hermes_home),
    );

    if let (Some(platform_key), Some(Value::Object(platforms))) =
        (platform_key, display.get("platforms"))
    {
        if let Some(platform_display @ Value::Object(_)) = platforms.get(platform_key) {
            merge_phrase_config(
                &mut catalog,
                platform_display.get("generic_status_phrases"),
                Some(&hermes_home),
            );
            merge_phrase_config(
                &mut catalog,
                platform_display.get("status_phrases"),
                Some(&hermes_home),
            );
        }
    }
    catalog
}

/// Classify an internal gateway event into a Hermes UI-surface bucket.
pub fn classify_status_context(kind: &str) -> &'static str {
    match kind.trim().to_lowercase().as_str() {
        "heartbeat" | "waiting" | "long_running" | "status" => "status",
        _ => "generic",
    }
}

/// Choose an index into `candidates`. Abstracts the RNG so tests are
/// deterministic. `None` (default) picks uniformly at random.
pub trait PhrasePicker {
    fn pick(&mut self, len: usize) -> usize;
}

/// Default uniform random picker.
pub struct RandomPicker;

impl PhrasePicker for RandomPicker {
    fn pick(&mut self, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        // A small, dependency-free source of entropy: nanosecond clock.
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as usize)
            .unwrap_or(0);
        n % len
    }
}

/// Pick a short generic status phrase, avoiding recent repeats.
///
/// `recent` (most-recent-last, trimmed to the last 6) is updated in place when
/// provided. `catalog` defaults to the built-in catalog when `None`.
pub fn choose_status_phrase(
    kind: &str,
    recent: Option<&mut Vec<String>>,
    picker: &mut dyn PhrasePicker,
    catalog: Option<&Catalog>,
) -> String {
    let builtin;
    let phrase_catalog = match catalog {
        Some(c) => c,
        None => {
            builtin = load_builtin_catalog();
            &builtin
        }
    };
    let category = classify_status_context(kind);

    let generic_fallback = ["on it".to_string(), "one sec".to_string()];
    let mut candidates: Vec<String> = phrase_catalog
        .get(category)
        .or_else(|| phrase_catalog.get("generic"))
        .cloned()
        .unwrap_or_else(|| generic_fallback.to_vec());

    if let Some(recent) = recent.as_ref() {
        if !recent.is_empty() {
            let recent_set: HashSet<&String> = recent.iter().collect();
            let fresh: Vec<String> = candidates
                .iter()
                .filter(|p| !recent_set.contains(*p))
                .cloned()
                .collect();
            if !fresh.is_empty() {
                candidates = fresh;
            }
        }
    }

    if candidates.is_empty() {
        return String::new();
    }
    let idx = picker.pick(candidates.len());
    let phrase = candidates[idx.min(candidates.len() - 1)].clone();

    if let Some(recent) = recent {
        recent.push(phrase.clone());
        // Keep only the last 6 (Python `del recent[:-6]`).
        if recent.len() > 6 {
            let drop = recent.len() - 6;
            recent.drain(0..drop);
        }
    }
    phrase
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic picker: always returns a fixed index (clamped).
    struct FixedPicker(usize);
    impl PhrasePicker for FixedPicker {
        fn pick(&mut self, len: usize) -> usize {
            if len == 0 {
                0
            } else {
                self.0 % len
            }
        }
    }

    #[test]
    fn builtin_catalog_loads_from_asset() {
        let c = load_builtin_catalog();
        // The asset replaces the fallbacks with its richer lists.
        assert!(c.get("status").unwrap().len() > 5);
        assert!(c.get("generic").unwrap().len() > 5);
        assert!(c.get("status").unwrap().iter().any(|p| p == "still on it"));
    }

    #[test]
    fn classify_buckets() {
        assert_eq!(classify_status_context("heartbeat"), "status");
        assert_eq!(classify_status_context(" Long_Running "), "status");
        assert_eq!(classify_status_context("tool"), "generic");
        assert_eq!(classify_status_context(""), "generic");
    }

    #[test]
    fn clean_phrase_list_trims_dedupes_caps() {
        let v = serde_json::json!(["  a ", "a", "", "b", "b", "c"]);
        assert_eq!(clean_phrase_list(Some(&v)), vec!["a", "b", "c"]);
        // Overlong phrase dropped.
        let long = "x".repeat(MAX_PHRASE_CHARS + 1);
        let v2 = serde_json::json!([long, "ok"]);
        assert_eq!(clean_phrase_list(Some(&v2)), vec!["ok"]);
    }

    #[test]
    fn merge_mapping_append_and_replace() {
        let mut c = fallback_catalog();
        let base_status = c.get("status").unwrap().len();
        // Append adds.
        let section = serde_json::json!({"status": ["extra one"]});
        merge_phrase_mapping(&mut c, &section, None);
        assert_eq!(c.get("status").unwrap().len(), base_status + 1);
        // Replace swaps the whole surface.
        let section2 = serde_json::json!({"mode": "replace", "status": ["only this"]});
        merge_phrase_mapping(&mut c, &section2, None);
        assert_eq!(c.get("status").unwrap(), &vec!["only this".to_string()]);
    }

    #[test]
    fn choose_avoids_recent_repeats() {
        let mut catalog = Catalog::new();
        catalog.insert(
            "generic".to_string(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
        );
        let mut recent = vec!["a".to_string()];
        // FixedPicker(0) would pick "a", but "a" is recent, so candidates become
        // [b, c] and index 0 -> "b".
        let mut p = FixedPicker(0);
        let phrase = choose_status_phrase("tool", Some(&mut recent), &mut p, Some(&catalog));
        assert_eq!(phrase, "b");
        assert_eq!(recent, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn choose_trims_recent_to_six() {
        let mut catalog = Catalog::new();
        catalog.insert(
            "generic".to_string(),
            (0..20).map(|i| format!("p{i}")).collect(),
        );
        let mut recent: Vec<String> = (0..6).map(|i| format!("r{i}")).collect();
        let mut p = FixedPicker(10);
        choose_status_phrase("tool", Some(&mut recent), &mut p, Some(&catalog));
        assert_eq!(recent.len(), 6);
        // The oldest was dropped.
        assert_eq!(recent[0], "r1");
    }
}
