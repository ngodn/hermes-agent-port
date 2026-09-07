//! Bounded coding prompt renderer for Hermes gateway.
//!
//! Ports the prompt rendering logic from `agent/coding_context.py`:
//!   - `ContextProfile` declarative posture definitions (`coding`, `general`)
//!   - `RuntimeMode.system_prompt_parts` bounded prompt rendering
//!   - Model family classification (`_model_family`)
//!   - Edit-format steering guidance line (`_edit_format_line`)
//!   - Toolset filtering (dropping `todo_list` when the tool is not loaded)
//!   - Operator instructions block formatting (`Operator instructions (from config):\n...`)
//!   - Workspace block pass-through from explicit resolved inputs
//!
//! # Architecture & Boundary Guarantees
//!
//! This module is a **pure, bounded prompt renderer**. It does NOT perform:
//!   - Ambient filesystem walks or project marker detection
//!   - Subprocess execution or git repository probing
//!   - Configuration loading or environment variable inspection
//!
//! All runtime detection, configuration resolution, and git probing MUST be
//! performed by caller gates before invoking this renderer. The renderer receives
//! already-resolved posture, model id, valid tool names, operator instructions,
//! and workspace snapshot text.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Literal Constants (Extracted from agent/coding_context.py)
// ---------------------------------------------------------------------------

/// Default toolset name for coding posture.
pub const CODING_TOOLSET: &str = "coding";

/// Operating brief for the coding posture.
///
/// Ports `agent/coding_context.py::CODING_AGENT_GUIDANCE` verbatim.
pub const CODING_AGENT_GUIDANCE: &str = concat!(
    "You are a coding agent pairing with the user inside their codebase. ",
    "Operate like a careful senior engineer.\n",
    "\n",
    "Gather context first:\n",
    "- Read the relevant files with `read_file` and locate code with ",
    "`search_files` before changing anything. Trace a symbol to its definition ",
    "and usages rather than guessing its shape.\n",
    "- Batch independent lookups: when several reads/searches don't depend on ",
    "each other, issue them together in one turn instead of one at a time.\n",
    "- Never invent files, symbols, APIs, or imports. If you haven't seen it in ",
    "the repo, go look. Don't assume a library is available \u{2014} check the project ",
    "manifest (pyproject.toml / package.json / Cargo.toml / go.mod) and how ",
    "neighbouring files import it.\n",
    "\n",
    "Make changes through the tools, not the chat:\n",
    "- Edit with `patch`/`write_file`. Do NOT print code blocks to the user as ",
    "a substitute for editing \u{2014} apply the change, then summarise it. Only show ",
    "code when the user explicitly asks to see it.\n",
    "- Match the project's existing style and conventions; AGENTS.md / ",
    "CLAUDE.md / .cursorrules already in context win over your defaults. Touch ",
    "only what the task needs \u{2014} no drive-by refactors, renames, or reformatting ",
    "\u{2014} and add any imports/dependencies your code requires.\n",
    "- If an edit fails to apply, re-read the file to get the current exact ",
    "contents before retrying \u{2014} don't repeat a stale patch. If the same region ",
    "fails twice, rewrite the enclosing function or file with `write_file` ",
    "instead of attempting a third patch.\n",
    "\n",
    "Verify, and know when to stop:\n",
    "- Use `terminal` for git, builds, tests, and inspection. Run the relevant ",
    "tests/linter/build and confirm they pass before claiming the work is done.\n",
    "- Terminal state persists across calls: current directory and exported ",
    "environment variables carry forward. Activate a virtualenv or export setup ",
    "vars once, then reuse that state instead of re-sourcing it before every ",
    "test command.\n",
    "- Fix root causes, not symptoms: when you find a bug, check sibling call ",
    "paths for the same flaw and fix the class, not just the reported site.\n",
    "- When fixing linter/type errors on a file, stop after about three ",
    "attempts on the same file and ask the user rather than looping.\n",
    "- Track multi-step work with `todo_list`. Reference code as `path:line` instead ",
    "of pasting whole files.\n",
    "\n",
    "Respect the user's repo: don't commit, push, or rewrite history unless ",
    "asked, and never read, print, or commit secrets \u{2014} leave `.env` and ",
    "credential files alone unless the user explicitly asks. The Workspace ",
    "block below is a snapshot from session start \u{2014} re-run `git status`/",
    "`git branch` before relying on it. Be concise: lead with the change or ",
    "answer, not a preamble."
);

