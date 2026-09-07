//! Profile skill ingestion for the prompt index. Keep raw snapshot entries
//! separate from visible entries so changing a toolset does not lose metadata.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{json, Value};

use crate::{skill_discovery, skills_index};

pub struct Visibility<'a> {
    pub host: &'a str,
    pub termux: bool,
    pub platform: Option<&'a str>,
    pub tools: Option<&'a BTreeSet<String>>,
    pub toolsets: Option<&'a BTreeSet<String>>,
    pub disabled: &'a BTreeSet<String>,
}

impl Visibility<'_> {
    fn permits(&self, entry: &Value) -> Result<bool, &'static str> {
        let empty = json!("");
        let name = entry
            .get("skill_name")
            .filter(|v| crate::python_value::truthy(v))
            .unwrap_or(&empty);
        let frontmatter_name = entry
            .get("frontmatter_name")
            .filter(|v| crate::python_value::truthy(v))
            .unwrap_or(name);
        // Preserve short-circuit membership order, including malformed cache
        // entries whose names would be unhashable in Python.
        for name in [frontmatter_name, name] {
            if name.is_array() || name.is_object() {
                return Err("unhashable skill name");
            }
            if name
                .as_str()
                .is_some_and(|name| self.disabled.contains(name))
            {
                return Ok(false);
            }
        }
        let empty = json!({});
        let conditions = entry
            .get("conditions")
            .filter(|v| crate::python_value::truthy(v))
            .unwrap_or(&empty);
        skills_index::should_show(conditions, self.tools, self.toolsets, self.platform)
    }
}

pub struct ProfileEntries {
    pub visible: Vec<Value>,
    /// Present only on a cold scan. Write these after project/org merging,
    /// together with category descriptions, as the Python loader does.
    pub scanned: Option<Vec<Value>>,
    pub category_descriptions: serde_json::Map<String, Value>,
}

/// Inputs resolved from the owning profile and the session's captured context.
pub struct Sources<'a> {
    pub home: &'a Path,
    pub skills: &'a Path,
    pub project: &'a [std::path::PathBuf],
    pub external: &'a [std::path::PathBuf],
}

/// Session-owned inputs for config-to-prompt resolution. The project start may
/// be a terminal scope cwd; relative trusted config paths use launch instead.
pub struct SourceContext<'a> {
    pub config_path: &'a Path,
    pub home: &'a Path,
    pub skills: &'a Path,
    pub user_home: &'a Path,
    pub launch: &'a Path,
    pub project_start: &'a Path,
    pub environment: &'a std::collections::BTreeMap<String, String>,
}

#[derive(PartialEq, Eq)]
struct PromptKey {
    skills: std::path::PathBuf,
    external: Vec<std::path::PathBuf>,
    project: Vec<std::path::PathBuf>,
    tools: BTreeSet<String>,
    toolsets: BTreeSet<String>,
    platform: String,
    disabled: BTreeSet<String>,
    compact: BTreeSet<String>,
}

/// Retain this owner across prompt builds. Mutating skills does not alter an
/// already cached prompt; explicit clearing or LRU eviction permits rebuilding.
/// OS/environment checks are intentionally absent from Python's cache key.
#[derive(Default)]
pub struct PromptLoader {
    sources: skill_discovery::SourceCache,
    admission: skill_discovery::ProjectAdmission,
    prompts: std::collections::VecDeque<(PromptKey, String)>,
}

impl PromptLoader {
    /// Resolve profile config and all source tiers before consulting the prompt
    /// LRU. Disabled names come from that same profile config, not another home.
    pub fn render_configured(
        &mut self,
        context: &SourceContext<'_>,
        visibility: &Visibility<'_>,
        compact: &BTreeSet<String>,
        detect: impl FnMut(&str) -> Result<bool, String>,
    ) -> anyhow::Result<String> {
        let expand = |raw: &str| {
            skill_discovery::expand_source_path(raw, context.environment, context.user_home)
        };
        let external =
            self.sources
                .extra_dirs(context.config_path, context.home, context.skills, expand)?;
        let config = self.sources.raw.load(context.config_path);
        let start = context.launch.join(context.project_start);
        let project = skill_discovery::project_dirs(
            &start,
            context.user_home,
            context.skills,
            &config,
            |raw| expand(raw).map(|path| context.launch.join(path)),
        )?;
        let platform = visibility
            .platform
            .filter(|value| !value.is_empty())
            .or_else(|| {
                context
                    .environment
                    .get("HERMES_PLATFORM")
                    .filter(|value| !value.is_empty())
                    .map(String::as_str)
            })
            .or_else(|| {
                context
                    .environment
                    .get("HERMES_SESSION_PLATFORM")
                    .filter(|value| !value.is_empty())
                    .map(String::as_str)
            });
        let disabled = skills_index::disabled_names(&config, platform, None, None)
            .map_err(anyhow::Error::msg)?;
        let visibility = Visibility {
            host: visibility.host,
            termux: visibility.termux,
            platform,
            tools: visibility.tools,
            toolsets: visibility.toolsets,
            disabled: &disabled,
        };
        self.render(
            &Sources {
                home: context.home,
                skills: context.skills,
                project: &project,
                external: &external,
            },
            &visibility,
            compact,
            detect,
        )
    }

