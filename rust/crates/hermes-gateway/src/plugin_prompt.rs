//! Canonical plugin prompt framing and recovery from persisted prompt bytes.
//! Recovery never invokes plugin code, so resuming cannot change cached context.
#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock,
    },
};

const START: &str = "<!-- hermes-plugin-sections:start -->";
const END: &str = "<!-- hermes-plugin-sections:end -->";

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Section {
    pub id: String,
    pub content: String,
}

/// Capture callback metadata from the owning agent's resolved scope. A missing
/// or invalid context cwd stays empty; it must not become the launch directory.
pub fn session_info(
    attributes: &serde_json::Map<String, serde_json::Value>,
    scope: &crate::runtime_cwd::CwdInputs<'_>,
    agent_home: Option<&std::path::Path>,
    default_root: &std::path::Path,
    ambient_profile: &serde_json::Value,
) -> serde_json::Map<String, serde_json::Value> {
    let text = |value: &serde_json::Value, fallback: &str| {
        if !crate::python_value::truthy(value) {
            fallback.into()
        } else if let Some(text) = value.as_str() {
            text.to_owned()
        } else {
            crate::python_value::python_repr(value)
        }
    };
    let mut info: serde_json::Map<_, _> = ["session_id", "model", "provider", "platform"]
        .into_iter()
        .map(|key| {
            (
                key.into(),
                serde_json::Value::String(text(
                    attributes.get(key).unwrap_or(&serde_json::Value::Null),
                    "",
                )),
            )
        })
        .collect();
    let profile = agent_home
        .map(|home| crate::profile_name::agent_profile_name(home, default_root))
        .unwrap_or_else(|| text(ambient_profile, "default"));
    let cwd = scope
        .context_cwd()
        .ok()
        .flatten()
        .and_then(|path| path.to_str().map(str::to_owned))
        .unwrap_or_default();
    info.insert("profile_name".into(), profile.into());
    info.insert("cwd".into(), cwd.into());
    info
}

type Renderer = dyn Fn(&serde_json::Map<String, serde_json::Value>) -> Result<serde_json::Value, String>
    + Send
    + Sync;

pub enum Content {
    Text(String),
    Callback(Arc<Renderer>),
}

struct Registration {
    owner: String,
    content: Content,
    max_chars: usize,
    active: Arc<AtomicBool>,
}

/// Identity is independent of the section ID. Disposing an old handle after
/// unloading and re-registering the same ID cannot remove the new registration.
pub struct Handle {
    id: String,
    active: Arc<AtomicBool>,
}

impl Handle {
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

#[derive(Default)]
pub struct Registry {
    entries: BTreeMap<String, Registration>,
}

impl Registry {
    pub fn register(
        &mut self,
        owner: &str,
        id: &str,
        content: Content,
        position: &str,
        max_chars: usize,
    ) -> Result<Handle, String> {
        let mut chars = id.bytes();
        let valid_first = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        if !valid_first
            || id.len() > 128
            || !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
        {
            return Err("system prompt section id must be 1-128 lowercase characters using letters, numbers, '.', '_', or '-'".into());
        }
        if position != "after_memory" {
            return Err("system prompt section position must be one of: after_memory".into());
        }
        if !(1..=4_000).contains(&max_chars) {
            return Err("system prompt section max_chars must be between 1 and 4000".into());
        }
        if let Some(existing) = self.entries.get(id) {
            return Err(format!(
                "system prompt section {id:?} is already registered by plugin {:?}",
                existing.owner
            ));
        }
        let active = Arc::new(AtomicBool::new(true));
        self.entries.insert(
            id.into(),
            Registration {
                owner: owner.into(),
                content,
                max_chars,
                active: active.clone(),
            },
        );
        Ok(Handle {
            id: id.into(),
            active,
        })
    }

    pub fn dispose(&mut self, handle: &Handle) {
        if self
            .entries
            .get(&handle.id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.active, &handle.active))
        {
            self.entries.remove(&handle.id);
            handle.active.store(false, Ordering::Release);
        }
    }

