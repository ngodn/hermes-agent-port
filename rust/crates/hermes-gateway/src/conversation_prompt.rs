//! Restore or rebuild one conversation's immutable system-prompt bytes.
//! Runtime and Bot Chat probes are computed by the owning initializer.
#![allow(dead_code)] // Resolution metadata is reserved for later lifecycle hooks.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredState {
    Missing,
    Null,
    Empty,
    Present,
    StaleRuntime,
}

pub struct RestoreInputs<'a> {
    pub has_history: bool,
    pub runtime: crate::system_prompt::PromptRuntime<'a>,
    pub capability_stale: bool,
    pub legacy_bot_upgrade: bool,
    pub bot: Option<BotRestoreInputs<'a>>,
}

pub struct BotRestoreInputs<'a> {
    pub enabled: bool,
    pub title_hint: &'a str,
    pub home: &'a std::path::Path,
    pub config: Option<&'a Value>,
    pub protocol: &'a crate::bot_mode::ProtocolCache,
}

#[derive(Default)]
pub struct BuildSnapshot {
    pub row: Option<Value>,
    pub conversation_root: Option<String>,
    pub refresh_capability: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Resolution {
    pub prompt: String,
    pub stored_state: StoredState,
    pub reused: bool,
    pub restore_frozen_sections: bool,
    pub reconstruct_static_prefix: bool,
    pub refreshed_capability: bool,
    pub read_attempted: bool,
    pub persist_attempted: bool,
}

pub trait PromptStore: Sync {
    fn session_row(&self, id: &str) -> anyhow::Result<Option<Value>>;
    fn persist_prompt(&self, id: &str, prompt: &str) -> anyhow::Result<()>;
    fn build_snapshot(&self, _id: &str) -> anyhow::Result<BuildSnapshot> {
        Ok(BuildSnapshot::default())
    }
}

/// Process-stable inputs and retained caches used to build prompts for routed
/// conversations. Per-profile and per-session values remain method inputs.
pub struct Initializer {
    root: PathBuf,
    launch: PathBuf,
    install_root: PathBuf,
    user_home: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    skill_environment: BTreeMap<String, String>,
    skills: Mutex<crate::skill_loader::PromptLoader>,
    environment_probe: tokio::sync::OnceCell<String>,
    timezone: crate::runtime_clock::TimezoneCache,
    protocol: crate::bot_mode::ProtocolCache,
}

pub struct FreshPromptInputs<'a> {
    pub home: &'a Path,
    pub config: &'a Value,
    pub model: &'a str,
    pub provider: &'a str,
    pub platform: &'a str,
    pub session_id: &'a str,
    pub tools: &'a [String],
    pub snapshot: BuildSnapshot,
}