    pub fn render(
        &mut self,
        sources: &Sources<'_>,
        visibility: &Visibility<'_>,
        compact: &BTreeSet<String>,
        detect: impl FnMut(&str) -> Result<bool, String>,
    ) -> anyhow::Result<String> {
        // The public Python wrapper checks source availability before its LRU.
        if !sources.skills.exists() && sources.project.is_empty() && sources.external.is_empty() {
            return Ok(String::new());
        }
        let key = PromptKey {
            skills: sources.skills.to_owned(),
            external: sources.external.to_vec(),
            project: sources.project.to_vec(),
            tools: visibility.tools.cloned().unwrap_or_default(),
            toolsets: visibility.toolsets.cloned().unwrap_or_default(),
            platform: visibility.platform.unwrap_or_default().into(),
            disabled: visibility.disabled.clone(),
            compact: compact.clone(),
        };
        if let Some(position) = self.prompts.iter().position(|(cached, _)| cached == &key) {
            let entry = self.prompts.remove(position).unwrap();
            let result = entry.1.clone();
            self.prompts.push_back(entry);
            return Ok(result);
        }
        let result =
            load_index(sources, &mut self.admission, visibility, compact, detect)?.render();
        self.prompts.push_back((key, result.clone()));
        if self.prompts.len() > 32 {
            self.prompts.pop_front();
        }
        Ok(result)
    }

    /// Clearing rendered prompts does not clear project quarantine decisions.
    /// Snapshot removal is best effort and scoped to the explicit profile home.
    pub fn clear(&mut self, home: &Path, clear_snapshot: bool) {
        self.prompts.clear();
        if clear_snapshot {
            if let Err(error) = std::fs::remove_file(home.join(".skills_prompt_snapshot.json")) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!(%error, "Could not remove skills prompt snapshot");
                }
            }
        }
    }
}

/// Assemble a cache-miss load in reference order. The caller owns the rendered
/// prompt cache and keeps admission decisions alive across these loads.
pub fn load_index(
    sources: &Sources<'_>,
    admission: &mut skill_discovery::ProjectAdmission,
    visibility: &Visibility<'_>,
    compact: &BTreeSet<String>,
    mut detect: impl FnMut(&str) -> Result<bool, String>,
) -> anyhow::Result<skills_index::Index> {
    let mut index = skills_index::Index {
        compact: compact.clone(),
        tools: visibility.tools.cloned(),
        ..Default::default()
    };
    if !sources.skills.exists() && sources.project.is_empty() && sources.external.is_empty() {
        return Ok(index);
    }
    let profile = load_profile(sources.home, sources.skills, visibility, &mut detect)?;
    let names = merge_project(
        &mut index,
        sources.project,
        sources.home,
        admission,
        visibility,
        &mut detect,
    )?;
    merge_profile(&mut index, &profile.visible, &names).map_err(anyhow::Error::msg)?;
    finish_profile(sources.home, sources.skills, profile, &mut index)?;
    merge_external(&mut index, sources.external, visibility, &mut detect)?;
    Ok(index)
}

