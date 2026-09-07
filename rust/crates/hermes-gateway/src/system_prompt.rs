//! Conversation prompt assembly and stored-prefix compatibility.
//! The runtime guard follows agent/conversation_loop.py; the native gateway
//! assembles these tiers once while initializing each routed conversation.
#![allow(dead_code)]

fn catalog() -> &'static serde_json::Value {
    static DATA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str(include_str!("../../../tools/system-prompt-guidance.json"))
            .expect("verified prompt guidance catalog")
    })
}

fn guidance(name: &str) -> &'static str {
    catalog()[name].as_str().expect("guidance text")
}

/// Render already-resolved profile identity. The home is the profile directory
/// itself; appending profiles/name again would point at the wrong data.
pub fn profile_hint(profile: &str, home: &str, root: &str) -> String {
    if profile == "default" {
        format!("Active Hermes profile: default. Other profiles (if any) live under {root}/profiles/<name>/. Each profile has its own skills/, plugins/, cron/, and memories/ that affect a different session than this one. Do not modify another profile's skills/plugins/cron/memories unless the user explicitly directs you to.")
    } else {
        format!("Active Hermes profile: {profile}. This session reads and writes {home}/. The default profile's data lives at {root}/skills/, {root}/plugins/, {root}/cron/, {root}/memories/ \u{2014} those belong to a different session run from a different shell. Do NOT modify another profile's skills/plugins/cron/memories unless the user explicitly directs you to.")
    }
}

/// Alibaba may report a different model name in API responses. Freeze the
/// configured identity in the stable tier, exactly as the Python workaround.
pub fn provider_identity(provider: &str, model: &str) -> Option<String> {
    (provider == "alibaba").then(|| {
        let short = model.rsplit('/').next().unwrap_or(model);
        format!("You are powered by the model named {short}. The exact model ID is {model}. When asked what model you are, always answer based on this information, not on any model name returned by the API.")
    })
}

/// Resolve context read settings from a profile's config snapshot. Python
/// treats booleans as numbers here, and does not parse numeric strings.
pub fn context_file_limits(
    config: &serde_json::Value,
    context_length: &serde_json::Value,
) -> (usize, f64) {
    let numeric = |value: &serde_json::Value| match value {
        serde_json::Value::Bool(value) => Some(u8::from(*value) as f64),
        _ => value.as_f64(),
    };
    let positive = |value: &serde_json::Value| numeric(value).filter(|value| *value > 0.0);
    let dynamic = match context_length {
        serde_json::Value::Bool(_) => 20_000,
        serde_json::Value::Number(value) if value.is_i64() || value.is_u64() => {
            ((value.as_f64().unwrap_or(0.0) * 4.0 * 0.06) as usize).clamp(20_000, 500_000)
        }
        _ => 20_000,
    };
    let limit = positive(&config["context_file_max_chars"])
        .filter(|value| value.is_finite())
        .map(|value| value as usize)
        .unwrap_or(dynamic);
    let timeout = positive(&config["context_file_read_timeout"]).unwrap_or(5.0);
    (limit, timeout)
}

/// A single leading BOM is an encoding artifact. Scan all remaining text with
/// the context scope, preserving the shared scanner's finding order.
pub fn scan_context_content(content: &str, filename: &str) -> String {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let findings = crate::threat_patterns::scan_for_threats(content, "context");
    if findings.is_empty() {
        content.into()
    } else {
        let findings = findings.join(", ");
        tracing::warn!("Context file {filename} blocked: {findings}");
        format!("[BLOCKED: {filename} contained potential prompt injection ({findings}). Content not loaded.]")
    }
}

/// Read from the agent's explicit home. A detached reader lets startup continue
/// when a network filesystem stalls, without leaving a Tokio blocking task
/// that would hold runtime shutdown open. The reader exits when I/O returns.
pub async fn load_soul(
    home: &std::path::Path,
    max_chars: usize,
    timeout: std::time::Duration,
) -> (Option<String>, Option<String>) {
    let path = home.join("SOUL.md");
    let Some(text) = crate::context_files::read_text(&path, timeout).await else {
        return (None, None);
    };
    let text = scan_context_content(&text, "SOUL.md");
    let (text, warning) = truncate_context_content(&text, "SOUL.md", max_chars, path.to_str());
    (Some(text), warning)
}

/// Keep Python's character-based head/tail slices, including its zero-tail
/// behavior (content[-0:] is the entire string). Return warnings to the owning
/// prompt build so concurrent sessions cannot drain each other's warnings.
pub fn truncate_context_content(
    content: &str,
    filename: &str,
    max_chars: usize,
    read_path: Option<&str>,
) -> (String, Option<String>) {
    let count = content.chars().count();
    if count <= max_chars {
        return (content.into(), None);
    }
    let head_chars = (max_chars as f64 * 0.7) as usize;
    let tail_chars = (max_chars as f64 * 0.2) as usize;
    let head: String = content.chars().take(head_chars).collect();
    let tail: String = content
        .chars()
        .skip(if tail_chars == 0 {
            0
        } else {
            count - tail_chars
        })
        .collect();
    let target = read_path
        .filter(|path| !path.is_empty())
        .unwrap_or(filename);
    let warning = format!("⚠️  Context file {filename} TRUNCATED: {count} chars exceeds limit of {max_chars} \u{2014} trim the file, pin a larger context_file_max_chars, or use a larger-context model!");
    let marker = format!("\n\n[...truncated {filename}: kept {head_chars}+{tail_chars} of {count} chars. The middle is omitted \u{2014} if you need the full instructions, read the complete file with the read_file tool: {target}]\n\n");
    (head + &marker + &tail, Some(warning))
}