impl Initializer {
    pub fn capture(root: PathBuf, install_root: PathBuf) -> anyhow::Result<Self> {
        let launch = std::env::current_dir()?;
        let environment: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
        let skill_environment = environment
            .iter()
            .filter_map(|(key, value)| Some((key.to_str()?.to_owned(), value.to_str()?.to_owned())))
            .collect();
        let user_home = environment
            .get(OsStr::new("HOME"))
            .or_else(|| environment.get(OsStr::new("USERPROFILE")))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| root.clone());
        Ok(Self {
            root,
            launch,
            install_root,
            user_home,
            environment,
            skill_environment,
            skills: Mutex::new(crate::skill_loader::PromptLoader::default()),
            environment_probe: tokio::sync::OnceCell::new(),
            timezone: crate::runtime_clock::TimezoneCache::default(),
            protocol: crate::bot_mode::ProtocolCache::default(),
        })
    }

    pub fn bot_inputs<'a>(
        &'a self,
        home: &'a Path,
        config: Option<&'a Value>,
    ) -> BotRestoreInputs<'a> {
        BotRestoreInputs {
            enabled: config
                .map(|config| config_flag(&config["agent"]["bot_mode_protocol"], true))
                .unwrap_or(true),
            title_hint: "",
            home,
            config,
            protocol: &self.protocol,
        }
    }

    pub fn runtime_cwd(&self, home: &Path, session: Option<&str>) -> anyhow::Result<String> {
        Ok(crate::runtime_cwd::CwdInputs {
            session,
            terminal: captured_text(&self.environment, "TERMINAL_CWD"),
            launch: &self.launch,
            home,
        }
        .agent_cwd()?
        .to_string_lossy()
        .into_owned())
    }

    pub async fn build_fresh(&self, input: FreshPromptInputs<'_>) -> anyhow::Result<String> {
        if input.snapshot.refresh_capability {
            self.skills
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear(input.home, true);
            self.protocol.section(input.home, true);
        }

        let row = input.snapshot.row.as_ref();
        let session_cwd = row.and_then(|row| row.get("cwd")).and_then(Value::as_str);
        let terminal_cwd = captured_text(&self.environment, "TERMINAL_CWD");
        let scope = crate::runtime_cwd::CwdInputs {
            session: session_cwd,
            terminal: terminal_cwd,
            launch: &self.launch,
            home: input.home,
        };
        let runtime_cwd = scope.agent_cwd()?;
        let runtime_cwd_text = runtime_cwd.to_string_lossy().into_owned();
        let config_path = input.home.join("config.yaml");
        let skills_path = input.home.join("skills");
        let project_start = scope.context_cwd()?.unwrap_or_else(|| PathBuf::from("."));

        let tool_names: BTreeSet<String> = input.tools.iter().cloned().collect();
        let toolsets = configured_strings(&input.config["tools"]["enabled_toolsets"]);
        let disabled = BTreeSet::new();
        let visibility = crate::skill_loader::Visibility {
            host: std::env::consts::OS,
            termux: self.environment.contains_key(OsStr::new("TERMUX_VERSION")),
            platform: Some(input.platform),
            tools: Some(&tool_names),
            toolsets: Some(&toolsets),
            disabled: &disabled,
        };
        let compact = if tool_names
            .iter()
            .any(|tool| matches!(tool.as_str(), "skills_list" | "skill_view" | "skill_manage"))
        {
            crate::coding_context::RuntimeMode::from_scope(
                input.platform,
                None,
                &scope,
                Some(&std::env::temp_dir()),
                input.config,
                Some(input.model),
            )
            .map(|mode| {
                mode.profile()
                    .compact_skill_categories
                    .iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
        } else {
            BTreeSet::new()
        };
        let source_context = crate::skill_loader::SourceContext {
            config_path: &config_path,
            home: input.home,
            skills: &skills_path,
            user_home: &self.user_home,
            launch: &self.launch,
            project_start: &project_start,
            environment: &self.skill_environment,
        };
        let mut sections = crate::system_prompt::ResolvedPromptSections::default();
        sections.load_skills(
            &mut self
                .skills
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            &source_context,
            &visibility,
            &compact,
            |program| Ok(command_available(program, &self.environment)),
        )?;

        let (context_limit, read_timeout) = crate::system_prompt::context_file_limits(
            input.config,
            &input.config["model"]["context_length"],
        );
        let mut context_request = crate::context_files::ContextRequest {
            cwd: None,
            launch_cwd: &self.launch,
            install_root: &self.install_root,
            home: input.home,
            skip_soul: false,
            allow_install_tree_fallback: false,
            max_chars: context_limit,
            read_timeout: std::time::Duration::from_secs_f64(read_timeout),
        };
        let settings = stable_settings(input.config);
        let (soul_loaded, identity_warnings) = sections
            .initialize_stable(
                &context_request,
                false,
                false,
                crate::system_prompt::StableGuidance {
                    soul: None,
                    tools: input.tools,
                    model: input.model,
                    skills_index: "",
                    settings: &settings,
                },
            )
            .await;
        for warning in identity_warnings {
            tracing::warn!(session_id = input.session_id, %warning, "Prompt context was truncated");
        }

        let environment = environment_inputs(
            &self.environment,
            input.config,
            &self.user_home,
            &runtime_cwd_text,
        );
        sections
            .load_runtime_guidance(&crate::system_prompt::RuntimeGuidance {
                provider: input.provider,
                model: input.model,
                platform: input.platform,
                tools: input.tools,
                config: input.config,
                scope: &scope,
                temp_root: Some(&std::env::temp_dir()),
                env: &self.environment,
                environment: &environment,
            })
            .await;

        let backend = captured_text(&self.environment, "TERMINAL_ENV");
        if config_flag(&input.config["agent"]["environment_probe"], true)
            && !crate::environment_prompt::is_known_remote_backend(backend)
        {
            let probe = self
                .environment_probe
                .get_or_init(|| crate::environment_probe::line(&self.environment))
                .await;
            if !probe.is_empty() {
                sections.post_workspace.push(probe.clone());
            }
        }

        let stored_title = row
            .and_then(|row| row.get("display_name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let timeless = sections.append_bot_chat(
            &self.protocol,
            &crate::system_prompt::BotChatGuidance {
                enabled: config_flag(&input.config["agent"]["bot_mode_protocol"], true),
                title_hint: "",
                stored_title,
                home: input.home,
                config: Some(input.config),
            },
        );
        sections.append_profile_platform(&crate::system_prompt::ProfilePlatformGuidance {
            home: input.home,
            root: &self.root,
            platform: input.platform,
            plugin_hint: "",
            config: input.config,
            overrides: &input.config["platform_hints"],
            desktop_terminal: optional_captured_text(&self.environment, "HERMES_DESKTOP_TERMINAL"),
        });

        context_request.skip_soul = soul_loaded;
        let launch_artifact = row
            .and_then(|row| row.get("model_config"))
            .and_then(|value| value.get("_context_cwd_is_launch_artifact"))
            .is_some_and(crate::python_value::truthy);
        for warning in sections
            .load_context(
                &scope,
                context_request,
                input.platform,
                launch_artifact,
                false,
            )
            .await?
        {
            tracing::warn!(session_id = input.session_id, %warning, "Prompt context was truncated");
        }

        let memory = &input.config["memory"];
        let memory_enabled = config_flag(&memory["memory_enabled"], true);
        let user_enabled = config_flag(&memory["user_profile_enabled"], true);
        if memory_enabled || user_enabled {
            match crate::memory_snapshot::MemorySnapshot::load(
                input.home,
                integer_setting(&memory["memory_char_limit"], 2_200),
                integer_setting(&memory["user_char_limit"], 1_375),
            ) {
                Ok(snapshot) => {
                    sections.set_memory_snapshot(Some(&snapshot), memory_enabled, user_enabled)
                }
                Err(error) => tracing::warn!(%error, "Could not load prompt memory snapshot"),
            }
        }

        let creation = row
            .and_then(|row| row.get("started_at"))
            .and_then(Value::as_f64)
            .and_then(|seconds| chrono::DateTime::from_timestamp_millis((seconds * 1_000.0) as i64))
            .map(|stamp| crate::prompt_footer::SessionStart::Aware(stamp.fixed_offset()));
        let mut footer = crate::prompt_footer::Footer {
            now_date: chrono::Utc::now().date_naive(),
            start_date: chrono::Utc::now().date_naive(),
            iana: None,
            abbreviation: String::new(),
            offset: String::new(),
            timeless,
            pass_session_id: false,
            session_id: input.session_id.to_owned(),
            model: input.model.to_owned(),
            provider: input.provider.to_owned(),
            platform: input.platform.to_owned(),
        };
        let timezone = self.timezone.resolve(
            captured_text(&self.environment, "HERMES_TIMEZONE"),
            &config_path,
            input.config,
        );
        crate::runtime_clock::ClockSnapshot::capture(timezone).apply_with_root(
            &mut footer,
            input.snapshot.conversation_root.as_deref(),
            creation,
        );
        sections.set_footer(&footer);
        Ok(sections.assemble().joined())
    }
}

fn captured_text<'a>(environment: &'a BTreeMap<OsString, OsString>, key: &str) -> &'a str {
    optional_captured_text(environment, key).unwrap_or_default()
}