/// Needles identifying models in the `"patch"` family (V4A multi-file diff).
pub const PATCH_MODEL_NEEDLES: &[&str] = &["gpt", "codex"];

/// Guidance line for models in the `"patch"` family.
pub const EDIT_FORMAT_PATCH_GUIDANCE: &str = concat!(
    "- Edit format: author new files with `write_file`; for edits to existing ",
    "code use `patch` with `mode='patch'` (V4A diff) \u{2014} including single-file edits. ",
    "It's the edit format you handle most reliably."
);

/// Needles identifying models in the `"replace"` family (find-and-swap).
pub const REPLACE_MODEL_NEEDLES: &[&str] = &[
    "claude", "sonnet", "opus", "haiku", "gemini", "gemma", "deepseek", "qwen", "kimi", "glm",
    "grok", "hermes", "llama", "mistral", "devstral", "minimax",
];

/// Guidance line for models in the `"replace"` family.
pub const EDIT_FORMAT_REPLACE_GUIDANCE: &str = concat!(
    "- Edit format: author new files with `write_file`; for edits to existing ",
    "code prefer `patch` in `mode='replace'` \u{2014} match a unique snippet and swap it. ",
    "Reach for `mode='patch'` (V4A) only when an edit genuinely spans several files at once."
);

/// Skill categories demoted to names-only under `focus` mode.
pub const NON_CODING_SKILL_CATEGORIES: &[&str] = &[
    "apple",
    "communication",
    "cooking",
    "creative",
    "email",
    "finance",
    "gaming",
    "gifs",
    "health",
    "media",
    "music",
    "note-taking",
    "productivity",
    "shopping",
    "smart-home",
    "social-media",
    "travel",
    "yuanbao",
];

/// Target sentence in `CODING_AGENT_GUIDANCE` when `todo_list` is available.
pub const TODO_REPLACEMENT_TARGET: &str = concat!(
    "- Track multi-step work with `todo_list`. Reference code as ",
    "`path:line` instead of pasting whole files."
);

/// Replacement sentence in `CODING_AGENT_GUIDANCE` when `todo_list` is omitted.
pub const TODO_REPLACEMENT_VALUE: &str =
    "- Reference code as `path:line` instead of pasting whole files.";

/// Prefix prepended to standing operator instructions from config.
pub const OPERATOR_INSTRUCTIONS_HEADER: &str = "Operator instructions (from config):\n";

// ---------------------------------------------------------------------------
// Model Family & Edit Format Steering
// ---------------------------------------------------------------------------

/// Edit format family classification for steering models toward their native format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFamily {
    /// Models trained on V4A multi-file diffs (e.g. OpenAI / Codex).
    Patch,
    /// Models trained on string replacement / search-and-replace (e.g. Claude, Gemini, DeepSeek, Qwen).
    Replace,
}

impl ModelFamily {
    /// Return the string identifier of the family (`"patch"` or `"replace"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Patch => "patch",
            Self::Replace => "replace",
        }
    }

    /// Return the exact guidance line for this model family.
    pub fn guidance_line(&self) -> &'static str {
        match self {
            Self::Patch => EDIT_FORMAT_PATCH_GUIDANCE,
            Self::Replace => EDIT_FORMAT_REPLACE_GUIDANCE,
        }
    }
}

/// Classify a model id into an edit-format family key, or `None`.
///
/// Matching order is deterministic: `"patch"` needles are evaluated first,
/// followed by `"replace"` needles. Substring matching is case-insensitive.
pub fn model_family(model: Option<&str>) -> Option<ModelFamily> {
    let model = model?;
    if model.is_empty() {
        return None;
    }
    let lowered = model.to_lowercase();
    if PATCH_MODEL_NEEDLES
        .iter()
        .any(|needle| lowered.contains(needle))
    {
        return Some(ModelFamily::Patch);
    }
    if REPLACE_MODEL_NEEDLES
        .iter()
        .any(|needle| lowered.contains(needle))
    {
        return Some(ModelFamily::Replace);
    }
    None
}

/// Return the edit-format guidance line for this model's family (`""` if none).
pub fn edit_format_line(model: Option<&str>) -> &'static str {
    match model_family(model) {
        Some(family) => family.guidance_line(),
        None => "",
    }
}