/// Admit and merge trusted project directories before profile and external
/// skills. Only accepted, visible entries claim names and shadow later tiers.
pub fn merge_project(
    index: &mut skills_index::Index,
    directories: &[std::path::PathBuf],
    home: &Path,
    admission: &mut skill_discovery::ProjectAdmission,
    visibility: &Visibility<'_>,
    mut detect: impl FnMut(&str) -> Result<bool, String>,
) -> anyhow::Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for directory in directories {
        if !directory.exists() {
            continue;
        }
        for file in admission.files(directory, home)? {
            let mut read = || -> anyhow::Result<()> {
                let (compatible, metadata, description) = skill_discovery::parse_skill_file(
                    &file,
                    visibility.host,
                    visibility.termux,
                    &mut detect,
                );
                if !compatible {
                    return Ok(());
                }
                let entry =
                    skill_discovery::snapshot_entry(&file, directory, &metadata, description)
                        .map_err(anyhow::Error::msg)?;
                let name = entry["frontmatter_name"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("non-string project name"))?;
                if names.contains(name)
                    || !visibility.permits(&entry).map_err(anyhow::Error::msg)?
                {
                    return Ok(());
                }
                let category = entry["category"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("non-string project category"))?;
                let description = entry["description"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| crate::python_value::python_repr(&entry["description"]));
                let description = format!("[project] {description}")
                    .trim_matches(crate::python_value::python_whitespace)
                    .to_owned();
                names.insert(name.to_owned());
                index
                    .categories
                    .entry(category.to_owned())
                    .or_default()
                    .push((name.to_owned(), description));
                Ok(())
            };
            if let Err(error) = read() {
                tracing::debug!(path = %file.display(), %error, "Error reading project skill");
            }
        }
    }
    Ok(names)
}

/// Merge already-visible profile entries after accepted project skills have
/// claimed their names. Personal/org collisions retain both entries and carry
/// Python's exact prompt labels; neither owner silently wins.
pub fn merge_profile(
    index: &mut skills_index::Index,
    entries: &[Value],
    project_names: &BTreeSet<String>,
) -> Result<(), &'static str> {
    let name = |entry: &Value| -> Result<String, &'static str> {
        for key in ["frontmatter_name", "skill_name"] {
            if let Some(value) = entry.get(key).filter(|v| crate::python_value::truthy(v)) {
                return value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or("non-string visible skill name");
            }
        }
        Ok(String::new())
    };
    let mut owners = std::collections::BTreeMap::<String, u8>::new();
    let mut visible = Vec::new();
    for entry in entries {
        let name = name(entry)?;
        if project_names.contains(&name) {
            continue;
        }
        let org = crate::python_value::truthy(&entry["org_id"]);
        *owners.entry(name.clone()).or_default() |= if org { 2 } else { 1 };
        visible.push((entry, name, org));
    }
    for (entry, name, org) in visible {
        let empty = json!("");
        let raw_description = entry.get("description").unwrap_or(&empty);
        let string = |value: &Value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| crate::python_value::python_repr(value))
        };
        let mut description = if org || owners[&name] == 3 {
            string(raw_description)
        } else {
            raw_description
                .as_str()
                .ok_or("non-string personal skill description")?
                .to_owned()
        };
        let category = if org {
            let author = entry
                .get("org_author")
                .filter(|v| crate::python_value::truthy(v));
            let tag = match author {
                Some(author) => format!(
                    "[org-shared: by {}]",
                    author.as_str().ok_or("non-string org author")?
                ),
                None => "[org-shared]".to_owned(),
            };
            description = format!("{tag} {description}")
                .trim_matches(crate::python_value::python_whitespace)
                .to_owned();
            format!("org:{}", string(&entry["org_id"]))
        } else {
            match entry
                .get("category")
                .filter(|v| crate::python_value::truthy(v))
            {
                Some(category) => category
                    .as_str()
                    .ok_or("non-string personal category")?
                    .to_owned(),
                None => "general".to_owned(),
            }
        };
        if owners[&name] == 3 {
            let other = if org { "personally" } else { "in your org" };
            description = format!("[name collision \u{2014} also exists {other}; load via category path] {description}").trim_matches(crate::python_value::python_whitespace).to_owned();
        }
        index
            .categories
            .entry(category)
            .or_default()
            .push((name, description));
    }
    Ok(())
}