/// Resolve built-in/plugin guidance and Telegram's opt-in extension before
/// applying the user's override. Config is a snapshot from this agent's home.
/// The desktop-terminal flag is captured from the session's environment once
/// at prompt build, not read from mutable process state here.
pub fn platform_hint(
    platform: &str,
    plugin_hint: &str,
    config: &serde_json::Value,
    overrides: &serde_json::Value,
    desktop_terminal: Option<&str>,
) -> String {
    let key = platform.to_lowercase();
    let key = key.trim_matches(crate::python_value::python_whitespace);
    let mut default = catalog()["PLATFORM_HINTS"]
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or(if key.is_empty() { "" } else { plugin_hint })
        .to_owned();
    if key == "telegram" && !default.is_empty() {
        // Python falls back to the base hint if either config traversal fails.
        // A malformed leaf extra is treated as {}, but a truthy non-map parent
        // raises during .get and prevents the entire extension.
        let extra = |path: &[&str]| -> Result<&serde_json::Value, ()> {
            let mut value = config;
            for name in path {
                if !crate::python_value::truthy(value) {
                    return Ok(&serde_json::Value::Null);
                }
                value = value
                    .as_object()
                    .ok_or(())?
                    .get(*name)
                    .unwrap_or(&serde_json::Value::Null);
            }
            Ok(value)
        };
        if let (Ok(gateway), Ok(top)) = (
            extra(&["gateway", "platforms", "telegram", "extra"]),
            extra(&["platforms", "telegram", "extra"]),
        ) {
            let rich = top
                .as_object()
                .and_then(|map| map.get("rich_messages"))
                .or_else(|| gateway.as_object().and_then(|map| map.get("rich_messages")));
            if rich.is_some_and(crate::python_value::truthy) {
                default = format!(
                    "{} {}",
                    default.trim_end_matches(crate::python_value::python_whitespace),
                    guidance("TELEGRAM_RICH_MESSAGES_HINT")
                );
            }
        }
    }
    let mut hint = resolve_platform_hint(key, &default, overrides);
    const PANE: &str = " You're in its embedded terminal pane, beside the GUI chat \u{2014} the user can select your output (Option-drag on macOS, Shift-drag elsewhere) and press Cmd/Ctrl+L to send it to the chat composer.";
    let embedded = desktop_terminal.is_some_and(|value| {
        ["1", "true", "yes", "on"].contains(
            &value
                .trim_matches(crate::python_value::python_whitespace)
                .to_lowercase()
                .as_str(),
        )
    });
    if key == "tui" && embedded && !hint.is_empty() && !hint.contains(PANE) {
        hint.push_str(PANE);
    }
    hint
}

/// Apply the resolved platform's override without changing other prompt tiers.
/// Python applies append after replacement when both are supplied, despite
/// its docstring describing replacement as taking precedence over append.
pub fn resolve_platform_hint(
    platform: &str,
    default: &str,
    overrides: &serde_json::Value,
) -> String {
    let trim = |text: &str| {
        text.trim_matches(crate::python_value::python_whitespace)
            .to_owned()
    };
    let Some(spec) = overrides.as_object().and_then(|map| map.get(platform)) else {
        return default.into();
    };
    if platform.is_empty() {
        return default.into();
    }
    let append = |base: &str, extra: &str| {
        let extra = trim(extra);
        if extra.is_empty() {
            base.to_owned()
        } else {
            trim(&format!("{base}\n\n{extra}"))
        }
    };
    if let Some(extra) = spec.as_str() {
        return append(default, extra);
    }
    let Some(spec) = spec.as_object() else {
        return default.into();
    };
    let replacement = spec
        .get("replace")
        .and_then(serde_json::Value::as_str)
        .map(trim);
    let base = replacement
        .as_deref()
        .filter(|text| !text.is_empty())
        .unwrap_or(default);
    spec.get("append")
        .and_then(serde_json::Value::as_str)
        .map(|extra| append(base, extra))
        .unwrap_or_else(|| base.to_owned())
}

/// Inputs already resolved by the agent's initialization. Settings use the
/// Python agent attribute names; this is not a raw config.yaml reader.
pub struct StableGuidance<'a> {
    pub soul: Option<&'a str>,
    pub tools: &'a [String],
    pub model: &'a str,
    pub skills_index: &'a str,
    pub settings: &'a serde_json::Value,
}

/// Initial stable sections through execution guidance. Environment, coding,
/// platform and profile sections are appended by the remaining assembler.
pub fn stable_guidance(input: &StableGuidance<'_>) -> Vec<String> {
    let has = |name: &str| input.tools.iter().any(|tool| tool == name);
    let enabled = |name: &str| {
        input
            .settings
            .get(name)
            .map(crate::python_value::truthy)
            .unwrap_or(true)
    };
    let mut parts = vec![input
        .soul
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| guidance("DEFAULT_AGENT_IDENTITY"))
        .to_owned()];
    parts.push(
        guidance(
            if has("skill_view") && input.skills_index.contains("- hermes-agent:") {
                "HERMES_AGENT_HELP_GUIDANCE"
            } else {
                "HERMES_AGENT_HELP_GUIDANCE_NO_SKILLS"
            },
        )
        .into(),
    );
    if !input.tools.is_empty() {
        if enabled("_task_completion_guidance") {
            parts.push(guidance("TASK_COMPLETION_GUIDANCE").into());
        }
        if enabled("_parallel_tool_call_guidance") {
            parts.push(guidance("PARALLEL_TOOL_CALL_GUIDANCE").into());
        }
    }
    let mut tool_parts = Vec::new();
    if has("memory") {
        if enabled("_memory_enabled") {
            tool_parts.push(guidance("MEMORY_GUIDANCE"));
        } else if enabled("_user_profile_enabled") {
            tool_parts.push(guidance("USER_PROFILE_GUIDANCE"));
        }
    }
    if has("session_search") {
        tool_parts.push(guidance("SESSION_SEARCH_GUIDANCE"));
    }
    if has("skill_manage") {
        tool_parts.push(guidance("SKILLS_GUIDANCE"));
    }
    let kanban = &input.settings["_kanban_worker_guidance"];
    if crate::python_value::truthy(kanban) {
        tool_parts.push(
            kanban
                .as_str()
                .expect("resolved kanban guidance must be text"),
        );
    } else if kanban.is_null() && has("kanban_show") {
        tool_parts.push(guidance("KANBAN_GUIDANCE"));
    }
    if !tool_parts.is_empty() {
        parts.push(tool_parts.join(" "));
    }
    if !input.tools.is_empty() {
        parts.push(guidance("STEER_CHANNEL_NOTE").into());
        let gate = |setting: &str, defaults: &str| {
            let patterns: Vec<_> = catalog()[defaults]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect();
            model_guidance_enabled(&input.settings[setting], input.model, &patterns)
        };
        if gate("_tool_use_enforcement", "TOOL_USE_ENFORCEMENT_MODELS") {
            parts.push(guidance("TOOL_USE_ENFORCEMENT_GUIDANCE").into());
            let model = input.model.to_lowercase();
            if model.contains("gemini") || model.contains("gemma") {
                parts.push(guidance("GOOGLE_MODEL_OPERATIONAL_GUIDANCE").into());
            }
        }
        if gate("_execution_guidance", "EXECUTION_GUIDANCE_MODELS") {
            let mut text = guidance("OPENAI_MODEL_EXECUTION_GUIDANCE").to_owned();
            if !has("web_search") {
                text = text
                    .replace(
                        "- Current facts (weather, news, versions) → use web_search\n",
                        "",
                    )
                    .replace(
                        "(search_files, web_search, read_file, etc.)",
                        "(search_files, read_file, etc.)",
                    );
            }
            parts.push(text);
        }
    }
    parts
}