// ---------------------------------------------------------------------------
// ContextProfile & CodingPosture
// ---------------------------------------------------------------------------

/// A named operating posture. Pure data.
///
/// Ports `agent/coding_context.py::ContextProfile`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextProfile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolset: Option<String>,
    #[serde(default)]
    pub guidance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_hint: Option<String>,
    #[serde(default = "default_memory_policy")]
    pub memory_policy: String,
    #[serde(default)]
    pub compact_skill_categories: Vec<String>,
}

fn default_memory_policy() -> String {
    "default".to_string()
}

impl Default for ContextProfile {
    fn default() -> Self {
        Self::general()
    }
}

impl ContextProfile {
    /// Whether this profile corresponds to the active coding posture.
    pub fn is_coding(&self) -> bool {
        self.name == "coding"
    }

    /// Return the standard `general` context profile.
    pub fn general() -> Self {
        Self {
            name: "general".to_string(),
            toolset: None,
            guidance: String::new(),
            model_hint: None,
            memory_policy: "default".to_string(),
            compact_skill_categories: Vec::new(),
        }
    }

    /// Return the standard `coding` context profile.
    pub fn coding() -> Self {
        Self {
            name: "coding".to_string(),
            toolset: Some(CODING_TOOLSET.to_string()),
            guidance: CODING_AGENT_GUIDANCE.to_string(),
            model_hint: Some("coding".to_string()),
            memory_policy: "project".to_string(),
            compact_skill_categories: NON_CODING_SKILL_CATEGORIES
                .iter()
                .map(|&s| s.to_string())
                .collect(),
        }
    }

    /// Return a registered profile by name, falling back to `general`.
    pub fn get(name: &str) -> Self {
        if name == "coding" {
            Self::coding()
        } else {
            Self::general()
        }
    }
}

/// Explicit already-resolved posture representation.
///
/// Can be supplied as a profile name (`"coding"`, `"general"`), a boolean flag,
/// or a full `ContextProfile`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CodingPosture {
    Name(String),
    Bool(bool),
    Profile(ContextProfile),
}

impl Default for CodingPosture {
    fn default() -> Self {
        Self::Name("coding".to_string())
    }
}

impl CodingPosture {
    /// Return the canonical `coding` posture.
    pub fn coding() -> Self {
        Self::Name("coding".to_string())
    }

    /// Return the canonical `general` posture.
    pub fn general() -> Self {
        Self::Name("general".to_string())
    }

    /// Whether this posture is coding.
    pub fn is_coding(&self) -> bool {
        match self {
            Self::Bool(b) => *b,
            Self::Name(name) => name == "coding",
            Self::Profile(profile) => profile.is_coding(),
        }
    }

    /// Resolve the posture into a `ContextProfile`.
    pub fn profile(&self) -> ContextProfile {
        match self {
            Self::Bool(true) => ContextProfile::coding(),
            Self::Bool(false) => ContextProfile::general(),
            Self::Name(name) => ContextProfile::get(name),
            Self::Profile(profile) => profile.clone(),
        }
    }
}

impl From<&str> for CodingPosture {
    fn from(s: &str) -> Self {
        Self::Name(s.to_string())
    }
}

impl From<String> for CodingPosture {
    fn from(s: String) -> Self {
        Self::Name(s)
    }
}

impl From<bool> for CodingPosture {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

impl From<ContextProfile> for CodingPosture {
    fn from(p: ContextProfile) -> Self {
        Self::Profile(p)
    }
}

impl From<&ContextProfile> for CodingPosture {
    fn from(p: &ContextProfile) -> Self {
        Self::Profile(p.clone())
    }
}

// ---------------------------------------------------------------------------
// Inputs and Prompt Parts
// ---------------------------------------------------------------------------

/// Explicit resolved inputs for rendering the coding prompt sections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingPromptInputs {
    /// Operating posture (e.g. `"coding"`, `"general"`, or full `ContextProfile`).
    #[serde(default)]
    pub posture: CodingPosture,
    /// The model id for this session (steers edit format guidance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Loaded tools for the session (when `None`, tool filtering is skipped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_tool_names: Option<Vec<String>>,
    /// Standing operator instructions (from `agent.coding_instructions`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Resolved workspace snapshot block (built once at session start).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_text: Option<String>,
}