fn optional_captured_text<'a>(
    environment: &'a BTreeMap<OsString, OsString>,
    key: &str,
) -> Option<&'a str> {
    environment
        .get(OsStr::new(key))
        .and_then(|value| value.to_str())
}

fn configured_strings(value: &Value) -> BTreeSet<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn config_flag(value: &Value, default: bool) -> bool {
    crate::config_schema::coerce_bool(value, default)
}

fn integer_setting(value: &Value, default: i64) -> i64 {
    match value {
        Value::Bool(value) => i64::from(*value),
        Value::Number(value) => value.as_i64().unwrap_or(default),
        _ => default,
    }
}

fn stable_settings(config: &Value) -> Value {
    let agent = &config["agent"];
    serde_json::json!({
        "_task_completion_guidance": agent.get("task_completion_guidance").map(crate::python_value::truthy).unwrap_or(true),
        "_parallel_tool_call_guidance": agent.get("parallel_tool_call_guidance").map(crate::python_value::truthy).unwrap_or(true),
        "_memory_enabled": config_flag(&config["memory"]["memory_enabled"], true),
        "_user_profile_enabled": config_flag(&config["memory"]["user_profile_enabled"], true),
        "_tool_use_enforcement": agent.get("tool_use_enforcement").cloned().unwrap_or_else(|| Value::String("auto".into())),
        "_execution_guidance": agent.get("execution_guidance").cloned().unwrap_or_else(|| Value::String("auto".into())),
    })
}

fn command_available(program: &str, environment: &BTreeMap<OsString, OsString>) -> bool {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return path.is_file();
    }
    environment
        .get(OsStr::new("PATH"))
        .into_iter()
        .flat_map(std::env::split_paths)
        .any(|directory| directory.join(program).is_file())
}