/// The three cache tiers are assembled separately before their final join.
/// Preserve interior bytes while dropping empty sections and trimming only
/// each section's boundary, exactly as build_system_prompt_parts does.
#[derive(Debug, PartialEq)]
pub struct PromptParts {
    pub stable: String,
    pub context: String,
    pub volatile: String,
}

/// Loaded/rendered sections at the prompt-build boundary. Loaders resolve
/// their own availability gates before handing these snapshots to assembly.
#[derive(Default)]
pub struct ResolvedPromptSections {
    pub stable: Vec<String>,
    pub coding_prefix: Vec<String>,
    pub coding_workspace: Vec<String>,
    pub coding_tail: Vec<String>,
    pub post_workspace: Vec<String>,
    pub system_message: Option<String>,
    pub context_files: Option<String>,
    pub skills: Option<String>,
    pub memory: Option<String>,
    pub user_profile: Option<String>,
    pub external_memory: Option<String>,
    pub plugin_sections: Vec<String>,
    pub footer: String,
}

/// Captured inputs for runtime guidance at prompt construction. Environment
/// probing happens upstream; these values must belong to the same session.
pub struct RuntimeGuidance<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub platform: &'a str,
    pub tools: &'a [String],
    pub config: &'a serde_json::Value,
    pub scope: &'a crate::runtime_cwd::CwdInputs<'a>,
    pub temp_root: Option<&'a std::path::Path>,
    pub env: &'a std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    pub environment: &'a crate::environment_prompt::EnvironmentPromptInputs,
}

/// Final post-workspace inputs. Profile home and default root are distinct:
/// using the active home for both would mislabel the default profile's files.
pub struct ProfilePlatformGuidance<'a> {
    pub home: &'a std::path::Path,
    pub root: &'a std::path::Path,
    pub platform: &'a str,
    pub plugin_hint: &'a str,
    pub config: &'a serde_json::Value,
    pub overrides: &'a serde_json::Value,
    pub desktop_terminal: Option<&'a str>,
}

pub struct BotChatGuidance<'a> {
    pub enabled: bool,
    pub title_hint: &'a str,
    pub stored_title: &'a str,
    pub home: &'a std::path::Path,
    pub config: Option<&'a serde_json::Value>,
}