    pub fn unload(&mut self, owner: &str) {
        self.entries.retain(|_, entry| {
            if entry.owner == owner {
                entry.active.store(false, Ordering::Release);
                false
            } else {
                true
            }
        });
    }

    pub fn render(
        &self,
        session_info: &serde_json::Map<String, serde_json::Value>,
    ) -> Vec<Section> {
        let limits = self
            .entries
            .iter()
            .map(|(id, entry)| (id.clone(), entry.max_chars))
            .collect();
        render_sections(&limits, |id| match &self.entries[id].content {
            Content::Text(text) => Ok(serde_json::Value::String(text.clone())),
            Content::Callback(callback) => callback(session_info),
        })
    }
}

/// Render validated registrations in ID order. The callback adapter owns the
/// read-only session metadata and reports plugin failures as errors. Evaluation
/// stays lazy so the section-count cap also prevents excess callback execution.
pub fn render_sections<E: std::fmt::Display>(
    registrations: &BTreeMap<String, usize>,
    mut evaluate: impl FnMut(&str) -> Result<serde_json::Value, E>,
) -> Vec<Section> {
    let mut rendered = Vec::new();
    let mut total = START.chars().count() + END.chars().count() + 2;
    for (id, max_chars) in registrations {
        if rendered.len() >= 32 {
            tracing::warn!(id, "Plugin prompt section count exceeded");
            continue;
        }
        let value = match evaluate(id) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(id, %error, "Plugin prompt section failed");
                continue;
            }
        };
        let Some(value) = value.as_str() else {
            tracing::warn!(id, "Plugin prompt section returned a non-string value");
            continue;
        };
        let text = value.trim_matches(crate::python_value::python_whitespace);
        if text.is_empty() {
            continue;
        }
        if text.contains(START) || text.contains(END) || text.chars().count() > *max_chars {
            tracing::warn!(
                id,
                "Plugin prompt section has reserved markers or exceeds its limit"
            );
            continue;
        }
        let section = Section {
            id: id.clone(),
            content: text.into(),
        };
        let mut size = format_section(&section).chars().count();
        if !rendered.is_empty() {
            size += 2;
        }
        if total + size > 8_000 {
            tracing::warn!(id, "Plugin prompt section exceeds aggregate limit");
            continue;
        }
        total += size;
        rendered.push(section);
    }
    rendered
}

/// Per-agent frozen sections. Explicit prompt invalidation permits a new render;
/// ordinary turns and compression reconstruction reuse the captured bytes.
#[derive(Clone, Default)]
pub struct Snapshot {
    current: Option<Vec<Section>>,
    previous: Vec<Section>,
}

impl Snapshot {
    pub fn invalidate(&mut self) {
        if let Some(current) = self.current.take() {
            self.previous = current;
        }
    }

    pub fn restore(&mut self, prompt: &str) {
        self.current = Some(restore(prompt));
    }

    #[cfg(test)]
    pub fn sections(&self) -> Option<&[Section]> {
        self.current.as_deref()
    }

    pub fn get_or_render<E: std::fmt::Display>(
        &mut self,
        stored_prompt: Option<&str>,
        render: impl FnOnce() -> Result<Vec<Section>, E>,
    ) -> &[Section] {
        if self.current.is_none() {
            let sections = if let Some(prompt) = stored_prompt.filter(|p| !p.is_empty()) {
                restore(prompt)
            } else {
                match render() {
                    Ok(sections) => sections,
                    Err(error) => {
                        tracing::warn!(%error, "Plugin prompt render failed; retaining previous sections");
                        self.previous.clone()
                    }
                }
            };
            self.current = Some(sections);
        }
        self.current.as_deref().unwrap()
    }
}

pub fn format(sections: &[Section]) -> String {
    if sections.is_empty() {
        return String::new();
    }
    let blocks: Vec<_> = sections.iter().map(format_section).collect();
    format!("{START}\n{}\n{END}", blocks.join("\n\n"))
}