fn environment_inputs(
    environment: &BTreeMap<OsString, OsString>,
    config: &Value,
    user_home: &Path,
    cwd: &str,
) -> crate::environment_prompt::EnvironmentPromptInputs {
    let backend = captured_text(environment, "TERMINAL_ENV");
    let extra = crate::environment_prompt::resolve_extra_hint(
        optional_captured_text(environment, "HERMES_ENVIRONMENT_HINT"),
        config,
    );
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .trim()
        .to_owned();
    let wsl = release.to_lowercase().contains("microsoft");
    let local_platform = if wsl {
        crate::environment_prompt::LocalHostPlatform::Wsl
    } else {
        crate::environment_prompt::LocalHostPlatform::Other {
            system: match std::env::consts::OS {
                "linux" => "Linux",
                "macos" => "Darwin",
                other => other,
            }
            .to_owned(),
            release,
        }
    };
    let mut inputs = crate::environment_prompt::EnvironmentPromptInputs {
        backend: Some(if backend.is_empty() { "local" } else { backend }.to_owned()),
        local_host: Some(crate::environment_prompt::LocalHostInfo::new(
            local_platform,
            user_home.to_string_lossy(),
            Some(cwd),
        )),
        is_wsl: wsl,
        extra_hint: extra,
        ..Default::default()
    };
    if crate::environment_prompt::is_known_remote_backend(backend) {
        inputs.is_remote = Some(true);
        inputs.local_host = None;
    }
    inputs
}

impl PromptStore for crate::session_db::SessionDb {
    fn session_row(&self, id: &str) -> anyhow::Result<Option<Value>> {
        Ok(self.get_session(id)?)
    }

    fn persist_prompt(&self, id: &str, prompt: &str) -> anyhow::Result<()> {
        Ok(self.update_system_prompt(id, Some(prompt))?)
    }

    fn build_snapshot(&self, id: &str) -> anyhow::Result<BuildSnapshot> {
        Ok(BuildSnapshot {
            row: self.get_session(id)?,
            conversation_root: self
                .get_conversation_root(id)
                .ok()
                .filter(|root| !root.is_empty()),
            refresh_capability: false,
        })
    }
}