/// Load the owning profile's snapshot or scan its filesystem. Environment
/// relevance is a cold-scan gate in Python; snapshot hits only recheck platform,
/// disabled names and tool conditions. Do not silently add a new warm-path gate.
pub fn load_profile(
    home: &Path,
    skills: &Path,
    visibility: &Visibility<'_>,
    mut detect: impl FnMut(&str) -> Result<bool, String>,
) -> anyhow::Result<ProfileEntries> {
    let mut visible = Vec::new();
    if let Some(snapshot) = skill_discovery::load_snapshot(home, skills)? {
        let empty = json!([]);
        let entries = snapshot.get("skills").unwrap_or(&empty);
        let entries: Vec<Value> = match entries {
            Value::Array(entries) => entries.clone(),
            // Iterating a mapping or string yields non-dict entries, all of
            // which the Python loop skips without coercing them into records.
            Value::Object(_) | Value::String(_) => Vec::new(),
            _ => anyhow::bail!("snapshot skills is not iterable"),
        };
        for entry in entries {
            if !entry.is_object() {
                continue;
            }
            if !skills_index::matches_platform(
                &entry["platforms"],
                visibility.host,
                visibility.termux,
            ) {
                continue;
            }
            if visibility.permits(&entry).map_err(anyhow::Error::msg)? {
                visible.push(entry);
            }
        }
        let category_descriptions = match snapshot
            .get("category_descriptions")
            .filter(|v| crate::python_value::truthy(v))
        {
            Some(Value::Object(values)) => values
                .iter()
                .map(|(key, value)| {
                    let text = value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| crate::python_value::python_repr(value));
                    (key.clone(), Value::String(text))
                })
                .collect(),
            None => Default::default(),
            _ => anyhow::bail!("snapshot category_descriptions is not a mapping"),
        };
        return Ok(ProfileEntries {
            visible,
            scanned: None,
            category_descriptions,
        });
    }
    let mut scanned = Vec::new();
    for file in skill_discovery::index_files(skills, "SKILL.md")? {
        let (compatible, frontmatter, description) = skill_discovery::parse_skill_file(
            &file,
            visibility.host,
            visibility.termux,
            &mut detect,
        );
        let entry = skill_discovery::snapshot_entry(&file, skills, &frontmatter, description)
            .map_err(anyhow::Error::msg)?;
        scanned.push(entry.clone());
        if compatible && visibility.permits(&entry).map_err(anyhow::Error::msg)? {
            visible.push(entry);
        }
    }
    Ok(ProfileEntries {
        visible,
        scanned: Some(scanned),
        category_descriptions: Default::default(),
    })
}

/// Profile descriptions replace entries; external descriptions fill gaps only.
fn load_descriptions(
    skills: &Path,
    index: &mut skills_index::Index,
    overwrite: bool,
) -> anyhow::Result<()> {
    for file in skill_discovery::index_files(skills, "DESCRIPTION.md")? {
        let read = || -> anyhow::Result<Option<(String, String)>> {
            let text = std::fs::read_to_string(&file)?
                .replace("\r\n", "\n")
                .replace('\r', "\n");
            let (metadata, _) = skill_discovery::parse_frontmatter(&text);
            let Some(description) = metadata
                .get("description")
                .filter(|v| crate::python_value::truthy(v))
            else {
                return Ok(None);
            };
            let parent = file.strip_prefix(skills)?.parent().unwrap_or(Path::new(""));
            let category = if parent.as_os_str().is_empty() {
                "general"
            } else {
                parent
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("non-Unicode category path"))?
            };
            let text = description
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| crate::python_value::python_repr(description));
            // Category descriptions are not subject to the per-skill cap.
            let text = text
                .trim_matches(crate::python_value::python_whitespace)
                .trim_matches(['\'', '"'])
                .to_owned();
            Ok(Some((category.to_owned(), text)))
        };
        match read() {
            Ok(Some((category, description))) => {
                if overwrite {
                    index.descriptions.insert(category, description);
                } else {
                    index.descriptions.entry(category).or_insert(description);
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(path = %file.display(), %error, "Could not read skill description")
            }
        }
    }
    Ok(())
}

/// Complete profile ingestion after project/org merging. Only cold scans read
/// category files and write the snapshot; external/project entries never enter
/// the profile cache. Manifest discovery errors retain Python's propagation.
pub fn finish_profile(
    home: &Path,
    skills: &Path,
    profile: ProfileEntries,
    index: &mut skills_index::Index,
) -> anyhow::Result<()> {
    if let Some(entries) = profile.scanned {
        load_descriptions(skills, index, true)?;
        skill_discovery::write_snapshot(
            home,
            &skill_discovery::manifest(skills)?,
            &entries,
            &index.descriptions,
        );
    } else {
        for (category, description) in profile.category_descriptions {
            index.descriptions.insert(
                category,
                description.as_str().unwrap_or_default().to_owned(),
            );
        }
    }
    Ok(())
}

