//! Filesystem discovery shared by skills index loading. This preserves the
//! Python index walk's support-directory and active-organization rules.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

const EXCLUDED: &[&str] = &[
    ".git",
    ".github",
    ".hub",
    ".archive",
    ".curator_backups",
    ".venv",
    "venv",
    "node_modules",
    "site-packages",
    "__pycache__",
    ".tox",
    ".nox",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
];
const SUPPORT: &[&str] = &["references", "templates", "assets", "scripts"];

const SNAPSHOT_VERSION: u8 = 2;

/// The shared skill config cache keeps only the most recent successful mapping.
/// Its key includes size as well as mtime; malformed reads do not replace the
/// previous entry. This intentionally differs from the external-roots cache.
#[derive(Default)]
pub struct RawConfigCache {
    cached: Option<((PathBuf, std::time::SystemTime, u64), serde_json::Value)>,
}

impl RawConfigCache {
    pub fn clear(&mut self) {
        self.cached = None;
    }

    pub fn load(&mut self, path: &Path) -> serde_json::Value {
        let empty = || serde_json::json!({});
        if !path.exists() {
            return empty();
        }
        let key = std::fs::metadata(path).ok().and_then(|metadata| {
            metadata
                .modified()
                .ok()
                .map(|time| (path.to_owned(), time, metadata.len()))
        });
        if let (Some(key), Some((cached_key, value))) = (&key, &self.cached) {
            if key == cached_key {
                return value.clone();
            }
        }
        let parsed = (|| -> anyhow::Result<serde_json::Value> {
            let text = std::fs::read_to_string(path)?
                .replace("\r\n", "\n")
                .replace('\r', "\n");
            let yaml = crate::skill_yaml::parse(&text).map_err(anyhow::Error::msg)?;
            Ok(serde_json::to_value(yaml)?)
        })();
        let value = match parsed {
            Ok(value) if value.is_object() => value,
            Ok(_) => return empty(),
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "Could not read skill config");
                return empty();
            }
        };
        if let Some(key) = key {
            self.cached = Some((key, value.clone()));
        }
        value
    }
}

/// Own the raw-config cache and Python's separate resolved-external cache.
/// External hits do not recheck existence or expansion until config mtime
/// changes. Creation roots are checked on every call, using cached raw config.
#[derive(Default)]
pub struct SourceCache {
    pub raw: RawConfigCache,
    external: std::collections::BTreeMap<(PathBuf, std::time::SystemTime), Vec<PathBuf>>,
}

impl SourceCache {
    pub fn clear(&mut self) {
        self.external.clear();
        self.raw.clear();
    }

    pub fn extra_dirs(
        &mut self,
        config_path: &Path,
        home: &Path,
        local: &Path,
        mut expand: impl FnMut(&str) -> std::io::Result<PathBuf>,
    ) -> std::io::Result<Vec<PathBuf>> {
        let config = self.raw.load(config_path);
        let create = serde_json::json!({"skills": {"create_dir": config["skills"]["create_dir"]}});
        let mut directories = profile_extra_dirs(home, local, &create, &mut expand)?;
        for path in self.external_dirs(config_path, home, local, &mut expand)? {
            if !directories.contains(&path) {
                directories.push(path);
            }
        }
        Ok(directories)
    }

    fn external_dirs(
        &mut self,
        config_path: &Path,
        home: &Path,
        local: &Path,
        expand: impl FnMut(&str) -> std::io::Result<PathBuf>,
    ) -> std::io::Result<Vec<PathBuf>> {
        if !config_path.exists() {
            return Ok(Vec::new());
        }
        let key = std::fs::metadata(config_path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .map(|time| (config_path.to_owned(), time));
        if let Some(cached) = key.as_ref().and_then(|key| self.external.get(key)) {
            return Ok(cached.clone());
        }
        let config = self.raw.load(config_path);
        let Some(settings) = config.get("skills").and_then(serde_json::Value::as_object) else {
            return Ok(Vec::new());
        };
        let raw = settings
            .get("external_dirs")
            .unwrap_or(&serde_json::Value::Null);
        // Falsy settings cache an empty result; truthy malformed types do not.
        if crate::python_value::truthy(raw) && !raw.is_string() && !raw.is_array() {
            return Ok(Vec::new());
        }
        let external = serde_json::json!({"skills": {"external_dirs": raw}});
        let result = profile_extra_dirs(home, local, &external, expand)?;
        if let Some(key) = key {
            self.external.insert(key, result.clone());
        }
        Ok(result)
    }
}

/// Match os.path.expanduser(os.path.expandvars(...)) against a captured
/// environment. Unknown variables and unknown named users remain literal.
/// The caller supplies the captured OS home (including an explicit HOME
/// override); it is distinct from the Hermes profile home.
pub fn expand_source_path(
    raw: &str,
    environment: &std::collections::BTreeMap<String, String>,
    user_home: &Path,
) -> std::io::Result<PathBuf> {
    let expanded =
        crate::webhook_filters::expandvars_with(raw, |name| environment.get(name).cloned());
    match crate::file_read_safety::expand_user(&expanded, user_home) {
        Ok(path) => Ok(path.into()),
        // pathlib.Path.expanduser raises for an unknown account, while the
        // os.path helper used by skills preserves the original spelling.
        Err(error) if error.to_string() == "could not determine home directory" => {
            Ok(expanded.into())
        }
        Err(error) => Err(error),
    }
}

/// Keep one instance for the loader's process lifetime. Like Python's project
/// quarantine cache, decisions persist until explicitly cleared, even if files
/// change. The disk attestation separately binds fresh scans to exact content.
#[derive(Default)]
pub struct ProjectAdmission {
    decisions: std::collections::BTreeMap<PathBuf, bool>,
}

impl ProjectAdmission {
    pub fn clear(&mut self) {
        self.decisions.clear();
    }