/// Read and write calls each complete before prompt construction or later model
/// I/O. The existing conversation lease supplies same-process serialization.
pub async fn restore_or_build<F, Fut>(
    store: Option<&dyn PromptStore>,
    session_id: &str,
    input: &RestoreInputs<'_>,
    build: F,
) -> anyhow::Result<Resolution>
where
    F: FnOnce(BuildSnapshot) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<String>>,
{
    let mut read_attempted = false;
    let row = if input.has_history {
        store.and_then(|store| {
            read_attempted = true;
            match store.session_row(session_id) {
                Ok(row) => row,
                Err(error) => {
                    tracing::warn!(%error, session_id, "Session DB get_session failed for system-prompt restore; rebuilding");
                    None
                }
            }
        })
    } else {
        None
    };
    let raw = row.as_ref().and_then(|row| row.get("system_prompt"));
    let mut state = match raw {
        None => StoredState::Missing,
        Some(Value::Null) => StoredState::Null,
        Some(Value::String(value)) if value.is_empty() => StoredState::Empty,
        Some(Value::String(_)) => StoredState::Present,
        // Python accepts any truthy object here, then its string operations
        // fail. SQLite's system_prompt column can only produce text or null.
        Some(_) => StoredState::Present,
    };
    let mut capability_stale = input.capability_stale;
    let mut legacy_bot_upgrade = input.legacy_bot_upgrade;
    if let Some(stored) = raw
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        if crate::system_prompt::stored_prompt_matches_runtime(stored, &input.runtime) {
            if let Some(bot) = &input.bot {
                if stored.contains(crate::bot_mode::EPOCH_PREFIX) {
                    let fingerprint = crate::bot_mode::capability_fingerprint(bot.home, bot.config);
                    capability_stale |=
                        crate::bot_mode::stored_prompt_capability_stale(stored, Some(&fingerprint));
                }
                let stored_title = row
                    .as_ref()
                    .and_then(|row| row.get("display_name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let title_hint = bot
                    .title_hint
                    .trim_matches(crate::python_value::python_whitespace);
                let title = if title_hint.is_empty() {
                    stored_title.trim_matches(crate::python_value::python_whitespace)
                } else {
                    title_hint
                };
                legacy_bot_upgrade |= !capability_stale
                    && bot.enabled
                    && title == crate::bot_mode::BOT_CHAT_TITLE
                    && bot.protocol.stored_prompt_needs_upgrade(stored, bot.home);
            }
            if !capability_stale && !legacy_bot_upgrade {
                return Ok(Resolution {
                    prompt: stored.to_owned(),
                    stored_state: state,
                    reused: true,
                    restore_frozen_sections: true,
                    reconstruct_static_prefix: true,
                    refreshed_capability: false,
                    read_attempted,
                    persist_attempted: false,
                });
            }
        } else {
            state = StoredState::StaleRuntime;
        }
    }
    if input.has_history && matches!(state, StoredState::Null | StoredState::Empty) {
        tracing::warn!(session_id, stored_state = ?state, "Stored system prompt is unusable; rebuilding");
    }
    let mut snapshot = match store {
        Some(store) => store.build_snapshot(session_id).unwrap_or_else(|error| {
            tracing::warn!(%error, session_id, "Session DB prompt metadata read failed; using empty build snapshot");
            BuildSnapshot::default()
        }),
        None => BuildSnapshot::default(),
    };
    snapshot.refresh_capability = capability_stale || legacy_bot_upgrade;
    let prompt = build(snapshot).await?;
    let mut persist_attempted = false;
    if let Some(store) = store {
        persist_attempted = true;
        if let Err(error) = store.persist_prompt(session_id, &prompt) {
            tracing::warn!(%error, session_id, "Session DB update_system_prompt failed; later turns may rebuild");
        }
    }
    Ok(Resolution {
        prompt,
        stored_state: state,
        reused: false,
        restore_frozen_sections: false,
        reconstruct_static_prefix: false,
        refreshed_capability: capability_stale || legacy_bot_upgrade,
        read_attempted,
        persist_attempted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Store {
        row: Option<Value>,
        reads: Mutex<usize>,
        writes: Mutex<Vec<(String, String)>>,
    }

    impl PromptStore for Store {
        fn session_row(&self, _id: &str) -> anyhow::Result<Option<Value>> {
            *self.reads.lock().unwrap() += 1;
            Ok(self.row.clone())
        }

        fn persist_prompt(&self, id: &str, prompt: &str) -> anyhow::Result<()> {
            self.writes.lock().unwrap().push((id.into(), prompt.into()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn decisions_match_actual_python_restore_helper() {
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../../tools/conversation-prompt-goldens.json"
        ))
        .unwrap();
        for case in cases {
            let store = Store {
                row: case.get("row").cloned().filter(|value| !value.is_null()),
                reads: Mutex::new(0),
                writes: Mutex::new(Vec::new()),
            };
            let runtime = crate::system_prompt::PromptRuntime {
                model: case
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("model-a"),
                provider: case
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or("provider-a"),
                platform: case
                    .get("platform")
                    .and_then(Value::as_str)
                    .unwrap_or("cli"),
                cwd: case.get("cwd").and_then(Value::as_str).unwrap_or("/work"),
            };
            let expected = &case["expected"];
            let result = restore_or_build(
                Some(&store),
                "session-1",
                &RestoreInputs {
                    has_history: case["history"].as_bool().unwrap(),
                    runtime,
                    capability_stale: case
                        .get("capability_stale")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    legacy_bot_upgrade: case
                        .get("legacy_upgrade")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        && case["row"]["title"].as_str() == Some(crate::bot_mode::BOT_CHAT_TITLE),
                    bot: None,
                },
                |_| async { Ok("fresh prompt".into()) },
            )
            .await
            .unwrap();
            assert_eq!(
                result.prompt,
                expected["prompt"].as_str().unwrap(),
                "{}",
                case["name"]
            );
            assert_eq!(
                *store.reads.lock().unwrap(),
                expected["reads"].as_u64().unwrap() as usize,
                "{}",
                case["name"]
            );
            let writes: Vec<Value> = store
                .writes
                .lock()
                .unwrap()
                .iter()
                .map(|(id, prompt)| serde_json::json!([id, prompt]))
                .collect();
            assert_eq!(
                serde_json::json!(writes),
                expected["writes"],
                "{}",
                case["name"]
            );
            assert_eq!(
                result.reused,
                expected["builds"].as_array().unwrap().is_empty(),
                "{}",
                case["name"]
            );
            assert_eq!(
                result.restore_frozen_sections,
                !expected["restored"].as_array().unwrap().is_empty()
            );
            assert_eq!(
                result.reconstruct_static_prefix,
                !expected["reconstructed"].as_array().unwrap().is_empty()
            );
        }
    }
}