impl Default for CodingPromptInputs {
    fn default() -> Self {
        Self {
            posture: CodingPosture::coding(),
            model: None,
            valid_tool_names: None,
            instructions: None,
            workspace_text: None,
        }
    }
}

impl CodingPromptInputs {
    /// Create a new inputs container with the specified posture.
    pub fn new(posture: impl Into<CodingPosture>) -> Self {
        Self {
            posture: posture.into(),
            model: None,
            valid_tool_names: None,
            instructions: None,
            workspace_text: None,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.valid_tool_names = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    pub fn with_workspace_text(mut self, workspace_text: impl Into<String>) -> Self {
        self.workspace_text = Some(workspace_text.into());
        self
    }

    /// Render prompt parts from these inputs.
    pub fn render(&self) -> CodingPromptParts {
        render_coding_prompt_parts(self)
    }
}

/// The three separated prompt blocks corresponding to Python's
/// `(prefix_parts, workspace_parts, trailing_parts)`.
///
/// Designed to populate `ResolvedPromptSections::coding_prefix`,
/// `ResolvedPromptSections::coding_workspace`, and `ResolvedPromptSections::coding_tail`
/// directly in prompt assembly.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CodingPromptParts {
    /// Operating brief with optional edit-format nudge (Stable Tier).
    pub prefix: Vec<String>,
    /// Live workspace snapshot (Context Tier if present).
    pub workspace: Vec<String>,
    /// Configured operator instructions (Tier follows presence of workspace).
    pub trailing: Vec<String>,
}

impl CodingPromptParts {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether all three prompt sections are empty.
    pub fn is_empty(&self) -> bool {
        self.prefix.is_empty() && self.workspace.is_empty() && self.trailing.is_empty()
    }

    /// Access the trailing operator instructions (synonymous with `coding_tail`).
    pub fn tail(&self) -> &[String] {
        &self.trailing
    }

    /// Flatten the parts into the historical list of system blocks.
    ///
    /// Ports `RuntimeMode.system_blocks()`.
    pub fn system_blocks(&self) -> Vec<String> {
        let mut blocks =
            Vec::with_capacity(self.prefix.len() + self.workspace.len() + self.trailing.len());
        blocks.extend(self.prefix.clone());
        blocks.extend(self.workspace.clone());
        blocks.extend(self.trailing.clone());
        blocks
    }

    /// Borrow parts as a 3-tuple `(&prefix, &workspace, &trailing)`.
    pub fn as_tuple(&self) -> (&[String], &[String], &[String]) {
        (&self.prefix, &self.workspace, &self.trailing)
    }

    /// Convert parts into an owned 3-tuple `(prefix, workspace, trailing)`.
    pub fn into_tuple(self) -> (Vec<String>, Vec<String>, Vec<String>) {
        (self.prefix, self.workspace, self.trailing)
    }
}

// ---------------------------------------------------------------------------
// Renderer Implementation
// ---------------------------------------------------------------------------

/// Render separated coding prompt parts from explicit resolved inputs.
///
/// Ports `RuntimeMode.system_prompt_parts(self, valid_tool_names=None)`.
pub fn render_coding_prompt_parts(inputs: &CodingPromptInputs) -> CodingPromptParts {
    let profile = inputs.posture.profile();
    if !inputs.posture.is_coding() {
        return CodingPromptParts::default();
    }

    let mut prefix = Vec::new();
    let mut workspace = Vec::new();
    let mut trailing = Vec::new();

    // 1. Operating brief with tool adaptation and edit-format steering
    if !profile.guidance.is_empty() {
        let mut brief = profile.guidance.clone();
        if let Some(ref tools) = inputs.valid_tool_names {
            if !tools.iter().any(|t| t == "todo_list") {
                brief = brief.replace(TODO_REPLACEMENT_TARGET, TODO_REPLACEMENT_VALUE);
            }
        }
        let edit_line = edit_format_line(inputs.model.as_deref());
        if !edit_line.is_empty() {
            brief.push('\n');
            brief.push_str(edit_line);
        }
        prefix.push(brief);
    }

    // 2. Workspace snapshot block
    if let Some(ref ws) = inputs.workspace_text {
        if !ws.is_empty() {
            workspace.push(ws.clone());
        }
    }

    // 3. Operator instructions block
    if let Some(ref inst) = inputs.instructions {
        if !inst.is_empty() {
            trailing.push(format!("{OPERATOR_INSTRUCTIONS_HEADER}{inst}"));
        }
    }

    CodingPromptParts {
        prefix,
        workspace,
        trailing,
    }
}

/// Convenience function to render prompt parts from explicit parameters.
pub fn render_coding_prompt(
    posture: impl Into<CodingPosture>,
    model: Option<&str>,
    valid_tool_names: Option<&[impl AsRef<str>]>,
    instructions: Option<&str>,
    workspace_text: Option<&str>,
) -> CodingPromptParts {
    let posture = posture.into();
    let inputs = CodingPromptInputs {
        posture,
        model: model.map(|s| s.to_string()),
        valid_tool_names: valid_tool_names
            .map(|tools| tools.iter().map(|t| t.as_ref().to_string()).collect()),
        instructions: instructions.map(|s| s.to_string()),
        workspace_text: workspace_text.map(|s| s.to_string()),
    };
    render_coding_prompt_parts(&inputs)
}

/// Compatibility alias matching Python's `coding_system_prompt_parts` signature.
pub fn coding_system_prompt_parts(
    posture: impl Into<CodingPosture>,
    model: Option<&str>,
    valid_tool_names: Option<&[impl AsRef<str>]>,
    instructions: Option<&str>,
    workspace_text: Option<&str>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    render_coding_prompt(
        posture,
        model,
        valid_tool_names,
        instructions,
        workspace_text,
    )
    .into_tuple()
}

/// Demoted skill categories under `focus` mode.
///
/// Ports `RuntimeMode.compact_skill_categories()`.
pub fn compact_skill_categories(
    is_coding: bool,
    config_mode: Option<&str>,
) -> &'static [&'static str] {
    if !is_coding || config_mode != Some("focus") {
        &[]
    } else {
        NON_CODING_SKILL_CATEGORIES
    }
}