    pub fn is_quarantined(&mut self, skill_md: &Path, home: &Path) -> std::io::Result<bool> {
        let directory = skill_md.parent().unwrap_or_else(|| Path::new("."));
        let key = match resolve_path(directory) {
            Ok(path) => path,
            // Python 3.12's RuntimeError happens before the scanner's broad
            // exception handler; ordinary resolution OSErrors use the raw path.
            Err(error) if error.to_string() == "symlink loop" => return Err(error),
            Err(_) => directory.to_owned(),
        };
        if let Some(decision) = self.decisions.get(&key) {
            return Ok(*decision);
        }
        let cache = home.join("cache/project_skill_scans");
        let quarantined = match crate::skills_guard::scan_skill_cached(
            directory,
            "project-local",
            "",
            Some(&cache),
        ) {
            Ok(result) => {
                let blocked = result.verdict == "dangerous";
                if blocked {
                    tracing::warn!(path = %directory.display(), summary = %result.summary, "Project skill quarantined");
                }
                blocked
            }
            Err(error) => {
                tracing::warn!(path = %directory.display(), %error, "Project skill scan failed; quarantining");
                true
            }
        };
        self.decisions.insert(key, quarantined);
        Ok(quarantined)
    }

    /// All project-tier consumers must use this scan gate after directory trust
    /// resolution, rather than calling the generic index walk directly.
    pub fn files(&mut self, directory: &Path, home: &Path) -> std::io::Result<Vec<PathBuf>> {
        let mut accepted = Vec::new();
        for skill in index_files(directory, "SKILL.md")? {
            if !self.is_quarantined(&skill, home)? {
                accepted.push(skill);
            }
        }
        Ok(accepted)
    }
}

fn resolve_path(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    crate::file_read_safety::realpath_abs(
        absolute
            .to_str()
            .ok_or_else(|| std::io::Error::other("non-Unicode project path"))?
            .to_owned(),
    )
}

/// Resolve creation and external roots from already-loaded profile config.
/// The expansion callback uses captured variables and user-home information;
/// relative paths are anchored to this profile's home, never the process cwd.
pub fn profile_extra_dirs(
    home: &Path,
    local_skills: &Path,
    config: &serde_json::Value,
    mut expand: impl FnMut(&str) -> std::io::Result<PathBuf>,
) -> std::io::Result<Vec<PathBuf>> {
    use serde_json::Value;
    let Some(settings) = config.get("skills").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut directories = Vec::new();
    if let Some(raw) = settings.get("create_dir").and_then(Value::as_str) {
        let raw = raw.trim_matches(crate::python_value::python_whitespace);
        if !raw.is_empty() {
            let expanded = expand(raw)?;
            let path = if expanded.is_absolute() {
                expanded
            } else {
                home.join(expanded)
            };
            let resolved = match resolve_path(&path) {
                Ok(path) => path,
                Err(error) if error.to_string() == "symlink loop" => return Err(error),
                Err(_) => path,
            };
            let local = match resolve_path(local_skills) {
                Ok(path) => Some(path),
                Err(error) if error.to_string() == "symlink loop" => return Err(error),
                Err(_) => None,
            };
            if local.as_ref() != Some(&resolved) && resolved.is_dir() {
                directories.push(resolved);
            }
        }
    }
    let entries = match settings.get("external_dirs") {
        Some(Value::String(raw)) if !raw.is_empty() => vec![Value::String(raw.clone())],
        Some(Value::Array(entries)) if !entries.is_empty() => entries.clone(),
        _ => return Ok(directories),
    };
    // External resolution errors propagate in Python, unlike create_dir's
    // ordinary OSError fallback. Resolve local before iterating even blanks.
    let local = resolve_path(local_skills)?;
    for entry in entries {
        let raw = entry
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::python_value::python_repr(&entry));
        let raw = raw.trim_matches(crate::python_value::python_whitespace);
        if raw.is_empty() {
            continue;
        }
        let expanded = expand(raw)?;
        let path = if expanded.is_absolute() {
            expanded
        } else {
            home.join(expanded)
        };
        let resolved = resolve_path(&path)?;
        if resolved != local && !directories.contains(&resolved) && resolved.is_dir() {
            directories.push(resolved);
        }
    }
    Ok(directories)
}

/// Resolve trusted project candidates from the owning config. Path expansion
/// is supplied by the caller's captured environment, not the process-global
/// profile. Directory trust does not replace the later per-skill scan gate.
pub fn project_dirs(
    start: &Path,
    user_home: &Path,
    local_skills: &Path,
    config: &serde_json::Value,
    mut expand: impl FnMut(&str) -> std::io::Result<PathBuf>,
) -> std::io::Result<Vec<PathBuf>> {
    use serde_json::Value;
    let settings = config.get("skills").and_then(Value::as_object);
    if settings.and_then(|s| s.get("project_discovery")) == Some(&Value::Bool(false)) {
        return Ok(Vec::new());
    }
    let Some(root) = project_root(start, user_home)? else {
        return Ok(Vec::new());
    };
    let raw = settings.and_then(|s| s.get("trusted_project_dirs"));
    let entries = match raw {
        Some(Value::String(value)) => vec![Value::String(value.clone())],
        Some(Value::Array(values)) => values.clone(),
        _ => Vec::new(),
    };
    let mut trusted = std::collections::BTreeSet::new();
    for entry in entries {
        let text = entry
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::python_value::python_repr(&entry));
        let text = text.trim_matches(crate::python_value::python_whitespace);
        if text.is_empty() {
            continue;
        }
        if let Ok(path) = expand(text).and_then(|path| resolve_path(&path)) {
            trusted.insert(path);
        }
    }
    if !trusted.contains(&root) {
        return Ok(Vec::new());
    }
    let local = resolve_path(local_skills)?;
    let mut directories = Vec::new();
    for subdir in [".hermes/skills", ".agents/skills"] {
        let candidate = root.join(subdir);
        if candidate.is_dir() {
            if let Ok(candidate) = resolve_path(&candidate) {
                if candidate != local {
                    directories.push(candidate);
                }
            }
        }
    }
    Ok(directories)
}

/// Locate the nearest checkout from a captured surface working directory.
/// The profile home and OS user home are distinct: only the latter is excluded
/// to avoid treating every session as a project in a dotfiles checkout.
pub fn project_root(start: &Path, user_home: &Path) -> std::io::Result<Option<PathBuf>> {
    let Ok(mut current) = resolve_path(start) else {
        return Ok(None);
    };
    let home = resolve_path(user_home)?;
    for _ in 0..64 {
        if current.join(".git").exists() {
            return Ok((current != home).then_some(current));
        }
        if !current.pop() {
            return Ok(None);
        }
    }
    Ok(None)
}