impl ResolvedPromptSections {
    /// Load skills before stable guidance so the help pointer sees the actual
    /// admitted index. Sessions without a skills tool perform no skill I/O.
    pub fn load_skills(
        &mut self,
        loader: &mut crate::skill_loader::PromptLoader,
        context: &crate::skill_loader::SourceContext<'_>,
        visibility: &crate::skill_loader::Visibility<'_>,
        compact: &std::collections::BTreeSet<String>,
        detect: impl FnMut(&str) -> Result<bool, String>,
    ) -> anyhow::Result<()> {
        self.skills = None;
        if !visibility.tools.is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| matches!(tool.as_str(), "skills_list" | "skill_view" | "skill_manage"))
        }) {
            return Ok(());
        }
        let rendered = loader.render_configured(context, visibility, compact, detect)?;
        if !rendered.is_empty() {
            self.skills = Some(rendered);
        }
        Ok(())
    }

    /// Snapshot availability and target enablement are independent. Disabled
    /// targets do not enter the prompt even when their snapshot is populated.
    pub fn set_memory_snapshot(
        &mut self,
        snapshot: Option<&crate::memory_snapshot::MemorySnapshot>,
        memory_enabled: bool,
        user_enabled: bool,
    ) {
        self.memory = snapshot
            .filter(|_| memory_enabled)
            .and_then(|s| s.memory())
            .map(str::to_owned);
        self.user_profile = snapshot
            .filter(|_| user_enabled)
            .and_then(|s| s.user())
            .map(str::to_owned);
    }

    pub fn set_footer(&mut self, footer: &crate::prompt_footer::Footer) {
        self.footer = footer.render();
    }

    /// Skills guidance is advertised only when the session has a skills tool.
    /// Use the same captured tool surface for the index's basic-tool wording.
    pub fn set_skills_index(&mut self, index: &crate::skills_index::Index, tools: &[String]) {
        self.skills = None;
        if !tools
            .iter()
            .any(|tool| matches!(tool.as_str(), "skills_list" | "skill_view" | "skill_manage"))
        {
            return;
        }
        let mut index = index.clone();
        index.tools = Some(tools.iter().cloned().collect());
        let rendered = index.render();
        if !rendered.is_empty() {
            self.skills = Some(rendered);
        }
    }

    /// Restore the after-memory plugin container without executing plugins.
    /// Invalid or absent persisted framing clears any previously staged block.
    pub fn restore_plugin_sections(&mut self, stored_prompt: &str) {
        let sections = crate::plugin_prompt::restore(stored_prompt);
        let block = crate::plugin_prompt::format(&sections);
        self.plugin_sections = if block.is_empty() {
            Vec::new()
        } else {
            vec![block]
        };
    }

    /// Resolve plugins once for this agent, then stage their frozen container
    /// after memory. A persisted prompt takes precedence over live callbacks.
    pub fn load_plugin_sections(
        &mut self,
        snapshot: &mut crate::plugin_prompt::Snapshot,
        stored_prompt: Option<&str>,
        registry: &crate::plugin_prompt::Registry,
        session_info: &serde_json::Map<String, serde_json::Value>,
    ) {
        let sections = snapshot.get_or_render(stored_prompt, || {
            Ok::<_, std::convert::Infallible>(registry.render(session_info))
        });
        let block = crate::plugin_prompt::format(sections);
        self.plugin_sections = if block.is_empty() {
            Vec::new()
        } else {
            vec![block]
        };
    }

    /// Insert only for the canonical Bot Chat. Return the timeless flag used
    /// by footer construction; legacy SOUL deduplication must not set it.
    pub fn append_bot_chat(
        &mut self,
        cache: &crate::bot_mode::ProtocolCache,
        input: &BotChatGuidance<'_>,
    ) -> bool {
        if !input.enabled {
            return false;
        }
        let hint = input
            .title_hint
            .trim_matches(crate::python_value::python_whitespace);
        let title = if hint.is_empty() {
            input
                .stored_title
                .trim_matches(crate::python_value::python_whitespace)
        } else {
            hint
        };
        if title != crate::bot_mode::BOT_CHAT_TITLE {
            return false;
        }
        let section = cache.section(input.home, false);
        if section.is_empty() {
            return false;
        }
        self.post_workspace.push(section);
        self.post_workspace.push(crate::bot_mode::epoch_line(
            &crate::bot_mode::capability_fingerprint(input.home, input.config),
        ));
        true
    }

    /// Append after environment-probe and Bot Chat protocol sections. Assembly
    /// keeps these in the stable tier unless a workspace snapshot precedes them.
    pub fn append_profile_platform(&mut self, input: &ProfilePlatformGuidance<'_>) {
        let profile = crate::profile_name::agent_profile_name(input.home, input.root);
        self.post_workspace.push(profile_hint(
            &profile,
            &input.home.to_string_lossy(),
            &input.root.to_string_lossy(),
        ));
        let hint = platform_hint(
            input.platform,
            input.plugin_hint,
            input.config,
            input.overrides,
            input.desktop_terminal,
        );
        if !hint.is_empty() {
            self.post_workspace.push(hint);
        }
    }

    /// Append provider/environment guidance, then capture coding blocks. Only
    /// coding discovery is best effort here, matching Python's exception gate.
    pub async fn load_runtime_guidance(&mut self, input: &RuntimeGuidance<'_>) {
        self.stable
            .extend(provider_identity(input.provider, input.model));
        let environment = crate::environment_prompt::build_environment_hints(input.environment);
        if !environment.is_empty() {
            self.stable.push(environment);
        }
        self.coding_prefix.clear();
        self.coding_workspace.clear();
        self.coding_tail.clear();
        if input.tools.is_empty() {
            return;
        }
        let coding = async {
            let cwd = input.scope.context_cwd()?;
            let cwd = cwd
                .as_ref()
                .map(|path| {
                    path.to_str()
                        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 coding cwd"))
                })
                .transpose()?;
            let mode = crate::coding_context::RuntimeMode::from_scope(
                input.platform,
                cwd,
                input.scope,
                input.temp_root,
                input.config,
                Some(input.model),
            )?;
            mode.system_prompt_parts(
                Some(input.tools),
                Some(input.scope.home),
                input.temp_root,
                input.env,
            )
            .await
        }
        .await;
        if let Ok(parts) = coding {
            self.coding_prefix = parts.prefix;
            self.coding_workspace = parts.workspace;
            self.coding_tail = parts.trailing;
        }
    }

    /// Initialize the stable prefix from the owning profile's identity. Return
    /// whether SOUL occupied that slot so context loading can avoid duplicating
    /// it. Cron may retain identity while disabling all project context.
    pub async fn initialize_stable(
        &mut self,
        request: &crate::context_files::ContextRequest<'_>,
        load_soul_identity: bool,
        skip_context_files: bool,
        input: StableGuidance<'_>,
    ) -> (bool, Vec<String>) {
        let (soul, warning) = if load_soul_identity || !skip_context_files {
            load_soul(request.home, request.max_chars, request.read_timeout).await
        } else {
            (None, None)
        };
        let loaded = soul.as_ref().is_some_and(|text| !text.is_empty());
        let input = StableGuidance {
            soul: soul.as_deref(),
            skills_index: self.skills.as_deref().unwrap_or_default(),
            ..input
        };
        self.stable = stable_guidance(&input);
        (loaded, warning.into_iter().collect())
    }

    /// Load the project-context slot with the surface policy from Python's
    /// prompt builder. A desktop launch artifact must remain a fallback even
    /// when tool routing has pinned it as the session cwd.
    pub async fn load_context(
        &mut self,
        scope: &crate::runtime_cwd::CwdInputs<'_>,
        mut request: crate::context_files::ContextRequest<'_>,
        platform: &str,
        launch_artifact: bool,
        skip_context_files: bool,
    ) -> std::io::Result<Vec<String>> {
        self.context_files = None;
        if skip_context_files {
            return Ok(Vec::new());
        }
        // Resolve before applying the artifact flag, preserving scope errors.
        let cwd = scope.context_cwd()?;
        let cwd = cwd.map(|path| scope.launch.join(path));
        let cwd = if launch_artifact { None } else { cwd };
        request.cwd = cwd.as_deref();
        request.launch_cwd = scope.launch;
        request.allow_install_tree_fallback = matches!(platform, "cli" | "tui");
        let loaded = crate::context_files::build_context(&request).await?;
        if !loaded.text.is_empty() {
            self.context_files = Some(loaded.text);
        }
        Ok(loaded.warnings)
    }

    /// A workspace snapshot moves the coding tail and following guidance to
    /// the context tier. Without it, both remain in the stable prefix. Test
    /// raw list presence before trimming, as Python does for this boundary.
    pub fn assemble(self) -> PromptParts {
        let mut stable = self.stable;
        stable.extend(self.coding_prefix);
        let mut context = Vec::new();
        let destination = if self.coding_workspace.is_empty() {
            &mut stable
        } else {
            context.extend(self.coding_workspace);
            &mut context
        };
        destination.extend(self.coding_tail);
        destination.extend(self.post_workspace);
        context.extend(self.system_message);
        context.extend(self.context_files);
        let mut volatile: Vec<String> = [
            self.skills,
            self.memory,
            self.user_profile,
            self.external_memory,
        ]
        .into_iter()
        .flatten()
        .collect();
        volatile.extend(self.plugin_sections);
        volatile.push(self.footer);
        PromptParts::from_sections(&stable, &context, &volatile)
    }
}