/// External directories are read-only and are scanned in caller order. Names
/// already claimed by project/profile entries win, then the first visible
/// external entry wins. A broken external entry does not abort other entries.
pub fn merge_external(
    index: &mut skills_index::Index,
    directories: &[std::path::PathBuf],
    visibility: &Visibility<'_>,
    mut detect: impl FnMut(&str) -> Result<bool, String>,
) -> anyhow::Result<()> {
    let mut seen: BTreeSet<String> = index
        .categories
        .values()
        .flatten()
        .map(|(name, _)| name.clone())
        .collect();
    for directory in directories {
        if !directory.exists() {
            continue;
        }
        for file in skill_discovery::index_files(directory, "SKILL.md")? {
            let mut read = || -> anyhow::Result<()> {
                let (compatible, metadata, description) = skill_discovery::parse_skill_file(
                    &file,
                    visibility.host,
                    visibility.termux,
                    &mut detect,
                );
                if !compatible {
                    return Ok(());
                }
                let entry =
                    skill_discovery::snapshot_entry(&file, directory, &metadata, description)
                        .map_err(anyhow::Error::msg)?;
                let name = entry["frontmatter_name"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("non-string external name"))?;
                if seen.contains(name) || !visibility.permits(&entry).map_err(anyhow::Error::msg)? {
                    return Ok(());
                }
                let category = entry["category"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("non-string external category"))?;
                let description = entry["description"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("non-string external description"))?;
                seen.insert(name.to_owned());
                index
                    .categories
                    .entry(category.to_owned())
                    .or_default()
                    .push((name.to_owned(), description.to_owned()));
                Ok(())
            };
            if let Err(error) = read() {
                tracing::debug!(path = %file.display(), %error, "Error reading external skill");
            }
        }
        load_descriptions(directory, index, false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_loader_resolves_sources_from_the_owning_profile() {
        let root =
            std::env::temp_dir().join(format!("hermes-configured-loader-{}", std::process::id()));
        let home = root.join("profile");
        let skills = home.join("skills");
        let launch = root.join("launch");
        let project = launch.join("repo");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        for (directory, name) in [
            (&skills, "local"),
            (&home.join("created"), "created"),
            (&home.join("external"), "external"),
            (&project.join(".agents/skills"), "project"),
        ] {
            let path = directory.join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} description\n---\n"),
            )
            .unwrap();
        }
        let config_path = home.join("config.yaml");
        std::fs::write(&config_path, "skills:\n  create_dir: created\n  external_dirs: $EXTRA\n  trusted_project_dirs: repo\n  platform_disabled:\n    telegram: [external]\n").unwrap();
        let environment = std::collections::BTreeMap::from([
            ("EXTRA".into(), "external".into()),
            ("HERMES_PLATFORM".into(), "telegram".into()),
        ]);
        let context = SourceContext {
            config_path: &config_path,
            home: &home,
            skills: &skills,
            user_home: &root,
            launch: &launch,
            project_start: Path::new("repo"),
            environment: &environment,
        };
        let disabled = BTreeSet::new();
        let visibility = Visibility {
            host: "linux",
            termux: false,
            platform: None,
            tools: None,
            toolsets: None,
            disabled: &disabled,
        };
        let mut loader = PromptLoader::default();
        let rendered = loader
            .render_configured(&context, &visibility, &BTreeSet::new(), |_| Ok(true))
            .unwrap();
        for expected in [
            "local description",
            "created description",
            "[project] project description",
        ] {
            assert!(rendered.contains(expected), "{rendered}");
        }
        assert!(!rendered.contains("external description"));
        assert!(home.join(".skills_prompt_snapshot.json").exists());
        assert!(home.join("cache/project_skill_scans").is_dir());
        assert!(!launch.join(".skills_prompt_snapshot.json").exists());
        assert_eq!(
            rendered,
            loader
                .render_configured(&context, &visibility, &BTreeSet::new(), |_| Ok(true))
                .unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prompt_cache_preserves_bytes_and_evicts_least_recently_used() {
        let root = std::env::temp_dir().join(format!("hermes-skills-lru-{}", std::process::id()));
        let skills = root.join("skills");
        std::fs::create_dir_all(skills.join("one")).unwrap();
        let file = skills.join("one/SKILL.md");
        std::fs::write(&file, "---\ndescription: original\n---\n").unwrap();
        let sources = Sources {
            home: &root,
            skills: &skills,
            project: &[],
            external: &[],
        };
        let disabled = BTreeSet::new();
        let compact = BTreeSet::new();
        let mut visibility = Visibility {
            host: "linux",
            termux: false,
            platform: Some("first"),
            tools: None,
            toolsets: None,
            disabled: &disabled,
        };
        let mut loader = PromptLoader::default();
        let original = loader
            .render(&sources, &visibility, &compact, |_| Ok(true))
            .unwrap();
        std::fs::write(&file, "---\ndescription: changed after caching\n---\n").unwrap();
        assert_eq!(
            original,
            loader
                .render(&sources, &visibility, &compact, |_| panic!(
                    "cache hit scanned environment"
                ))
                .unwrap()
        );
        // Fill all slots, then refresh the oldest entry before inserting one more.
        let platforms: Vec<_> = (1..32).map(|i| format!("platform-{i}")).collect();
        for platform in &platforms {
            visibility.platform = Some(platform);
            assert!(loader
                .render(&sources, &visibility, &compact, |_| Ok(true))
                .unwrap()
                .contains("changed after caching"));
        }
        visibility.platform = Some("first");
        assert_eq!(
            original,
            loader
                .render(&sources, &visibility, &compact, |_| Ok(true))
                .unwrap()
        );
        visibility.platform = Some("last");
        loader
            .render(&sources, &visibility, &compact, |_| Ok(true))
            .unwrap();
        assert_eq!(loader.prompts.len(), 32);
        assert!(!loader
            .prompts
            .iter()
            .any(|(key, _)| key.platform == "platform-1"));
        visibility.platform = Some("first");
        assert_eq!(
            original,
            loader
                .render(&sources, &visibility, &compact, |_| Ok(true))
                .unwrap()
        );
        loader.clear(&root, true);
        assert!(!root.join(".skills_prompt_snapshot.json").exists());
        assert!(loader
            .render(&sources, &visibility, &compact, |_| Ok(true))
            .unwrap()
            .contains("changed after caching"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn combined_loader_preserves_tier_precedence_and_snapshot_scope() {
        let root =
            std::env::temp_dir().join(format!("hermes-combined-skills-{}", std::process::id()));
        let home = root.join("home");
        let skills = home.join("skills");
        let project = root.join("project");
        let external = root.join("external");
        for (directory, name, description) in [
            (&skills, "shared", "profile"),
            (&skills, "local", "personal"),
            (&project, "shared", "vendored"),
            (&external, "shared", "external duplicate"),
            (&external, "extra", "external only"),
        ] {
            let path = directory.join("category").join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
            )
            .unwrap();
        }
        std::fs::write(
            skills.join("category/DESCRIPTION.md"),
            "---\ndescription: profile category\n---\n",
        )
        .unwrap();
        std::fs::write(
            external.join("category/DESCRIPTION.md"),
            "---\ndescription: external category\n---\n",
        )
        .unwrap();
        std::fs::write(
            external.join("DESCRIPTION.md"),
            "---\ndescription: external general\n---\n",
        )
        .unwrap();
        let disabled = BTreeSet::new();
        let visibility = Visibility {
            host: "linux",
            termux: false,
            platform: None,
            tools: None,
            toolsets: None,
            disabled: &disabled,
        };
        let projects = [project];
        let externals = [external.clone()];
        let sources = Sources {
            home: &home,
            skills: &skills,
            project: &projects,
            external: &externals,
        };
        let mut admission = skill_discovery::ProjectAdmission::default();
        let cold = load_index(
            &sources,
            &mut admission,
            &visibility,
            &BTreeSet::new(),
            |_| Ok(true),
        )
        .unwrap();
        let entries = &cold.categories["category"];
        assert!(entries.contains(&("shared".into(), "[project] vendored".into())));
        assert_eq!(
            entries.iter().filter(|(name, _)| name == "shared").count(),
            1
        );
        assert!(entries.contains(&("local".into(), "personal".into())));
        assert!(entries.contains(&("extra".into(), "external only".into())));
        assert_eq!(cold.descriptions["category"], "profile category");
        assert_eq!(cold.descriptions["general"], "external general");
        let snapshot: Value = serde_json::from_slice(
            &std::fs::read(home.join(".skills_prompt_snapshot.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot["skills"].as_array().unwrap().len(), 2);
        assert!(snapshot["category_descriptions"].get("general").is_none());
        assert!(!external.join(".skills_prompt_snapshot.json").exists());
        let warm = load_index(
            &sources,
            &mut admission,
            &visibility,
            &BTreeSet::new(),
            |_| Ok(true),
        )
        .unwrap();
        assert_eq!(cold.render(), warm.render());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_merge_scans_before_claiming_profile_names() {
        let root =
            std::env::temp_dir().join(format!("hermes-project-merge-{}", std::process::id()));
        let project = root.join("project");
        for (name, description) in [
            ("accepted", "vendored"),
            ("blocked", "dangerous"),
            ("disabled", "hidden"),
        ] {
            std::fs::create_dir_all(project.join(name)).unwrap();
            std::fs::write(
                project.join(name).join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
            )
            .unwrap();
        }
        std::fs::write(project.join("blocked/payload.exe"), b"binary").unwrap();
        let disabled = BTreeSet::from(["disabled".into()]);
        let visibility = Visibility {
            host: "linux",
            termux: false,
            platform: None,
            tools: None,
            toolsets: None,
            disabled: &disabled,
        };
        let mut index = skills_index::Index::default();
        let names = merge_project(
            &mut index,
            &[project],
            &root.join("home"),
            &mut skill_discovery::ProjectAdmission::default(),
            &visibility,
            |_| Ok(true),
        )
        .unwrap();
        assert_eq!(names, BTreeSet::from(["accepted".into()]));
        assert_eq!(
            index
                .categories
                .values()
                .flatten()
                .cloned()
                .collect::<Vec<_>>(),
            vec![("accepted".into(), "[project] vendored".into())]
        );
        assert!(!root.join("project/.scan-cache").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_merge_and_render_match_python_with_preaccepted_edges() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../tools/skill-merge-goldens.json")).unwrap();
        for case in cases {
            let mut index = skills_index::Index::default();
            let mut project_names = BTreeSet::new();
            // The oracle supplies accepted edge entries, bypassing discovery,
            // quarantine and visibility. Set up those same inputs here; this
            // test covers profile merging and rendering, not edge admission.
            let identity = |entry: &Value| {
                let name = ["frontmatter_name", "skill_name"]
                    .iter()
                    .find_map(|key| {
                        entry
                            .get(key)
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                    })
                    .unwrap_or("")
                    .to_owned();
                let category = entry
                    .get("category")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("general")
                    .to_owned();
                (name, category)
            };
            for entry in case["project_entries"].as_array().unwrap() {
                let (name, category) = identity(entry);
                if project_names.insert(name.clone()) {
                    let description = entry
                        .get("description")
                        .map(|v| {
                            v.as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| crate::python_value::python_repr(v))
                        })
                        .unwrap_or_default();
                    index.categories.entry(category).or_default().push((
                        name,
                        format!("[project] {description}")
                            .trim_matches(crate::python_value::python_whitespace)
                            .to_owned(),
                    ));
                }
            }
            merge_profile(
                &mut index,
                case["visible_entries"].as_array().unwrap(),
                &project_names,
            )
            .unwrap();
            let mut seen: BTreeSet<String> = index
                .categories
                .values()
                .flatten()
                .map(|(name, _)| name.clone())
                .collect();
            for entry in case["external_entries"].as_array().unwrap() {
                let (name, category) = identity(entry);
                if seen.insert(name.clone()) {
                    index.categories.entry(category).or_default().push((
                        name,
                        entry
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                    ));
                }
            }
            index.descriptions =
                serde_json::from_value(case["category_descriptions"].clone()).unwrap();
            index.compact = serde_json::from_value(case["compact_categories"].clone()).unwrap();
            index.tools = serde_json::from_value(case["available_tools"].clone()).unwrap();
            assert_eq!(
                serde_json::to_value(&index.categories).unwrap(),
                case["skills_by_category"],
                "{}",
                case["name"]
            );
            assert_eq!(
                index.render(),
                case["rendered"].as_str().unwrap(),
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn completed_profile_snapshot_excludes_external_entries() {
        let root = std::env::temp_dir().join(format!("hermes-skill-finish-{}", std::process::id()));
        let skills = root.join("skills");
        let external = root.join("external");
        for (dir, name, description) in [
            (&skills, "shared", "local"),
            (&external, "shared", "external"),
            (&external, "unique", "unique"),
        ] {
            let path = dir.join(name).join("SKILL.md");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                format!("---\nname: {name}\ndescription: {description}\n---\nbody"),
            )
            .unwrap();
        }
        let category_description = "category ".repeat(12);
        std::fs::write(
            skills.join("DESCRIPTION.md"),
            format!("---\ndescription: '{category_description}'\n---\n"),
        )
        .unwrap();
        let disabled = BTreeSet::new();
        let visibility = Visibility {
            host: "linux",
            termux: false,
            platform: None,
            tools: None,
            toolsets: None,
            disabled: &disabled,
        };
        let profile = load_profile(&root, &skills, &visibility, |_| Ok(true)).unwrap();
        let mut index = skills_index::Index::default();
        merge_profile(&mut index, &profile.visible, &BTreeSet::new()).unwrap();
        finish_profile(&root, &skills, profile, &mut index).unwrap();
        merge_external(
            &mut index,
            std::slice::from_ref(&external),
            &visibility,
            |_| Ok(true),
        )
        .unwrap();
        assert_eq!(
            index.categories["shared"],
            [("shared".into(), "local".into())]
        );
        assert_eq!(
            index.categories["unique"],
            [("unique".into(), "unique".into())]
        );
        assert_eq!(index.descriptions["general"], category_description.trim());
        let snapshot = skill_discovery::load_snapshot(&root, &skills)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot["skills"].as_array().unwrap().len(), 1);
        assert!(!external.join(".skills_prompt_snapshot.json").exists());
        let warm = load_profile(&root, &skills, &visibility, |_| panic!("warm metadata")).unwrap();
        let mut restored = skills_index::Index::default();
        finish_profile(&root, &skills, warm, &mut restored).unwrap();
        assert_eq!(restored.descriptions, index.descriptions);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_shadowing_precedes_personal_org_collision_labels() {
        let entries = vec![
            json!({"frontmatter_name":"shared", "category":"local", "description":"personal"}),
            json!({"frontmatter_name":"shared", "org_id":"team", "org_author":"device", "description":"organization"}),
        ];
        let mut index = skills_index::Index::default();
        merge_profile(&mut index, &entries, &BTreeSet::new()).unwrap();
        assert_eq!(index.categories["local"], [("shared".into(), "[name collision \u{2014} also exists in your org; load via category path] personal".into())]);
        assert_eq!(index.categories["org:team"], [("shared".into(), "[name collision \u{2014} also exists personally; load via category path] [org-shared: by device] organization".into())]);
        let mut project = skills_index::Index::default();
        project.categories.insert(
            "project".into(),
            vec![("shared".into(), "[project] vendored".into())],
        );
        merge_profile(&mut project, &entries, &BTreeSet::from(["shared".into()])).unwrap();
        assert_eq!(project.categories.len(), 1);
        assert!(!project.render().contains("name collision"));
    }

    #[test]
    fn profile_scan_and_snapshot_apply_their_distinct_filters() {
        let root = std::env::temp_dir().join(format!("hermes-profile-scan-{}", std::process::id()));
        let skills = root.join("skills");
        for (name, metadata) in [
            (
                "container",
                "environments: [docker]\ndescription: container",
            ),
            (
                "conditional",
                "metadata: {hermes: {requires_tools: [terminal]}}",
            ),
            ("hidden", "description: hidden"),
        ] {
            let dir = skills.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), format!("---\n{metadata}\n---\nbody")).unwrap();
        }
        let disabled = BTreeSet::from(["hidden".into()]);
        let tools = BTreeSet::new();
        let visibility = Visibility {
            host: "linux",
            termux: false,
            platform: Some("cli"),
            tools: Some(&tools),
            toolsets: None,
            disabled: &disabled,
        };
        let cold = load_profile(&root, &skills, &visibility, |_| Ok(false)).unwrap();
        assert!(cold.visible.is_empty());
        let entries = cold.scanned.unwrap();
        assert_eq!(entries.len(), 3);
        skill_discovery::write_snapshot(
            &root,
            &skill_discovery::manifest(&skills).unwrap(),
            &entries,
            &Default::default(),
        );
        let warm = load_profile(&root, &skills, &visibility, |_| {
            panic!("warm path must not detect environments")
        })
        .unwrap();
        assert!(warm.scanned.is_none());
        // The cold environment rejection cached an empty description. A warm
        // hit does not reconstruct it from SKILL.md, matching Python's index.
        assert_eq!(warm.visible[0]["description"], "");
        assert_eq!(
            warm.visible
                .iter()
                .map(|e| e["skill_name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["container"]
        );
        // A separate owning home must miss this profile's disk snapshot.
        let isolated =
            load_profile(&root.join("other"), &skills, &visibility, |_| Ok(false)).unwrap();
        assert!(isolated.scanned.is_some());
        assert!(isolated.visible.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