/// Read one skill for an index scan. Platform rejection precedes environment
/// detection; read/detection failures retain an unnamed, visible entry just as
/// _parse_skill_file does. Explicit skill loads must not use this offer filter.
pub fn parse_skill_file(
    path: &Path,
    host: &str,
    termux: bool,
    mut detect: impl FnMut(&str) -> Result<bool, String>,
) -> (bool, serde_json::Value, String) {
    let failed = |error: &dyn std::fmt::Display| {
        tracing::warn!(path = %path.display(), %error, "Failed to parse skill file");
        (true, serde_json::json!({}), String::new())
    };
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => return failed(&error),
    };
    // Path.read_text uses universal newlines, unlike the standalone parser.
    let raw = raw.replace("\r\n", "\n").replace('\r', "\n");
    let (frontmatter, _) = parse_frontmatter(&raw);
    if !crate::skills_index::matches_platform(&frontmatter["platforms"], host, termux) {
        return (false, frontmatter, String::new());
    }
    let mut error = None;
    let compatible =
        crate::skills_index::matches_environment(&frontmatter["environments"], |tag| {
            match detect(tag) {
                Ok(active) => active,
                Err(failure) => {
                    error = Some(failure);
                    // Stop detection immediately, then take the same outer failure
                    // path as a raised exception in Python's environment predicate.
                    true
                }
            }
        });
    if let Some(error) = error {
        return failed(&error);
    }
    let description = if compatible {
        crate::skills_index::extract_description(&frontmatter)
    } else {
        String::new()
    };
    (compatible, frontmatter, description)
}

/// Separate the optional frontmatter using the Python skill loader's fence
/// rules. Invalid YAML falls back to literal key/value lines, preserving the
/// body even when metadata cannot be decoded.
pub fn parse_frontmatter(content: &str) -> (serde_json::Value, &str) {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let empty = || serde_json::json!({});
    let Some(rest) = content.strip_prefix("---") else {
        return (empty(), content);
    };
    // Python's \\s includes the four information separators that Rust's
    // Unicode regex whitespace class omits. Use the shared predicate instead.
    let mut fence = None;
    for (start, _) in rest.match_indices("\n---") {
        let after = start + 4;
        let whitespace = &rest[after..];
        let end = whitespace
            .char_indices()
            .take_while(|(_, ch)| crate::python_value::python_whitespace(*ch))
            .filter(|(_, ch)| *ch == '\n')
            .map(|(offset, _)| after + offset + 1)
            .last();
        if let Some(end) = end {
            fence = Some((start, end));
            break;
        }
    }
    let Some((start, end)) = fence else {
        return (empty(), content);
    };
    let yaml = &rest[..start];
    let body = &rest[end..];
    let parsed = crate::skill_yaml::parse(yaml);
    if let Ok(value) = parsed {
        if !value.is_mapping() {
            return (empty(), body);
        }
        if let Ok(value) = serde_json::to_value(value) {
            return (value, body);
        }
    }
    let mut fallback = serde_json::Map::new();
    for line in yaml
        .trim_matches(crate::python_value::python_whitespace)
        .split('\n')
    {
        if let Some((key, value)) = line.split_once(':') {
            fallback.insert(
                key.trim_matches(crate::python_value::python_whitespace)
                    .to_owned(),
                serde_json::Value::String(
                    value
                        .trim_matches(crate::python_value::python_whitespace)
                        .to_owned(),
                ),
            );
        }
    }
    (serde_json::Value::Object(fallback), body)
}