fn format_section(section: &Section) -> String {
    format!(
        "## Plugin Context: {}\n<!-- hermes-plugin-section-chars:{} -->\n\n{}",
        section.id,
        section.content.chars().count(),
        section.content
    )
}

pub fn restore(prompt: &str) -> Vec<Section> {
    static FRAME: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(
            r"(?m)^## Plugin Context: (?P<id>[a-z0-9][a-z0-9._-]{0,127})\n<!-- hermes-plugin-section-chars:(?P<chars>[0-9]{1,4}) -->\n\n",
        ).expect("constant plugin section expression")
    });
    let Some(start) = prompt.rfind(START) else {
        return Vec::new();
    };
    let Some(end) = prompt[start + START.len()..].find(END) else {
        return Vec::new();
    };
    let after_end = start + START.len() + end + END.len();
    if !prompt[after_end..].starts_with("\n\nConversation started:") {
        return Vec::new();
    }
    let framed = &prompt[start..after_end];
    let mut sections = Vec::new();
    for captures in FRAME.captures_iter(framed) {
        let Ok(captures) = captures else {
            return Vec::new();
        };
        let length: usize = captures.name("chars").unwrap().as_str().parse().unwrap();
        if length > 4_000 {
            continue;
        }
        // Lengths in persisted frames count Python characters, not UTF-8 bytes.
        let content: String = framed[captures.get(0).unwrap().end()..]
            .chars()
            .take(length)
            .collect();
        if content.chars().count() != length {
            continue;
        }
        sections.push(Section {
            id: captures.name("id").unwrap().as_str().into(),
            content,
        });
    }
    // Reformat the entire container to reject partial matches and lookalikes,
    // including a frame-like header embedded inside another section's content.
    if format(&sections) == framed {
        sections
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_uses_agent_home_and_context_cwd_without_ambient_fallback() {
        let root =
            std::env::temp_dir().join(format!("hermes-plugin-metadata-{}", std::process::id()));
        std::fs::create_dir_all(root.join("project")).unwrap();
        let attrs =
            serde_json::json!({"session_id":false,"model":12,"provider":["x"],"platform":" cli "});
        let scope = crate::runtime_cwd::CwdInputs {
            session: Some("missing"),
            terminal: "project",
            launch: &root,
            home: &root,
        };
        for (path, expected) in [
            ("profiles/Red/nested", "Red"),
            ("elsewhere", "default"),
            ("profiles", "default"),
            ("profiles/red", "red"),
        ] {
            let info = session_info(
                attrs.as_object().unwrap(),
                &scope,
                Some(&root.join(path)),
                &root,
                &serde_json::json!("blue"),
            );
            assert_eq!(info["profile_name"], expected);
            assert_eq!(info["cwd"], "");
            assert_eq!(info["session_id"], "");
            assert_eq!(info["model"], "12");
            assert_eq!(info["provider"], "['x']");
            assert_eq!(info["platform"], " cli ");
        }
        let scope = crate::runtime_cwd::CwdInputs {
            session: None,
            ..scope
        };
        let info = session_info(
            attrs.as_object().unwrap(),
            &scope,
            None,
            &root,
            &serde_json::json!("blue"),
        );
        assert_eq!(info["cwd"], "project");
        assert_eq!(info["profile_name"], "blue");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registration_ownership_preserves_other_plugins_and_frozen_sessions() {
        let mut registry = Registry::default();
        let original = registry
            .register(
                "one",
                "notes",
                Content::Text("original".into()),
                "after_memory",
                4000,
            )
            .unwrap();
        assert!(registry
            .register(
                "two",
                "notes",
                Content::Text("replacement".into()),
                "after_memory",
                4000
            )
            .is_err());
        let other = registry
            .register(
                "two",
                "profile",
                Content::Callback(Arc::new(|info| Ok(info["profile"].clone()))),
                "after_memory",
                4000,
            )
            .unwrap();
        let info = serde_json::json!({"profile":"red"})
            .as_object()
            .unwrap()
            .clone();
        let mut snapshot = Snapshot::default();
        let before = snapshot
            .get_or_render(None, || Ok::<_, String>(registry.render(&info)))
            .to_vec();
        let mut prompt = crate::system_prompt::ResolvedPromptSections::default();
        prompt.load_plugin_sections(&mut snapshot, None, &registry, &info);
        assert_eq!(prompt.plugin_sections, [format(&before)]);
        assert_eq!(
            before
                .iter()
                .map(|s| s.content.as_str())
                .collect::<Vec<_>>(),
            ["original", "red"]
        );
        registry.unload("one");
        assert!(!original.active());
        assert!(other.active());
        let replacement = registry
            .register(
                "one",
                "notes",
                Content::Text("new".into()),
                "after_memory",
                4000,
            )
            .unwrap();
        registry.dispose(&original);
        assert!(replacement.active());
        assert_eq!(
            snapshot.get_or_render(None, || Ok::<_, String>(registry.render(&info))),
            before
        );
        snapshot.invalidate();
        assert_eq!(
            snapshot.get_or_render(None, || Ok::<_, String>(registry.render(&info)))[0].content,
            "new"
        );
        registry.dispose(&replacement);
        registry.dispose(&replacement);
        assert!(!replacement.active());
        assert_eq!(registry.render(&info)[0].id, "profile");
    }

    #[test]
    fn registration_validates_before_mutating_registry() {
        let mut registry = Registry::default();
        for id in [
            "",
            "Upper",
            ".leading",
            "has space",
            "slash/id",
            "你好",
            "trailing\n",
        ] {
            assert!(
                registry
                    .register("owner", id, Content::Text("x".into()), "after_memory", 4000)
                    .is_err(),
                "{id:?}"
            );
        }
        assert!(registry
            .register(
                "owner",
                &"a".repeat(129),
                Content::Text("x".into()),
                "after_memory",
                4000
            )
            .is_err());
        for (position, limit) in [
            ("before_memory", 4000),
            ("after_memory", 0),
            ("after_memory", 4001),
        ] {
            assert!(registry
                .register("owner", "valid", Content::Text("x".into()), position, limit)
                .is_err());
        }
        assert!(registry.entries.is_empty());
        for id in ["a", "0", "a.b-c_d", &"a".repeat(128)] {
            assert!(registry
                .register("owner", id, Content::Text("x".into()), "after_memory", 1)
                .is_ok());
        }
    }

    #[test]
    fn restoration_matches_actual_python_fixtures() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../tools/plugin-prompt-goldens.json"))
                .unwrap();
        for (index, case) in cases.iter().enumerate() {
            let expected: Vec<Section> = serde_json::from_value(case["expected"].clone()).unwrap();
            assert_eq!(
                restore(case["prompt"].as_str().unwrap()),
                expected,
                "case {index}"
            );
        }
    }

    #[test]
    fn rendering_budgets_count_frames_and_skip_callbacks_after_count_limit() {
        let registrations = (0..40).map(|i| (format!("s{i:02}"), 4_000)).collect();
        let mut calls = Vec::new();
        let rendered = render_sections(&registrations, |id| {
            calls.push(id.to_owned());
            Ok::<_, &str>(serde_json::json!("\u{001c}你好 \n"))
        });
        assert_eq!(rendered.len(), 32);
        assert_eq!(calls.len(), 32);
        assert_eq!(rendered[0].content, "你好");
        assert_eq!(rendered[31].id, "s31");

        let registrations = ["a", "b", "c", "d", "e", "f", "g", "h"]
            .into_iter()
            .map(|id| (id.into(), 4_000))
            .collect();
        let rendered = render_sections(&registrations, |id| match id {
            "a" => Err("callback failed"),
            "b" => Ok(serde_json::Value::Null),
            "c" => Ok(serde_json::json!(START)),
            "d" => Ok(serde_json::json!("x".repeat(4_001))),
            "e" | "f" => Ok(serde_json::json!("🦀".repeat(4_000))),
            "g" => Ok(serde_json::json!("small")),
            _ => Ok(serde_json::json!(" \n")),
        });
        // The second large section is skipped but later small sections fit.
        assert_eq!(
            rendered.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["e", "g"]
        );
        assert!(format(&rendered).chars().count() <= 8_000);

        let registrations = [("a".into(), 4_000), ("b".into(), 4_000)].into();
        let baseline = vec![
            Section {
                id: "a".into(),
                content: "x".repeat(3_900),
            },
            Section {
                id: "b".into(),
                content: "x".repeat(3_000),
            },
        ];
        let remaining = 3_000 + 8_000 - format(&baseline).chars().count();
        for extra in [0, 1] {
            let rendered = render_sections(&registrations, |id| {
                Ok::<_, &str>(serde_json::json!("x".repeat(if id == "a" {
                    3_900
                } else {
                    remaining + extra
                })))
            });
            if extra == 0 {
                assert_eq!(format(&rendered).chars().count(), 8_000);
                assert_eq!(rendered.len(), 2);
            } else {
                assert_eq!(rendered.len(), 1);
            }
        }
    }

    #[test]
    fn frozen_sections_survive_render_failure_and_resume_never_renders() {
        let original = vec![Section {
            id: "notes".into(),
            content: "first".into(),
        }];
        let mut snapshot = Snapshot::default();
        assert_eq!(
            snapshot.get_or_render(None, || Ok::<_, &str>(original.clone())),
            original
        );
        assert_eq!(
            snapshot.get_or_render::<&str>(None, || panic!("already frozen")),
            original
        );
        snapshot.invalidate();
        snapshot.invalidate();
        assert_eq!(
            snapshot.get_or_render(None, || Err("plugin unavailable")),
            original
        );
        snapshot.invalidate();
        assert!(snapshot
            .get_or_render(None, || Ok::<_, &str>(Vec::new()))
            .is_empty());
        snapshot.invalidate();
        assert!(snapshot
            .get_or_render(None, || Err("plugin unavailable"))
            .is_empty());
        let mut resumed = Snapshot::default();
        assert!(resumed
            .get_or_render::<&str>(Some("legacy prompt"), || panic!(
                "resume must not execute plugins"
            ))
            .is_empty());
        resumed.restore(&format!(
            "{}\n\nConversation started: today",
            format(&original)
        ));
        assert_eq!(
            resumed.get_or_render::<&str>(None, || panic!("restored")),
            original
        );
    }

    #[test]
    fn restores_exact_unicode_bytes_and_rejects_modified_framing() {
        let sections = vec![Section {
            id: "sample.notes".into(),
            content: "你好 🦀\nkeep trailing space ".into(),
        }];
        let block = format(&sections);
        let prompt = format!("stable\n\n{block}\n\nConversation started: Monday");
        assert_eq!(restore(&prompt), sections);
        assert!(restore(&prompt.replace("sample.notes", "Sample.notes")).is_empty());
        assert!(restore(&prompt.replace("Conversation started:", "UTC")).is_empty());
        assert!(restore(&prompt.replace(" -->\n\n", " -->\n")).is_empty());
        assert_eq!(restore(&format!("{START}\nold\n{prompt}")), sections);
        assert!(restore(&format!("{prompt}\n{START}")).is_empty());

        let mut resolved = crate::system_prompt::ResolvedPromptSections {
            stable: vec!["stable".into()],
            memory: Some("memory".into()),
            footer: "Conversation started: Monday".into(),
            ..Default::default()
        };
        resolved.restore_plugin_sections(&prompt);
        let parts = resolved.assemble();
        assert_eq!(parts.stable, "stable");
        assert_eq!(
            parts.volatile,
            format!("memory\n\n{block}\n\nConversation started: Monday")
        );
        let mut resolved = crate::system_prompt::ResolvedPromptSections::default();
        resolved.restore_plugin_sections(&prompt);
        resolved.restore_plugin_sections("unframed");
        assert!(resolved.plugin_sections.is_empty());
    }
}