impl PromptParts {
    pub fn from_sections(stable: &[String], context: &[String], volatile: &[String]) -> Self {
        let join = |parts: &[String]| {
            parts
                .iter()
                .map(|part| part.trim_matches(crate::python_value::python_whitespace))
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        Self {
            stable: join(stable),
            context: join(context),
            volatile: join(volatile),
        }
    }

    pub fn joined(&self) -> String {
        [&self.stable, &self.context, &self.volatile]
            .into_iter()
            .filter(|part| !part.is_empty())
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Shared gate for tool-use enforcement and execution guidance. The caller
/// first checks whether tools exist. Unknown values use the model defaults;
/// custom lists match string entries only, including an empty-string wildcard.
pub fn model_guidance_enabled(setting: &serde_json::Value, model: &str, defaults: &[&str]) -> bool {
    match setting {
        serde_json::Value::Bool(enabled) => *enabled,
        serde_json::Value::String(value)
            if ["true", "always", "yes", "on"].contains(&value.to_lowercase().as_str()) =>
        {
            true
        }
        serde_json::Value::String(value)
            if ["false", "never", "no", "off"].contains(&value.to_lowercase().as_str()) =>
        {
            false
        }
        serde_json::Value::Array(patterns) => patterns
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|pattern| model.to_lowercase().contains(&pattern.to_lowercase())),
        _ => defaults
            .iter()
            .any(|pattern| model.to_lowercase().contains(pattern)),
    }
}

/// Identity resolved for the actual agent, including its terminal working dir.
pub struct PromptRuntime<'a> {
    pub model: &'a str,
    pub provider: &'a str,
    pub platform: &'a str,
    pub cwd: &'a str,
}

/// Check only the runtime identity fields used by Python's stored-prompt guard.
/// Footer fields use the last matching line. Cwd must come from the host-info
/// block, so embedded project instructions cannot invalidate it every turn.
pub fn stored_prompt_matches_runtime(prompt: &str, runtime: &PromptRuntime<'_>) -> bool {
    let lines = crate::python_value::split_lines(prompt);
    let trim = |value: &str| {
        value
            .trim_matches(crate::python_value::python_whitespace)
            .to_owned()
    };
    for (label, current) in [
        ("Model:", runtime.model),
        ("Provider:", runtime.provider),
        ("Platform:", runtime.platform),
    ] {
        let stored = lines
            .iter()
            .rev()
            .find_map(|line| line.strip_prefix(label))
            .map(trim)
            .unwrap_or_default();
        let current = trim(current);
        if !stored.is_empty() && !current.is_empty() && stored != current {
            return false;
        }
    }
    let cwd = lines
        .iter()
        .enumerate()
        .find_map(|(index, line)| {
            line.starts_with("User home directory:")
                .then(|| {
                    lines
                        .iter()
                        .skip(index + 1)
                        .take(3)
                        .find_map(|line| line.strip_prefix("Current working directory:"))
                })
                .flatten()
        })
        .map(trim)
        .unwrap_or_default();
    cwd.is_empty() || cwd == runtime.cwd
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn configured_skills_gate_io_and_supply_the_stable_help_pointer() {
        use std::collections::{BTreeMap, BTreeSet};
        let root =
            std::env::temp_dir().join(format!("hermes-prompt-skills-init-{}", std::process::id()));
        let skills = root.join("skills");
        std::fs::create_dir_all(skills.join("hermes-agent")).unwrap();
        std::fs::write(
            skills.join("hermes-agent/SKILL.md"),
            "---\nname: hermes-agent\ndescription: Operating manual\n---\n",
        )
        .unwrap();
        let config_path = root.join("config.yaml");
        let env = BTreeMap::new();
        let context = crate::skill_loader::SourceContext {
            config_path: &config_path,
            home: &root,
            skills: &skills,
            user_home: &root,
            launch: &root,
            project_start: &root,
            environment: &env,
        };
        let disabled = BTreeSet::new();
        let tools = BTreeSet::new();
        let mut visibility = crate::skill_loader::Visibility {
            host: "linux",
            termux: false,
            platform: Some("cli"),
            tools: Some(&tools),
            toolsets: None,
            disabled: &disabled,
        };
        let mut loader = crate::skill_loader::PromptLoader::default();
        let mut sections = super::ResolvedPromptSections::default();
        sections
            .load_skills(&mut loader, &context, &visibility, &BTreeSet::new(), |_| {
                panic!("tool gate must avoid scanning")
            })
            .unwrap();
        assert!(sections.skills.is_none());
        assert!(!root.join(".skills_prompt_snapshot.json").exists());
        let tools = BTreeSet::from(["skill_view".into()]);
        visibility.tools = Some(&tools);
        sections
            .load_skills(&mut loader, &context, &visibility, &BTreeSet::new(), |_| {
                Ok(true)
            })
            .unwrap();
        assert!(sections
            .skills
            .as_ref()
            .unwrap()
            .contains("- hermes-agent: Operating manual"));
        let request = crate::context_files::ContextRequest {
            cwd: None,
            launch_cwd: &root,
            install_root: &root,
            home: &root,
            skip_soul: true,
            allow_install_tree_fallback: false,
            max_chars: 20_000,
            read_timeout: std::time::Duration::from_secs(1),
        };
        sections
            .initialize_stable(
                &request,
                false,
                true,
                super::StableGuidance {
                    soul: None,
                    tools: &["skill_view".into()],
                    model: "",
                    skills_index: "",
                    settings: &serde_json::json!({}),
                },
            )
            .await;
        assert!(sections
            .stable
            .contains(&super::guidance("HERMES_AGENT_HELP_GUIDANCE").to_owned()));
        assert!(sections.assemble().volatile.contains("Operating manual"));
        std::fs::remove_dir_all(root).unwrap();
    }

    use super::*;

    #[test]
    fn bot_chat_title_gate_controls_protocol_epoch_and_timeless_flag() {
        let root = std::env::temp_dir().join(format!("hermes-bot-prompt-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("profile.yaml"), "ui_meta:\n  hermes-bots: {}\n").unwrap();
        let cache = crate::bot_mode::ProtocolCache::default();
        let config = serde_json::json!({});
        for (enabled, hint, stored, expected) in [
            (false, "Bot Chat", "", false),
            (true, "Other", "Bot Chat", false),
            (true, " bot chat ", "", false),
            (true, " Bot Chat ", "Other", true),
            (true, " \u{1c}", " Bot Chat ", true),
        ] {
            let mut sections = ResolvedPromptSections::default();
            assert_eq!(
                sections.append_bot_chat(
                    &cache,
                    &BotChatGuidance {
                        enabled,
                        title_hint: hint,
                        stored_title: stored,
                        home: &root,
                        config: Some(&config)
                    }
                ),
                expected
            );
            if expected {
                assert!(sections.post_workspace[0].starts_with(crate::bot_mode::PROTOCOL_HEADING));
                assert_eq!(
                    sections.post_workspace[1],
                    crate::bot_mode::epoch_line(&crate::bot_mode::capability_fingerprint(
                        &root,
                        Some(&config)
                    ))
                );
            } else {
                assert!(sections.post_workspace.is_empty());
            }
        }
        std::fs::write(root.join("SOUL.md"), crate::bot_mode::PROTOCOL_HEADING).unwrap();
        cache.section(&root, true);
        assert!(!ResolvedPromptSections::default().append_bot_chat(
            &cache,
            &BotChatGuidance {
                enabled: true,
                title_hint: "Bot Chat",
                stored_title: "",
                home: &root,
                config: Some(&config)
            }
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_platform_guidance_keeps_profile_paths_and_section_order() {
        let root = std::env::temp_dir().join("hermes-profile-platform-test");
        for profile in ["default", "red", "blue"] {
            let home = if profile == "default" {
                root.clone()
            } else {
                root.join("profiles").join(profile)
            };
            for workspace in [false, true] {
                let mut sections = ResolvedPromptSections {
                    post_workspace: vec!["Probe".into(), "Bot protocol".into(), "Epoch".into()],
                    coding_workspace: if workspace {
                        vec!["Workspace".into()]
                    } else {
                        vec![]
                    },
                    ..Default::default()
                };
                sections.append_profile_platform(&ProfilePlatformGuidance {
                    home: &home,
                    root: &root,
                    platform: "custom-platform",
                    plugin_hint: "Plugin platform",
                    config: &serde_json::json!({}),
                    overrides: &serde_json::json!({}),
                    desktop_terminal: None,
                });
                assert_eq!(
                    &sections.post_workspace[..3],
                    &["Probe", "Bot protocol", "Epoch"]
                );
                assert_eq!(
                    sections.post_workspace[3],
                    profile_hint(profile, home.to_str().unwrap(), root.to_str().unwrap())
                );
                assert_eq!(sections.post_workspace[4], "Plugin platform");
                let prompt = sections.assemble();
                let text = if workspace {
                    &prompt.context
                } else {
                    &prompt.stable
                };
                assert!(text.ends_with("\n\nPlugin platform"));
                assert!(text.contains(&format!("Active Hermes profile: {profile}.")));
                assert!(!text.contains(&format!("/profiles/{profile}/profiles/{profile}")));
            }
        }
    }

    #[tokio::test]
    async fn runtime_guidance_orders_sections_and_contains_coding_failures() {
        let root =
            std::env::temp_dir().join(format!("hermes-runtime-guidance-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        let home = root.join("home");
        let scope = crate::runtime_cwd::CwdInputs {
            session: None,
            terminal: "",
            launch: &root,
            home: &home,
        };
        let environment = crate::environment_prompt::EnvironmentPromptInputs {
            extra_hint: Some("Runtime environment".into()),
            ..Default::default()
        };
        for tools in [vec![], vec!["read_file".into()]] {
            for config in [
                serde_json::json!({"agent":{"coding_context":"on", "coding_instructions":"Operator tail"}}),
                serde_json::json!({"agent": true}),
            ] {
                let mut sections = ResolvedPromptSections {
                    stable: vec!["Identity".into()],
                    post_workspace: vec!["Platform hint".into()],
                    ..Default::default()
                };
                sections
                    .load_runtime_guidance(&RuntimeGuidance {
                        provider: "alibaba",
                        model: "vendor/test",
                        platform: "cli",
                        tools: &tools,
                        config: &config,
                        scope: &scope,
                        temp_root: Some(&std::env::temp_dir()),
                        env: &std::env::vars_os().collect(),
                        environment: &environment,
                    })
                    .await;
                let coding = !tools.is_empty() && config["agent"].is_object();
                assert_eq!(!sections.coding_prefix.is_empty(), coding);
                assert_eq!(sections.stable[0], "Identity");
                assert_eq!(
                    sections.stable[1],
                    provider_identity("alibaba", "vendor/test").unwrap()
                );
                assert_eq!(sections.stable[2], "Runtime environment");
                let parts = sections.assemble();
                assert_eq!(parts.context.contains("Operator tail"), coding);
                assert_eq!(parts.stable.contains("Platform hint"), !coding);
                assert_eq!(parts.context.contains("Platform hint"), coding);
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn stable_identity_uses_profile_home_and_does_not_duplicate_context() {
        let root =
            std::env::temp_dir().join(format!("hermes-stable-identity-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for (profile, contents) in [
            ("red", "Red persona"),
            ("blue", "Blue persona"),
            ("empty", "  \n"),
        ] {
            let home = root.join(profile);
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(home.join("SOUL.md"), contents).unwrap();
            for load in [false, true] {
                for skip in [false, true] {
                    let mut sections = ResolvedPromptSections::default();
                    let mut request = crate::context_files::ContextRequest {
                        cwd: None,
                        launch_cwd: &root,
                        install_root: &root,
                        home: &home,
                        skip_soul: false,
                        allow_install_tree_fallback: false,
                        max_chars: 20_000,
                        read_timeout: std::time::Duration::from_secs(1),
                    };
                    let (loaded, warnings) = sections
                        .initialize_stable(
                            &request,
                            load,
                            skip,
                            StableGuidance {
                                soul: Some("Unrelated supplied identity"),
                                tools: &[],
                                model: "",
                                skills_index: "",
                                settings: &serde_json::json!({}),
                            },
                        )
                        .await;
                    let expected = (load || !skip) && profile != "empty";
                    assert_eq!(loaded, expected);
                    assert!(warnings.is_empty());
                    assert_eq!(
                        sections.stable[0],
                        if expected {
                            contents
                        } else {
                            guidance("DEFAULT_AGENT_IDENTITY")
                        }
                    );
                    request.skip_soul = loaded;
                    let scope = crate::runtime_cwd::CwdInputs {
                        session: None,
                        terminal: "",
                        launch: &root,
                        home: &home,
                    };
                    sections
                        .load_context(&scope, request, "desktop", false, skip)
                        .await
                        .unwrap();
                    let prompt = sections.assemble();
                    assert!(!prompt.context.contains("persona"));
                    assert!(!prompt.stable.contains("Unrelated supplied identity"));
                }
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn context_loading_applies_surface_and_launch_artifact_policy() {
        let root = std::env::temp_dir().join(format!(
            "hermes-prompt-context-policy-{}",
            std::process::id()
        ));
        let install = root.join("install");
        let home = root.join("profile");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(install.join("AGENTS.md"), "Contributor instructions").unwrap();
        for platform in ["cli", "tui", "desktop", " CLI "] {
            for artifact in [false, true] {
                for configured in [false, true] {
                    for skip in [false, true] {
                        let scope = crate::runtime_cwd::CwdInputs {
                            session: configured.then(|| install.to_str().unwrap()),
                            terminal: "",
                            launch: &install,
                            home: &home,
                        };
                        let request = crate::context_files::ContextRequest {
                            cwd: None,
                            launch_cwd: &root,
                            install_root: &install,
                            home: &home,
                            skip_soul: true,
                            allow_install_tree_fallback: false,
                            max_chars: 20_000,
                            read_timeout: std::time::Duration::from_secs(1),
                        };
                        let mut sections = ResolvedPromptSections {
                            context_files: Some("stale context".into()),
                            ..Default::default()
                        };
                        sections
                            .load_context(&scope, request, platform, artifact, skip)
                            .await
                            .unwrap();
                        let expected = !skip
                            && ((configured && !artifact) || matches!(platform, "cli" | "tui"));
                        assert_eq!(
                            sections.context_files.is_some(),
                            expected,
                            "{platform:?} artifact={artifact} configured={configured} skip={skip}"
                        );
                        let assembled = sections.assemble();
                        assert_eq!(
                            assembled.context.contains("Contributor instructions"),
                            expected
                        );
                        assert!(assembled.stable.is_empty());
                    }
                }
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_and_provider_identity_match_python() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/prompt-identity-goldens.json"))
                .unwrap();
        for case in cases["profiles"].as_array().unwrap() {
            assert_eq!(
                profile_hint(
                    case["profile"].as_str().unwrap(),
                    case["home"].as_str().unwrap(),
                    case["root"].as_str().unwrap()
                ),
                case["expected"].as_str().unwrap()
            );
        }
        for case in cases["providers"].as_array().unwrap() {
            assert_eq!(
                provider_identity(
                    case["provider"].as_str().unwrap(),
                    case["model"].as_str().unwrap()
                )
                .as_deref(),
                case["expected"].as_str()
            );
        }
    }

    #[test]
    fn coding_workspace_preserves_prompt_boundaries() {
        for workspace in [vec![], vec![String::new()], vec![" workspace ".into()]] {
            let has_workspace = !workspace.is_empty();
            let parts = ResolvedPromptSections {
                stable: vec![" identity ".into()],
                coding_prefix: vec!["brief".into()],
                coding_workspace: workspace.clone(),
                coding_tail: vec!["coding tail".into()],
                post_workspace: vec!["profile/platform".into()],
                system_message: Some("caller".into()),
                context_files: Some("project".into()),
                skills: Some("skills".into()),
                memory: Some("memory".into()),
                user_profile: Some("user".into()),
                external_memory: Some("external".into()),
                plugin_sections: vec!["plugin".into()],
                footer: "metadata".into(),
            }
            .assemble();
            if has_workspace {
                assert_eq!(parts.stable, "identity\n\nbrief");
                let prefix = if workspace[0].is_empty() {
                    ""
                } else {
                    "workspace\n\n"
                };
                assert_eq!(
                    parts.context,
                    format!("{prefix}coding tail\n\nprofile/platform\n\ncaller\n\nproject")
                );
            } else {
                assert_eq!(
                    parts.stable,
                    "identity\n\nbrief\n\ncoding tail\n\nprofile/platform"
                );
                assert_eq!(parts.context, "caller\n\nproject");
            }
            assert_eq!(
                parts.volatile,
                "skills\n\nmemory\n\nuser\n\nexternal\n\nplugin\n\nmetadata"
            );
        }
    }

    #[test]
    fn context_limits_match_python() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/context-limits-goldens.json"))
                .unwrap();
        for case in cases.as_array().unwrap() {
            let (limit, timeout) = context_file_limits(&case["config"], &case["context_length"]);
            assert_eq!(limit as u64, case["limit"].as_u64().unwrap(), "{case}");
            assert_eq!(timeout, case["timeout"].as_f64().unwrap(), "{case}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn soul_deadline_releases_startup_while_reader_is_stalled() {
        use std::os::unix::ffi::OsStrExt;
        let root = std::env::temp_dir().join(format!("hermes-soul-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("SOUL.md");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // A FIFO with no writer stalls the real open/read path, without mocks.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            load_soul(&root, 20000, std::time::Duration::from_millis(20)),
        )
        .await;
        // Connect a writer after the deadline so the detached reader can exit.
        let writer = std::thread::spawn(move || std::fs::write(path, b"late identity"));
        writer.join().unwrap().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(result.unwrap(), (None, None));
    }

    #[tokio::test]
    async fn soul_reads_are_scoped_and_normalized() {
        let root = std::env::temp_dir().join(format!("hermes-soul-{}", std::process::id()));
        let red = root.join("red");
        let blue = root.join("blue");
        std::fs::create_dir_all(&red).unwrap();
        std::fs::create_dir_all(&blue).unwrap();
        let deadline = std::time::Duration::from_secs(2);
        std::fs::write(red.join("SOUL.md"), " \u{feff}red\r\nidentity\r ").unwrap();
        std::fs::write(blue.join("SOUL.md"), " blue identity ").unwrap();
        let (a, b) = tokio::join!(
            load_soul(&red, 20000, deadline),
            load_soul(&blue, 20000, deadline)
        );
        assert_eq!(a, (Some("red\nidentity".into()), None));
        assert_eq!(b, (Some("blue identity".into()), None));
        std::fs::write(red.join("SOUL.md"), "\u{001c} \n").unwrap();
        assert_eq!(load_soul(&red, 20000, deadline).await, (None, None));
        std::fs::write(red.join("SOUL.md"), [0xff]).unwrap();
        assert_eq!(load_soul(&red, 20000, deadline).await, (None, None));
        std::fs::write(red.join("SOUL.md"), "a\u{200b}b").unwrap();
        let (text, warning) = load_soul(&red, 20000, deadline).await;
        assert_eq!(text.unwrap(), "[BLOCKED: SOUL.md contained potential prompt injection (invisible_unicode_U+200B). Content not loaded.]");
        assert!(warning.is_none());
        let absent = root.join("absent");
        assert_eq!(load_soul(&absent, 20000, deadline).await, (None, None));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn context_truncation_matches_python() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/context-truncation-goldens.json"
        ))
        .unwrap();
        for case in cases.as_array().unwrap() {
            let (content, warning) = truncate_context_content(
                case["content"].as_str().unwrap(),
                case["filename"].as_str().unwrap(),
                case["max_chars"].as_u64().unwrap() as usize,
                case["read_path"].as_str(),
            );
            assert_eq!(content, case["expected"].as_str().unwrap(), "{case}");
            assert_eq!(warning.as_deref(), case["warning"].as_str(), "{case}");
        }
    }

    #[test]
    fn platform_overrides_match_python() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/platform-hint-goldens.json"))
                .unwrap();
        for case in cases.as_array().unwrap() {
            assert_eq!(
                resolve_platform_hint(
                    case["platform"].as_str().unwrap(),
                    case["default"].as_str().unwrap(),
                    &case["overrides"],
                ),
                case["expected"].as_str().unwrap(),
                "{case}"
            );
        }
    }

    #[test]
    fn platform_selection_matches_python() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/platform-hint-selection-goldens.json"
        ))
        .unwrap();
        for case in cases.as_array().unwrap() {
            assert_eq!(
                platform_hint(
                    case["platform"].as_str().unwrap(),
                    " plugin hint ",
                    &case["config"],
                    &case["overrides"],
                    case["desktop_terminal"].as_str(),
                ),
                case["expected"].as_str().unwrap(),
                "{case}"
            );
        }
    }

    #[test]
    fn initial_stable_sections_match_python_order_and_bytes() {
        use sha2::{Digest, Sha256};
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/stable-guidance-goldens.json"))
                .unwrap();
        for case in cases.as_array().unwrap() {
            let tools: Vec<String> = serde_json::from_value(case["tools"].clone()).unwrap();
            let parts = stable_guidance(&StableGuidance {
                tools: &tools,
                model: case["model"].as_str().unwrap(),
                settings: &case["settings"],
                soul: case["soul"].as_str(),
                skills_index: case["skills_index"].as_str().unwrap(),
            });
            let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&parts).unwrap()));
            assert_eq!(hash, case["sha256"].as_str().unwrap(), "{case}");
        }
    }

    #[test]
    fn tier_join_and_guidance_gates_match_python() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/system-prompt-assembly-goldens.json"
        ))
        .unwrap();
        for case in cases["tiers"].as_array().unwrap() {
            let sections: Vec<Vec<String>> =
                serde_json::from_value(case["sections"].clone()).unwrap();
            let parts = PromptParts::from_sections(&sections[0], &sections[1], &sections[2]);
            assert_eq!(
                [&parts.stable, &parts.context, &parts.volatile],
                [
                    case["parts"]["stable"].as_str().unwrap(),
                    case["parts"]["context"].as_str().unwrap(),
                    case["parts"]["volatile"].as_str().unwrap()
                ]
            );
            assert_eq!(parts.joined(), case["joined"].as_str().unwrap());
        }
        for case in cases["gates"].as_array().unwrap() {
            let defaults: Vec<&str> = case["defaults"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(
                model_guidance_enabled(
                    &case["setting"],
                    case["model"].as_str().unwrap(),
                    &defaults
                ),
                case["enabled"].as_bool().unwrap(),
                "{case}"
            );
        }
    }

    #[test]
    fn stored_identity_matches_python_oracle() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/stored-prompt-runtime-goldens.json"
        ))
        .unwrap();
        for case in cases.as_array().unwrap() {
            let runtime = &case["runtime"];
            assert_eq!(
                stored_prompt_matches_runtime(
                    case["prompt"].as_str().unwrap(),
                    &PromptRuntime {
                        model: runtime["model"].as_str().unwrap(),
                        provider: runtime["provider"].as_str().unwrap(),
                        platform: runtime["platform"].as_str().unwrap(),
                        cwd: runtime["cwd"].as_str().unwrap(),
                    }
                ),
                case["matches"].as_bool().unwrap(),
                "{case}"
            );
        }
    }
}