/// Construct the same cache entry for a fresh scan as the Python prompt loader.
/// Organization paths carry provenance but derive names inside the mirror.
pub fn snapshot_entry(
    file: &Path,
    skills: &Path,
    frontmatter: &serde_json::Value,
    description: impl Into<serde_json::Value>,
) -> Result<serde_json::Value, &'static str> {
    use serde_json::{json, Value};
    let relative = file
        .strip_prefix(skills)
        .map_err(|_| "skill outside root")?;
    let mut parts = relative
        .iter()
        .map(|part| part.to_str().ok_or("non-Unicode skill path"))
        .collect::<Result<Vec<_>, _>>()?;
    let org = if parts.len() >= 3 && parts[0] == "_org" {
        let org = parts[1].to_owned();
        parts.drain(..2);
        Some(org)
    } else {
        None
    };
    let (name, category) = if parts.len() >= 2 {
        (
            parts[parts.len() - 2].to_owned(),
            if parts.len() > 2 {
                parts[..parts.len() - 2].join("/")
            } else {
                parts[0].to_owned()
            },
        )
    } else {
        (
            file.parent()
                .and_then(Path::file_name)
                .and_then(|v| v.to_str())
                .unwrap_or("")
                .to_owned(),
            "general".to_owned(),
        )
    };
    let metadata = frontmatter
        .as_object()
        .ok_or("frontmatter must be an object")?;
    let string = |v: &Value| {
        v.as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::python_value::python_repr(v))
    };
    let platforms = metadata
        .get("platforms")
        .filter(|v| crate::python_value::truthy(v));
    let values = match platforms {
        None => Vec::new(),
        Some(Value::String(value)) => vec![Value::String(value.clone())],
        Some(Value::Array(values)) => values.clone(),
        Some(Value::Object(values)) => values
            .keys()
            .map(|key| Value::String(key.clone()))
            .collect(),
        _ => return Err("platforms is not iterable"),
    };
    let platforms = values
        .iter()
        .map(|v| {
            string(v)
                .trim_matches(crate::python_value::python_whitespace)
                .to_owned()
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    let mut entry = json!({
        "skill_name": name,
        "category": category,
        "frontmatter_name": metadata.get("name").map(string).unwrap_or(name),
        "description": description.into(),
        "platforms": platforms,
        "conditions": crate::skills_index::extract_conditions(frontmatter),
    });
    if let Some(org) = org {
        let provenance = std::fs::read(skills.join("_org").join(&org).join(".org-provenance.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let author = provenance
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|value| {
                value
                    .get("author_device")
                    .filter(|v| crate::python_value::truthy(v))
                    .or_else(|| {
                        value
                            .get("author_user_id")
                            .filter(|v| crate::python_value::truthy(v))
                    })
            })
            .map(string)
            .unwrap_or_default();
        entry["org_id"] = json!(org);
        entry["org_author"] = json!(author);
    }
    Ok(entry)
}

/// Read only this profile's cache. A missing, malformed or stale snapshot is a
/// cache miss; errors discovering the live manifest still propagate as Python does.
pub fn load_snapshot(home: &Path, skills: &Path) -> std::io::Result<Option<serde_json::Value>> {
    let bytes = match std::fs::read(home.join(".skills_prompt_snapshot.json")) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(None);
    };
    let Ok(snapshot) = serde_json::from_str::<serde_json::Value>(text) else {
        return Ok(None);
    };
    if !snapshot.is_object()
        || !crate::python_value::python_equal(
            &snapshot["version"],
            &serde_json::json!(SNAPSHOT_VERSION),
        )
    {
        return Ok(None);
    }
    let current = serde_json::to_value(manifest(skills)?).map_err(std::io::Error::other)?;
    if !crate::python_value::python_equal(&snapshot["manifest"], &current) {
        return Ok(None);
    }
    Ok(Some(snapshot))
}

/// Persist resolved metadata best-effort. Failure to write the optimization
/// must not prevent the caller from using the freshly scanned skill entries.
pub fn write_snapshot(
    home: &Path,
    manifest: &std::collections::BTreeMap<String, [i128; 2]>,
    entries: &[serde_json::Value],
    descriptions: &std::collections::BTreeMap<String, String>,
) {
    let write = || -> std::io::Result<()> {
        let payload = serde_json::json!({"version": SNAPSHOT_VERSION, "manifest": manifest, "skills": entries, "category_descriptions": descriptions});
        let bytes = serde_json::to_vec_pretty(&payload).map_err(std::io::Error::other)?;
        let path = home.join(".skills_prompt_snapshot.json");
        // Preserve a managed profile's snapshot symlink when replacing its
        // target. The shared writer supplies temporary-file fsync and rename.
        let path = if path.is_symlink() {
            let absolute = if path.is_absolute() {
                path
            } else {
                std::env::current_dir()?.join(path)
            };
            crate::file_read_safety::realpath_abs(
                absolute
                    .to_str()
                    .ok_or_else(|| std::io::Error::other("non-Unicode snapshot path"))?
                    .to_owned(),
            )?
        } else {
            path
        };
        crate::atomic_file::write(&path, &bytes)
    };
    if let Err(error) = write() {
        tracing::debug!(%error, "Could not write skills prompt snapshot");
    }
}

pub fn active_org(skills: &Path) -> std::io::Result<Option<String>> {
    // Python swallows filesystem errors but propagates invalid UTF-8 here.
    let bytes = match std::fs::read(skills.join("_org/.active_org")) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let value = String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let value = value.trim_matches(crate::python_value::python_whitespace);
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

pub fn index_files(skills: &Path, filename: &str) -> std::io::Result<Vec<PathBuf>> {
    let active = active_org(skills)?;
    let mut matches = Vec::new();
    walk(skills, active.as_deref(), &[filename], |path| {
        matches.push(path)
    });
    matches.sort();
    Ok(matches)
}

fn walk(skills: &Path, active: Option<&str>, filenames: &[&str], mut visit: impl FnMut(PathBuf)) {
    let org_root = skills.join("_org");
    let mut pending = vec![skills.to_owned()];
    while let Some(root) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        // os.walk discards the current directory if scandir fails partway
        // through enumeration, rather than returning a partial directory view.
        let Ok(entries) = entries.collect::<std::io::Result<Vec<_>>>() else {
            continue;
        };
        let mut directories = Vec::new();
        let mut files = Vec::new();
        for entry in entries {
            let path = entry.path();
            // os.walk(followlinks=True) follows directory symlinks; broken
            // symlinks remain in its files list and may match the filename.
            if path.is_dir() {
                directories.push(path);
            } else {
                files.push(entry.file_name());
            }
        }
        let has_skill = files.iter().any(|name| name == "SKILL.md");
        for filename in filenames {
            if files.iter().any(|name| name == *filename) {
                visit(root.join(filename));
            }
        }
        for directory in directories {
            let name = directory.file_name().unwrap();
            if root == skills && name == "_org" && active.is_none() {
                continue;
            }
            if root == org_root && Some(name) != active.map(std::ffi::OsStr::new) {
                continue;
            }
            if name.to_str().is_some_and(|name| {
                EXCLUDED.contains(&name) || (has_skill && SUPPORT.contains(&name))
            }) {
                continue;
            }
            pending.push(directory);
        }
    }
}

/// Snapshot validation uses nanoseconds for skill files, but Python's existing
/// sync marker contract stores int(st_mtime), which truncates float seconds.
pub fn manifest(skills: &Path) -> std::io::Result<std::collections::BTreeMap<String, [i128; 2]>> {
    let active = active_org(skills)?;
    let mut manifest = std::collections::BTreeMap::new();
    let stamp = |path: &Path, marker: bool| -> Option<[i128; 2]> {
        let metadata = std::fs::metadata(path).ok()?;
        let modified = metadata.modified().ok()?;
        let (negative, duration) = match modified.duration_since(std::time::UNIX_EPOCH) {
            Ok(duration) => (false, duration),
            Err(error) => (true, error.duration()),
        };
        let sign = if negative { -1 } else { 1 };
        let nanos = i128::try_from(duration.as_nanos()).ok()? * sign;
        let time = if marker {
            // Split seconds and the positive nanosecond remainder like stat,
            // including timestamps before the epoch and rounding at a second.
            let secs = nanos.div_euclid(1_000_000_000);
            let nanos = nanos.rem_euclid(1_000_000_000);
            (secs as f64 + nanos as f64 * 1e-9) as i128
        } else {
            nanos
        };
        Some([time, i128::from(metadata.len())])
    };
    if let Some(stamp) = stamp(&skills.join("_org/.active_org"), true) {
        manifest.insert("_org/.active_org".into(), stamp);
    }
    let mut invalid_path = false;
    walk(
        skills,
        active.as_deref(),
        &["SKILL.md", "DESCRIPTION.md"],
        |path| {
            if let Some(stamp) = stamp(&path, false) {
                if let Some(relative) = path.strip_prefix(skills).ok().and_then(Path::to_str) {
                    manifest.insert(relative.to_owned(), stamp);
                } else {
                    invalid_path = true;
                }
            }
        },
    );
    if invalid_path {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "non-Unicode skill path",
        ));
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_cache_keeps_resolved_paths_while_creation_is_rechecked() {
        let home = std::env::temp_dir().join(format!("hermes-source-cache-{}", std::process::id()));
        let local = home.join("skills");
        let config = home.join("config.yaml");
        for name in ["skills", "created", "first", "second"] {
            std::fs::create_dir_all(home.join(name)).unwrap();
        }
        std::fs::write(
            &config,
            "skills:\n  create_dir: created\n  external_dirs: first\n",
        )
        .unwrap();
        let stamp = std::fs::metadata(&config).unwrap().modified().unwrap();
        let mut cache = SourceCache::default();
        let first = cache
            .extra_dirs(&config, &home, &local, |raw| Ok(raw.into()))
            .unwrap();
        assert_eq!(first, vec![home.join("created"), home.join("first")]);
        std::fs::remove_dir(home.join("created")).unwrap();
        std::fs::remove_dir(home.join("first")).unwrap();
        assert_eq!(
            cache
                .extra_dirs(&config, &home, &local, |raw| {
                    assert_eq!(raw, "created", "cached external path was expanded again");
                    Ok(raw.into())
                })
                .unwrap(),
            vec![home.join("first")]
        );
        // A size-only edit refreshes raw config, but not the external cache.
        std::fs::write(
            &config,
            "skills:\n  create_dir: second\n  external_dirs: second\n# size change\n",
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&config)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        assert_eq!(
            cache
                .extra_dirs(&config, &home, &local, |raw| Ok(raw.into()))
                .unwrap(),
            vec![home.join("second"), home.join("first")]
        );
        cache.clear();
        assert_eq!(
            cache
                .extra_dirs(&config, &home, &local, |raw| Ok(raw.into()))
                .unwrap(),
            vec![home.join("second")]
        );
        std::fs::remove_file(&config).unwrap();
        assert!(cache
            .extra_dirs(&config, &home, &local, |raw| Ok(raw.into()))
            .unwrap()
            .is_empty());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn raw_skill_config_cache_uses_time_size_and_successful_mapping_only() {
        let root =
            std::env::temp_dir().join(format!("hermes-raw-skill-config-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.yaml");
        let mut cache = RawConfigCache::default();
        assert_eq!(cache.load(&path), serde_json::json!({}));
        std::fs::write(&path, "skills:\n  project_discovery: yes\n").unwrap();
        let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
        let first = cache.load(&path);
        assert_eq!(first["skills"]["project_discovery"], true);
        // Same size and timestamp preserve the cached mapping, as in Python.
        std::fs::write(&path, "skills:\n  project_discovery: off\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        assert_eq!(cache.load(&path), first);
        cache.clear();
        assert_eq!(cache.load(&path)["skills"]["project_discovery"], false);
        std::fs::write(&path, "skills:\n  project_discovery: true\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        assert_eq!(cache.load(&path)["skills"]["project_discovery"], true);
        let successful_key = cache.cached.as_ref().unwrap().0.clone();
        std::fs::write(&path, "[broken").unwrap();
        assert_eq!(cache.load(&path), serde_json::json!({}));
        assert_eq!(cache.cached.as_ref().unwrap().0, successful_key);
        std::fs::write(&path, "[one, two]").unwrap();
        assert_eq!(cache.load(&path), serde_json::json!({}));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn source_expansion_uses_captured_values_before_tilde_expansion() {
        let environment = std::collections::BTreeMap::from([
            ("ROOT".into(), "~/captured".into()),
            ("NESTED".into(), "$ROOT".into()),
            ("odd name".into(), "strange".into()),
            ("EMPTY".into(), String::new()),
        ]);
        let home = Path::new("/captured/home");
        for (raw, expected) in [
            ("$ROOT/x", "/captured/home/captured/x"),
            ("${ROOT}/x", "/captured/home/captured/x"),
            ("$NESTED", "$ROOT"),
            ("${odd name}/x", "strange/x"),
            ("$EMPTY/x", "/x"),
            ("$MISSING/x", "$MISSING/x"),
            ("${unfinished", "${unfinished"),
            ("$$ROOT", "$~/captured"),
            (
                "~hermes_nonexistent_user_728915/x",
                "~hermes_nonexistent_user_728915/x",
            ),
        ] {
            assert_eq!(
                expand_source_path(raw, &environment, home).unwrap(),
                PathBuf::from(expected),
                "{raw}"
            );
        }
        assert_eq!(
            expand_source_path("~/x", &environment, Path::new("/")).unwrap(),
            PathBuf::from("/x")
        );
        assert_eq!(
            expand_source_path("~", &environment, Path::new("/")).unwrap(),
            PathBuf::from("/")
        );
    }

    #[cfg(unix)]
    #[test]
    fn profile_extra_roots_use_home_order_and_resolved_deduplication() {
        let root =
            std::env::temp_dir().join(format!("hermes-profile-extra-{}", std::process::id()));
        let local = root.join("skills");
        for name in ["skills", "created", "external", "None"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::os::unix::fs::symlink(&local, root.join("local-alias")).unwrap();
        std::os::unix::fs::symlink(root.join("external"), root.join("external-alias")).unwrap();
        let config = serde_json::json!({"skills": {
            "create_dir": " created ",
            "external_dirs": ["$EXTERNAL", "external-alias", "created", "local-alias", "missing", null, " "]
        }});
        let mut expanded = Vec::new();
        let environment =
            std::collections::BTreeMap::from([("EXTERNAL".into(), "external".into())]);
        let paths = profile_extra_dirs(&root, &local, &config, |raw| {
            expanded.push(raw.to_owned());
            expand_source_path(raw, &environment, &root)
        })
        .unwrap();
        assert_eq!(
            paths,
            vec![
                root.join("created"),
                root.join("external"),
                root.join("None")
            ]
        );
        assert_eq!(
            expanded,
            [
                "created",
                "$EXTERNAL",
                "external-alias",
                "created",
                "local-alias",
                "missing",
                "None"
            ]
        );
        let config =
            serde_json::json!({"skills": {"create_dir": "missing", "external_dirs": "external"}});
        assert_eq!(
            profile_extra_dirs(&root, &local, &config, |raw| Ok(raw.into())).unwrap(),
            vec![root.join("external")]
        );
        std::os::unix::fs::symlink("loop", root.join("loop")).unwrap();
        let config = serde_json::json!({"skills": {"external_dirs": ["loop"]}});
        assert!(profile_extra_dirs(&root, &local, &config, |raw| Ok(raw.into())).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn project_admission_filters_danger_and_caches_decisions_until_clear() {
        let root =
            std::env::temp_dir().join(format!("hermes-project-admission-{}", std::process::id()));
        let home = root.join("home");
        let project = root.join("project");
        for name in ["clean", "danger", "caution", "error"] {
            std::fs::create_dir_all(project.join(name)).unwrap();
            std::fs::write(project.join(name).join("SKILL.md"), "# Safe skill\n").unwrap();
        }
        std::fs::write(project.join("danger/payload.exe"), b"binary").unwrap();
        std::fs::write(project.join("caution/SKILL.md"), "invisible\u{200b}text").unwrap();
        std::os::unix::fs::symlink("loop", project.join("error/loop")).unwrap();
        // A dangling/looping symlink contributes no bytes to Python's hash.
        // Use distinct content so this exercises a fresh scanner failure,
        // rather than reusing the clean bundle's identical-content attestation.
        std::fs::write(project.join("error/SKILL.md"), "# Scanner failure case\n").unwrap();
        let mut admission = ProjectAdmission::default();
        let accepted = admission.files(&project, &home).unwrap();
        assert_eq!(
            accepted,
            vec![
                project.join("caution/SKILL.md"),
                project.join("clean/SKILL.md")
            ]
        );
        assert!(home.join("cache/project_skill_scans").is_dir());
        assert!(!project.join(".scan-cache").exists());
        std::fs::remove_file(project.join("danger/payload.exe")).unwrap();
        assert!(admission
            .is_quarantined(&project.join("danger/SKILL.md"), &home)
            .unwrap());
        std::os::unix::fs::symlink(project.join("danger"), root.join("alias")).unwrap();
        assert!(admission
            .is_quarantined(&root.join("alias/SKILL.md"), &home)
            .unwrap());
        admission.clear();
        assert!(!admission
            .is_quarantined(&project.join("danger/SKILL.md"), &home)
            .unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_candidates_require_exact_trust_and_exclude_local_skills() {
        use serde_json::json;
        let root =
            std::env::temp_dir().join(format!("hermes-project-candidates-{}", std::process::id()));
        let repo = root.join("repo");
        for sub in [".git", ".hermes/skills", ".agents/skills"] {
            std::fs::create_dir_all(repo.join(sub)).unwrap();
        }
        let home = root.join("user-home");
        let local = repo.join(".hermes/skills");
        let identity = |text: &str| Ok(PathBuf::from(text));
        assert!(project_dirs(&repo, &home, &local, &json!({}), identity)
            .unwrap()
            .is_empty());
        assert!(project_dirs(
            &repo,
            &home,
            &local,
            &json!({"skills":{"trusted_project_dirs":[root]}}),
            identity
        )
        .unwrap()
        .is_empty());
        let config = json!({"skills":{"trusted_project_dirs":[repo],"project_discovery":false}});
        assert!(project_dirs(&repo, &home, &local, &config, |_| panic!(
            "disabled before expansion"
        ))
        .unwrap()
        .is_empty());
        // Only boolean false disables discovery, not a numeric zero.
        let config = json!({"skills":{"trusted_project_dirs":[repo],"project_discovery":0}});
        assert_eq!(
            project_dirs(&repo.join("missing"), &home, &local, &config, identity).unwrap(),
            [repo.join(".agents/skills")]
        );
        #[cfg(unix)]
        {
            let alias = root.join("alias");
            std::os::unix::fs::symlink(&repo, &alias).unwrap();
            let config = json!({"skills":{"trusted_project_dirs":alias}});
            assert_eq!(
                project_dirs(&repo, &home, &local, &config, identity).unwrap(),
                [repo.join(".agents/skills")]
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_root_prefers_nearest_checkout_and_excludes_user_home() {
        let root = std::env::temp_dir().join(format!("hermes-project-root-{}", std::process::id()));
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".git")).unwrap();
        assert!(project_root(&home.join("untracked/missing"), &home)
            .unwrap()
            .is_none());
        let nested = home.join("repo");
        std::fs::create_dir_all(&nested).unwrap();
        // Worktrees and submodules use a .git file, which counts equally.
        std::fs::write(nested.join(".git"), "gitdir: elsewhere").unwrap();
        assert_eq!(
            project_root(&nested.join("missing/path"), &home).unwrap(),
            Some(nested.clone())
        );
        let deepest = nested.join("inner");
        std::fs::create_dir_all(deepest.join(".git")).unwrap();
        assert_eq!(project_root(&deepest, &home).unwrap(), Some(deepest));
        let mut beyond_limit = nested.clone();
        for _ in 0..64 {
            beyond_limit.push("child");
        }
        assert!(project_root(&beyond_limit, &home).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skill_file_scan_orders_filters_and_fails_open() {
        let root = std::env::temp_dir().join(format!("hermes-skill-file-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("SKILL.md");
        let fallback = (true, serde_json::json!({}), String::new());
        assert_eq!(
            parse_skill_file(&path, "linux", false, |_| panic!("no metadata")),
            fallback
        );
        std::fs::write(&path, [0xff]).unwrap();
        assert_eq!(
            parse_skill_file(&path, "linux", false, |_| panic!("invalid file")),
            fallback
        );
        std::fs::write(
            &path,
            "---\rplatforms: [windows]\renvironments: [docker]\rdescription: hidden\r---\rbody",
        )
        .unwrap();
        let (visible, metadata, description) = parse_skill_file(&path, "linux", false, |_| {
            panic!("platform must reject first")
        });
        assert!(!visible);
        assert_eq!(metadata["description"], "hidden");
        assert!(description.is_empty());
        std::fs::write(&path, "---\r\nplatforms: [linux]\r\nenvironments: [docker, s6]\r\ndescription: \"text\"\r\n---\r\nbody").unwrap();
        let mut calls = Vec::new();
        let (visible, _, description) = parse_skill_file(&path, "linux", false, |tag| {
            calls.push(tag.to_owned());
            Ok(tag == "s6")
        });
        assert!(visible);
        assert_eq!(description, "text");
        assert_eq!(calls, ["docker", "s6"]);
        calls.clear();
        assert_eq!(
            parse_skill_file(&path, "linux", false, |tag| {
                calls.push(tag.to_owned());
                Err("detector failed".into())
            }),
            fallback
        );
        assert_eq!(calls, ["docker"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn frontmatter_matches_actual_python_oracle() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../../tools/skill-frontmatter-goldens.json"
        ))
        .unwrap();
        let mut mismatches = Vec::new();
        for (number, case) in cases.iter().enumerate() {
            let (frontmatter, body) = parse_frontmatter(case["input"].as_str().unwrap());
            if frontmatter != case["frontmatter"] || body != case["body"].as_str().unwrap() {
                mismatches.push(format!(
                    "case {number}: {frontmatter:?} / expected {:?}; body {body:?}",
                    case["frontmatter"]
                ));
            }
        }
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    }

    #[test]
    fn frontmatter_preserves_python_fences_body_and_fallback() {
        use serde_json::json;
        for (input, metadata, body) in [
            ("\u{feff}plain", json!({}), "plain"),
            (
                "---\nname: example\n---\nbody",
                json!({"name":"example"}),
                "body",
            ),
            (
                "---\nname: example\n---",
                json!({}),
                "---\nname: example\n---",
            ),
            (
                "---\nname: [broken\nother: literal:colon\n---\nbody",
                json!({"name":"[broken", "other":"literal:colon"}),
                "body",
            ),
            ("---\n[one, two]\n---\nbody", json!({}), "body"),
            (
                "---\nname: example\n---\u{1c}\n \nbody",
                json!({"name":"example"}),
                "body",
            ),
            (
                "---\nbase: &base {description: text}\nmetadata: {<<: *base}\n---\nbody",
                json!({"base":{"description":"text"},"metadata":{"description":"text"}}),
                "body",
            ),
        ] {
            assert_eq!(parse_frontmatter(input), (metadata, body), "{input:?}");
        }
    }

    #[test]
    fn frontmatter_duplicate_keys_keep_last_typed_value() {
        use serde_json::json;
        let input = "---\nmetadata:\n  count: 1\n  count: 2\nname: first\nname: final\n---\nbody";
        assert_eq!(
            parse_frontmatter(input),
            (json!({"metadata":{"count":2},"name":"final"}), "body")
        );
        let tagged = "---\nname: !unknown tag\n---\nbody";
        assert_eq!(
            parse_frontmatter(tagged),
            (json!({"name":"!unknown tag"}), "body")
        );
    }

    #[test]
    fn snapshot_entries_match_actual_python_oracle() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../tools/skill-entry-goldens.json")).unwrap();
        let root = std::env::temp_dir().join(format!("hermes-entry-oracle-{}", std::process::id()));
        for (number, case) in cases.iter().enumerate() {
            let skills = root.join(number.to_string()).join("skills");
            std::fs::create_dir_all(&skills).unwrap();
            let relative = Path::new(case["relative_path"].as_str().unwrap());
            if let Some(raw) = case["provenance"].as_str() {
                let parts: Vec<_> = relative.iter().collect();
                if parts.len() >= 3 && parts[0] == "_org" {
                    let path = skills
                        .join("_org")
                        .join(parts[1])
                        .join(".org-provenance.json");
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, raw).unwrap();
                }
            }
            let actual = snapshot_entry(
                &skills.join(relative),
                &skills,
                &case["frontmatter"],
                case["description"].clone(),
            );
            if case["error"].is_null() {
                assert_eq!(actual.unwrap(), case["expected"], "case {number}");
            } else {
                assert!(actual.is_err(), "case {number}: expected {}", case["error"]);
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_entries_match_python_paths_and_raw_metadata() {
        use serde_json::json;
        let root = std::env::temp_dir().join(format!("hermes-entry-{}", std::process::id()));
        let skills = root.join("skills");
        std::fs::create_dir_all(skills.join("_org/team")).unwrap();
        for (relative, name, category) in [
            ("SKILL.md", "skills", "general"),
            ("one/SKILL.md", "one", "one"),
            ("cat/one/SKILL.md", "one", "cat"),
            ("_org/team/one/SKILL.md", "one", "one"),
        ] {
            let entry = snapshot_entry(
                &skills.join(relative),
                &skills,
                &json!({"name": null, "platforms": [null, false, " linux "]}),
                "desc",
            )
            .unwrap();
            assert_eq!(entry["skill_name"], name);
            assert_eq!(entry["category"], category);
            assert_eq!(entry["frontmatter_name"], "None");
            assert_eq!(entry["platforms"], json!(["None", "False", "linux"]));
            if relative.starts_with("_org/") {
                assert_eq!(entry["org_id"], "team");
                assert_eq!(entry["org_author"], "");
            } else {
                assert!(entry.get("org_id").is_none());
            }
        }
        let file = skills.join("_org/team/one/SKILL.md");
        let provenance = skills.join("_org/team/.org-provenance.json");
        for (raw, author) in [
            (
                r#"{"author_device":"device","author_user_id":"user"}"#,
                "device",
            ),
            (r#"{"author_device":false,"author_user_id":"user"}"#, "user"),
            ("[]", ""),
            ("{", ""),
        ] {
            std::fs::write(&provenance, raw).unwrap();
            assert_eq!(
                snapshot_entry(&file, &skills, &json!({}), "").unwrap()["org_author"],
                author
            );
        }
        assert!(snapshot_entry(&file, &skills, &json!({"platforms": true}), "").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_is_profile_scoped_and_invalidated_by_live_metadata() {
        let root = std::env::temp_dir().join(format!("hermes-snapshot-{}", std::process::id()));
        let home = root.join("profile");
        let skills = home.join("skills");
        std::fs::create_dir_all(skills.join("example")).unwrap();
        let file = skills.join("example/SKILL.md");
        std::fs::write(&file, "original").unwrap();
        assert!(load_snapshot(&home, &skills).unwrap().is_none());
        let entries = vec![serde_json::json!({"name": "example", "description": "日本語"})];
        write_snapshot(
            &home,
            &manifest(&skills).unwrap(),
            &entries,
            &Default::default(),
        );
        let loaded = load_snapshot(&home, &skills).unwrap().unwrap();
        assert_eq!(loaded["skills"], serde_json::json!(entries));
        assert!(load_snapshot(&root.join("other-profile"), &skills)
            .unwrap()
            .is_none());
        // Size changes guarantee invalidation even on coarse timestamp filesystems.
        std::fs::write(&file, "changed and longer").unwrap();
        assert!(load_snapshot(&home, &skills).unwrap().is_none());
        write_snapshot(
            &home,
            &manifest(&skills).unwrap(),
            &entries,
            &Default::default(),
        );
        assert!(load_snapshot(&home, &skills).unwrap().is_some());
        std::fs::create_dir_all(skills.join("_org")).unwrap();
        std::fs::write(skills.join("_org/.active_org"), [0xff]).unwrap();
        assert_eq!(
            load_snapshot(&home, &skills).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        // Malformed cache data is rejected before attempting the live manifest.
        std::fs::write(home.join(".skills_prompt_snapshot.json"), b"{").unwrap();
        assert!(load_snapshot(&home, &skills).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn snapshot_write_preserves_symlink_and_failure_is_best_effort() {
        let root =
            std::env::temp_dir().join(format!("hermes-snapshot-link-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("managed.json");
        std::fs::write(&target, "old").unwrap();
        let link = root.join(".skills_prompt_snapshot.json");
        std::os::unix::fs::symlink("managed.json", &link).unwrap();
        write_snapshot(&root, &Default::default(), &[], &Default::default());
        assert!(link.is_symlink());
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(value["skills"], serde_json::json!([]));
        assert!(load_snapshot(&root, &root.join("missing-skills"))
            .unwrap()
            .is_some());
        // An ordinary file cannot serve as the profile directory. Cache failure
        // must leave that file intact and must not escape to the caller.
        write_snapshot(&target, &Default::default(), &[], &Default::default());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&target).unwrap()).unwrap(),
            value
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn filesystem_walk_matches_actual_python_fixtures() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../tools/skill-walk-goldens.json")).unwrap();
        for (number, case) in cases.iter().enumerate() {
            let root = std::env::temp_dir()
                .join(format!("hermes-skill-walk-{}-{number}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            for (relative, content) in case["files"].as_object().unwrap() {
                let path = root.join(relative);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, content.as_str().unwrap()).unwrap();
            }
            for (relative, target) in case["symlinks"].as_object().unwrap() {
                let path = root.join(relative);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::os::unix::fs::symlink(target.as_str().unwrap(), path).unwrap();
            }
            let paths: Vec<_> = index_files(&root, case["filename"].as_str().unwrap())
                .unwrap()
                .into_iter()
                .map(|path| {
                    path.strip_prefix(&root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned()
                })
                .collect();
            assert_eq!(serde_json::json!(paths), case["expected"], "case {number}");
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn manifest_preserves_file_nanoseconds_and_marker_seconds() {
        let root =
            std::env::temp_dir().join(format!("hermes-skill-manifest-{}", std::process::id()));
        let stamp = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789);
        for (name, content) in [
            ("SKILL.md", "root"),
            ("DESCRIPTION.md", "desc"),
            ("_org/.active_org", "red"),
            ("_org/red/entry/SKILL.md", "org"),
            ("_org/blue/entry/SKILL.md", "stale"),
            ("references/old/SKILL.md", "ignored"),
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(stamp))
                .unwrap();
        }
        let expected = [
            ("SKILL.md".into(), [1_700_000_000_123_456_789, 4]),
            ("DESCRIPTION.md".into(), [1_700_000_000_123_456_789, 4]),
            ("_org/.active_org".into(), [1_700_000_000, 3]),
            (
                "_org/red/entry/SKILL.md".into(),
                [1_700_000_000_123_456_789, 3],
            ),
        ]
        .into();
        assert_eq!(manifest(&root).unwrap(), expected);
        let marker = std::fs::File::options()
            .write(true)
            .open(root.join("_org/.active_org"))
            .unwrap();
        marker
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH - std::time::Duration::from_millis(500)),
            )
            .unwrap();
        assert_eq!(manifest(&root).unwrap()["_org/.active_org"], [0, 3]);
        #[cfg(unix)]
        {
            std::fs::create_dir_all(root.join("broken")).unwrap();
            std::os::unix::fs::symlink("missing", root.join("broken/SKILL.md")).unwrap();
            assert!(index_files(&root, "SKILL.md")
                .unwrap()
                .contains(&root.join("broken/SKILL.md")));
            assert!(!manifest(&root).unwrap().contains_key("broken/SKILL.md"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovers_support_and_org_entries_only_in_their_active_scope() {
        let root =
            std::env::temp_dir().join(format!("hermes-skill-discovery-{}", std::process::id()));
        for name in [
            "plain/references/nested/SKILL.md",
            "package/SKILL.md",
            "package/references/old/SKILL.md",
            ".git/hidden/SKILL.md",
            ".visible/SKILL.md",
            "_org/red/one/SKILL.md",
            "_org/blue/two/SKILL.md",
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "skill").unwrap();
        }
        let relative = || {
            index_files(&root, "SKILL.md")
                .unwrap()
                .into_iter()
                .map(|path| path.strip_prefix(&root).unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            relative(),
            [
                ".visible/SKILL.md",
                "package/SKILL.md",
                "plain/references/nested/SKILL.md"
            ]
            .map(PathBuf::from)
        );
        std::fs::write(root.join("_org/.active_org"), "\u{1c}red\n").unwrap();
        assert!(relative().contains(&PathBuf::from("_org/red/one/SKILL.md")));
        assert!(!relative().contains(&PathBuf::from("_org/blue/two/SKILL.md")));
        std::fs::write(root.join("_org/.active_org"), [0xff]).unwrap();
        assert_eq!(
            index_files(&root, "SKILL.md").unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