// ---------------------------------------------------------------------------
// Inline Tests & Golden Verification
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct GoldenConstants {
        coding_toolset: String,
        coding_agent_guidance: String,
        non_coding_skill_categories: Vec<String>,
        edit_format_guidance: std::collections::HashMap<String, GoldenEditGuidance>,
        todo_replacement_target: String,
        todo_replacement_value: String,
        operator_instructions_header: String,
    }

    #[derive(Deserialize)]
    struct GoldenEditGuidance {
        needles: Vec<String>,
        line: String,
    }

    #[derive(Deserialize)]
    struct GoldenModelCase {
        model: Option<String>,
        expected_family: Option<String>,
        expected_line: String,
    }

    #[derive(Deserialize)]
    struct GoldenPromptPartsCase {
        id: String,
        inputs: CodingPromptInputs,
        expected: CodingPromptParts,
    }

    #[derive(Deserialize)]
    struct GoldensRoot {
        constants: GoldenConstants,
        model_family_cases: Vec<GoldenModelCase>,
        prompt_parts_cases: Vec<GoldenPromptPartsCase>,
    }

    fn load_goldens() -> GoldensRoot {
        let text = include_str!("../../../tools/coding-prompt-goldens.json");
        serde_json::from_str(text).expect("valid coding-prompt-goldens.json")
    }

    #[test]
    fn test_constants_match_goldens() {
        let goldens = load_goldens();
        assert_eq!(CODING_TOOLSET, goldens.constants.coding_toolset);
        assert_eq!(
            CODING_AGENT_GUIDANCE,
            goldens.constants.coding_agent_guidance
        );
        assert_eq!(
            NON_CODING_SKILL_CATEGORIES,
            goldens
                .constants
                .non_coding_skill_categories
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .as_slice()
        );
        assert_eq!(
            TODO_REPLACEMENT_TARGET,
            goldens.constants.todo_replacement_target
        );
        assert_eq!(
            TODO_REPLACEMENT_VALUE,
            goldens.constants.todo_replacement_value
        );
        assert_eq!(
            OPERATOR_INSTRUCTIONS_HEADER,
            goldens.constants.operator_instructions_header
        );

        // Edit format guidance
        let patch_golden = &goldens.constants.edit_format_guidance["patch"];
        assert_eq!(EDIT_FORMAT_PATCH_GUIDANCE, patch_golden.line);
        assert_eq!(
            PATCH_MODEL_NEEDLES,
            patch_golden
                .needles
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .as_slice()
        );

        let replace_golden = &goldens.constants.edit_format_guidance["replace"];
        assert_eq!(EDIT_FORMAT_REPLACE_GUIDANCE, replace_golden.line);
        assert_eq!(
            REPLACE_MODEL_NEEDLES,
            replace_golden
                .needles
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .as_slice()
        );
    }

    #[test]
    fn test_model_family_cases() {
        let goldens = load_goldens();
        for case in goldens.model_family_cases {
            let model_ref = case.model.as_deref();
            let actual_family = model_family(model_ref);
            let actual_family_str = actual_family.map(|f| f.as_str().to_string());
            assert_eq!(
                actual_family_str, case.expected_family,
                "Model family mismatch for model: {:?}",
                case.model
            );

            let actual_line = edit_format_line(model_ref);
            assert_eq!(
                actual_line, case.expected_line,
                "Edit format line mismatch for model: {:?}",
                case.model
            );
        }
    }

    #[test]
    fn test_prompt_parts_cases() {
        let goldens = load_goldens();
        for case in goldens.prompt_parts_cases {
            let rendered = render_coding_prompt_parts(&case.inputs);
            assert_eq!(
                rendered.prefix, case.expected.prefix,
                "Prefix mismatch on golden case: {}",
                case.id
            );
            assert_eq!(
                rendered.workspace, case.expected.workspace,
                "Workspace mismatch on golden case: {}",
                case.id
            );
            assert_eq!(
                rendered.trailing, case.expected.trailing,
                "Trailing mismatch on golden case: {}",
                case.id
            );
            assert_eq!(
                rendered.system_blocks(),
                case.expected.system_blocks(),
                "system_blocks mismatch on golden case: {}",
                case.id
            );
        }
    }

    #[test]
    fn test_context_profile_and_posture() {
        let general = ContextProfile::general();
        assert!(!general.is_coding());
        assert_eq!(general.name, "general");
        assert_eq!(general.guidance, "");

        let coding = ContextProfile::coding();
        assert!(coding.is_coding());
        assert_eq!(coding.name, "coding");
        assert_eq!(coding.toolset.as_deref(), Some(CODING_TOOLSET));
        assert_eq!(coding.guidance, CODING_AGENT_GUIDANCE);

        let from_coding_str: CodingPosture = "coding".into();
        assert!(from_coding_str.is_coding());

        let from_general_str: CodingPosture = "general".into();
        assert!(!from_general_str.is_coding());

        let from_bool_true: CodingPosture = true.into();
        assert!(from_bool_true.is_coding());

        let from_bool_false: CodingPosture = false.into();
        assert!(!from_bool_false.is_coding());
    }

    #[test]
    fn test_compact_skill_categories() {
        assert!(compact_skill_categories(false, Some("focus")).is_empty());
        assert!(compact_skill_categories(true, Some("auto")).is_empty());
        assert!(compact_skill_categories(true, None).is_empty());
        assert!(compact_skill_categories(true, Some("off")).is_empty());
        assert_eq!(
            compact_skill_categories(true, Some("focus")),
            NON_CODING_SKILL_CATEGORIES
        );
    }

    #[test]
    fn test_builder_and_convenience_apis() {
        let inputs = CodingPromptInputs::new("coding")
            .with_model("claude-3-5-sonnet")
            .with_tools(vec!["read_file", "patch"])
            .with_instructions("Custom instruction")
            .with_workspace_text("Workspace: /root");

        let parts = inputs.render();
        assert_eq!(parts.prefix.len(), 1);
        assert!(!parts.prefix[0].contains("todo_list"));
        assert!(parts.prefix[0].contains("mode='replace'"));
        assert_eq!(parts.workspace, vec!["Workspace: /root".to_string()]);
        assert_eq!(
            parts.trailing,
            vec!["Operator instructions (from config):\nCustom instruction".to_string()]
        );
        assert_eq!(parts.tail(), &parts.trailing);

        let (p, w, t) = coding_system_prompt_parts(
            "coding",
            Some("gpt-4o"),
            Some(&["todo_list", "read_file"]),
            Some("Keep tests passing"),
            Some("Workspace: /app"),
        );
        assert_eq!(p.len(), 1);
        assert!(p[0].contains("todo_list"));
        assert!(p[0].contains("mode='patch'"));
        assert_eq!(w, vec!["Workspace: /app".to_string()]);
        assert_eq!(
            t,
            vec!["Operator instructions (from config):\nKeep tests passing".to_string()]
        );
    }
}
