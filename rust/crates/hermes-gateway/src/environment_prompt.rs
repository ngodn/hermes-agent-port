//! Environment prompt rendering for the system prompt stable tier.
//!
//! Renders `agent/prompt_builder.py::build_environment_hints` from resolved inputs and
//! transitive helpers (`_WINDOWS_BASH_SHELL_HINT`, `WSL_ENVIRONMENT_HINT`,
//! `_BACKEND_FALLBACK_DESCRIPTIONS`, `_probe_remote_backend` parsing, and
//! `_windows_marketing_version`).
//!
//! This module renders the environment section of the system prompt from
//! explicit resolved inputs without requiring ambient system mutation.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Instructions emitted when Hermes runs inside WSL (Windows Subsystem for Linux).
/// Appended in both local and remote terminal modes when running under WSL.
pub const WSL_ENVIRONMENT_HINT: &str = concat!(
    "You are running inside WSL (Windows Subsystem for Linux). ",
    "The Windows host filesystem is mounted under /mnt/ — ",
    "/mnt/c/ is the C: drive, /mnt/d/ is D:, etc. ",
    "The user's Windows files are typically at ",
    "/mnt/c/Users/<username>/Desktop/, Documents/, Downloads/, etc. ",
    "When the user references Windows paths or desktop files, translate ",
    "to the /mnt/c/ equivalent. You can list /mnt/c/Users/ to discover ",
    "the Windows username if needed."
);

/// Shell guidance emitted on native Windows hosts where terminal commands run through MSYS/bash.
pub const WINDOWS_BASH_SHELL_HINT: &str = concat!(
    "Shell: on this Windows host your `terminal` tool runs commands through ",
    "bash (git-bash / MSYS), NOT PowerShell or cmd.exe. Use POSIX shell ",
    "syntax (`ls`, `$HOME`, `&&`, `|`, single-quoted strings) inside terminal ",
    "calls. MSYS-style paths like `/c/Users/<user>/...` work alongside ",
    "native `C:\\Users\\<user>\\...` paths. PowerShell builtins ",
    "(`Get-ChildItem`, `$env:FOO`, `Select-String`) will NOT work — use their ",
    "POSIX equivalents (`ls`, `$FOO`, `grep`). Path arguments for NATIVE ",
    "Windows programs (git, rg, node, python, ...) are NOT translated: MSYS ",
    "path conversion is disabled here, so `git -C /c/Users/x` or ",
    "`node /tmp/a.js` fails with 'cannot change to'/'not found' even though ",
    "`cd /c/Users/x` (a bash builtin) works. Pass `C:/Users/x`-style ",
    "forward-slash native paths to native tools, and prefer ",
    "`$LOCALAPPDATA/Temp` over `/tmp` for scratch files a native tool must ",
    "read. When answering prompts in a ",
    "pty background process, use process(submit) — never process(write) ",
    "with a bare trailing newline: Enter on a Windows PTY is a carriage ",
    "return, and a lone `\\n` is not delivered as a line terminator, so the ",
    "child's prompt silently never returns. When a CLI offers a ",
    "non-interactive path (flags, `--with-token`, config files, an OAuth ",
    "device flow polled with curl), prefer it over driving prompts."
);

/// Hostname disambiguation notice emitted on Windows hosts.
pub const WINDOWS_HOSTNAME_NOTE: &str = concat!(
    "Note: on Windows, the machine hostname (e.g. from `hostname` ",
    "or uname) is NOT the username. Use the 'User home directory' ",
    "above to construct paths under C:\\Users\\<user>\\, never the ",
    "hostname."
);

/// In-tree remote terminal backends that run commands inside a separate container or host.
pub const REMOTE_TERMINAL_BACKENDS: &[&str] = &[
    "docker",
    "singularity",
    "modal",
    "daytona",
    "ssh",
    "vercel_sandbox",
    "managed_modal",
];

/// Returns the static fallback description for built-in remote backends when live probe fails.
pub fn backend_fallback_description(backend: &str) -> Option<&'static str> {
    match backend {
        "docker" => Some("a Docker container (Linux)"),
        "singularity" => Some("a Singularity container (Linux)"),
        "modal" => Some("a Modal sandbox (Linux)"),
        "managed_modal" => Some("a managed Modal sandbox (Linux)"),
        "daytona" => Some("a Daytona workspace (Linux)"),
        "vercel_sandbox" => Some("a Vercel sandbox (Linux)"),
        "ssh" => Some("a remote host reached over SSH (likely Linux)"),
        _ => None,
    }
}

/// Returns fallback description for any backend name, defaulting to "a {backend} environment (likely Linux)".
pub fn default_fallback_description(backend: &str) -> String {
    backend_fallback_description(backend)
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("a {backend} environment (likely Linux)"))
}

/// Computes the marketing Windows version string ("10" or "11") from the OS build number.
/// Build >= 22000 corresponds to Windows 11.
pub fn windows_marketing_version_from_build(build: u32) -> &'static str {
    if build >= 22000 {
        "11"
    } else {
        "10"
    }
}

/// Helper to check if a backend name belongs to the built-in remote backend set.
pub fn is_known_remote_backend(backend: &str) -> bool {
    REMOTE_TERMINAL_BACKENDS.contains(
        &backend
            .trim_matches(crate::python_value::python_whitespace)
            .to_lowercase()
            .as_str(),
    )
}

/// Resolve the embedder hint before typed config. Python stringifies the
/// configured value, including null and containers, but skips malformed
/// truthy agent sections whose .get call would fail.
pub fn resolve_extra_hint(env_hint: Option<&str>, config: &serde_json::Value) -> Option<String> {
    if let Some(hint) = env_hint {
        let trimmed = hint.trim_matches(crate::python_value::python_whitespace);
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let agent = config.as_object()?.get("agent")?;
    if !crate::python_value::truthy(agent) {
        return None;
    }
    let hint = agent.as_object()?.get("environment_hint")?;
    let text = match hint {
        serde_json::Value::String(text) => text.clone(),
        _ => crate::python_value::python_repr(hint),
    };
    let trimmed = text.trim_matches(crate::python_value::python_whitespace);
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Parsed state from the remote introspection one-liner:
/// `printf 'os=%s\nkernel=%s\nhome=%s\ncwd=%s\nuser=%s\n' ...`
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RemoteProbeState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl RemoteProbeState {
    /// Parse key=value output lines from the POSIX probe command into structured fields.
    pub fn parse_raw(output: &str) -> Option<Self> {
        let mut parsed = HashMap::new();
        for line in crate::python_value::split_lines(output) {
            if let Some((k, v)) = line.split_once('=') {
                parsed.insert(
                    k.trim_matches(crate::python_value::python_whitespace),
                    v.trim_matches(crate::python_value::python_whitespace),
                );
            }
        }

        let os = parsed
            .get("os")
            .copied()
            .filter(|s| !s.is_empty() && *s != "unknown")
            .map(String::from);
        let kernel = parsed
            .get("kernel")
            .copied()
            .filter(|s| !s.is_empty() && *s != "unknown")
            .map(String::from);
        let user = parsed
            .get("user")
            .copied()
            .filter(|s| !s.is_empty() && *s != "unknown")
            .map(String::from);
        let home = parsed
            .get("home")
            .copied()
            .filter(|s| !s.is_empty())
            .map(String::from);
        let cwd = parsed
            .get("cwd")
            .copied()
            .filter(|s| !s.is_empty())
            .map(String::from);

        if os.is_none() && kernel.is_none() && user.is_none() && home.is_none() && cwd.is_none() {
            None
        } else {
            Some(Self {
                os,
                kernel,
                user,
                home,
                cwd,
            })
        }
    }

    /// Formats the state into 2-space indented lines matching Python's `_probe_remote_backend`:
    /// ```text
    ///   OS: <os> <kernel>
    ///   User: <user>
    ///   Home: <home>
    ///   Working directory: <cwd>
    /// ```
    pub fn format_probe(&self) -> Option<String> {
        let mut pieces = Vec::new();
        let mut os_bits = Vec::new();
        if let Some(ref os) = self.os {
            if !os.is_empty() && os != "unknown" {
                os_bits.push(os.as_str());
            }
        }
        if let Some(ref kernel) = self.kernel {
            if !kernel.is_empty() && kernel != "unknown" {
                os_bits.push(kernel.as_str());
            }
        }
        if !os_bits.is_empty() {
            pieces.push(format!("OS: {}", os_bits.join(" ")));
        }
        if let Some(ref user) = self.user {
            if !user.is_empty() && user != "unknown" {
                pieces.push(format!("User: {user}"));
            }
        }
        if let Some(ref home) = self.home {
            if !home.is_empty() {
                pieces.push(format!("Home: {home}"));
            }
        }
        if let Some(ref cwd) = self.cwd {
            if !cwd.is_empty() {
                pieces.push(format!("Working directory: {cwd}"));
            }
        }

        if pieces.is_empty() {
            None
        } else {
            Some(
                pieces
                    .into_iter()
                    .map(|p| format!("  {p}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
    }
}

/// Represents the probe outcome for a remote backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum RemoteProbeResult {
    /// Already-formatted 2-space indented probe text.
    #[serde(rename = "formatted")]
    Formatted(String),
    /// Structured probe state to be formatted.
    #[serde(rename = "structured")]
    Structured(RemoteProbeState),
    /// Raw stdout lines from probe execution to be parsed and formatted.
    #[serde(rename = "raw")]
    Raw(String),
    /// The probe failed or timed out.
    #[serde(rename = "failed")]
    Failed,
}

impl RemoteProbeResult {
    /// Resolves the probe result into formatted 2-space indented text, or `None` if failed/empty.
    pub fn to_formatted(&self) -> Option<String> {
        match self {
            Self::Formatted(s) => {
                if s.is_empty() {
                    None
                } else {
                    Some(s.clone())
                }
            }
            Self::Structured(state) => state.format_probe(),
            Self::Raw(raw) => RemoteProbeState::parse_raw(raw).and_then(|s| s.format_probe()),
            Self::Failed => None,
        }
    }
}

/// Host OS classification for local terminal environments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum LocalHostPlatform {
    /// Host: WSL (Windows Subsystem for Linux)
    #[serde(rename = "wsl")]
    Wsl,
    /// Host: Windows ({marketing_version})
    #[serde(rename = "windows")]
    Windows { marketing_version: String },
    /// Host: macOS ({version})
    #[serde(rename = "mac_os")]
    MacOS { version: String },
    /// Host: {system} ({release})
    #[serde(rename = "other")]
    Other { system: String, release: String },
}

/// Host execution context when running local terminal backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalHostInfo {
    pub platform: LocalHostPlatform,
    pub user_home: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl LocalHostInfo {
    pub fn new(
        platform: LocalHostPlatform,
        user_home: impl Into<String>,
        cwd: Option<impl Into<String>>,
    ) -> Self {
        Self {
            platform,
            user_home: user_home.into(),
            cwd: cwd.map(Into::into),
        }
    }
}

/// Fully-resolved inputs for rendering the environment prompt.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EnvironmentPromptInputs {
    /// The configured terminal backend name, e.g. "local", "docker", "ssh".
    /// Defaults to "local" when unset or empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,

    /// Plugin classification, added to the built-in remote backend set.
    /// False never makes a built-in remote backend local.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_remote: Option<bool>,

    /// Outcome of the remote probe command for remote backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_probe: Option<RemoteProbeResult>,

    /// Fallback description used when remote probe fails.
    /// If None, uses built-in backend description or "a {backend} environment (likely Linux)".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_description: Option<String>,

    /// Local host execution details (host OS, user home, current working directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_host: Option<LocalHostInfo>,

    /// Whether the host machine is WSL. When true, appends `WSL_ENVIRONMENT_HINT`.
    #[serde(default)]
    pub is_wsl: bool,

    /// Extra embedder-configured environment hint (`HERMES_ENVIRONMENT_HINT` or `config.agent.environment_hint`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_hint: Option<String>,
}

impl EnvironmentPromptInputs {
    /// Convenience constructor for local backend execution.
    pub fn local(
        platform: LocalHostPlatform,
        user_home: impl Into<String>,
        cwd: Option<impl Into<String>>,
    ) -> Self {
        Self {
            backend: Some("local".to_string()),
            local_host: Some(LocalHostInfo::new(platform, user_home, cwd)),
            ..Default::default()
        }
    }

    /// Convenience constructor for remote backend execution.
    pub fn remote(backend: impl Into<String>, probe: RemoteProbeResult) -> Self {
        let b = backend.into();
        Self {
            backend: Some(b),
            is_remote: Some(true),
            remote_probe: Some(probe),
            ..Default::default()
        }
    }

    /// Appends or sets an embedder extra hint.
    pub fn with_extra_hint(mut self, hint: impl Into<String>) -> Self {
        self.extra_hint = Some(hint.into());
        self
    }

    /// Sets WSL host flag.
    pub fn with_wsl(mut self, is_wsl: bool) -> Self {
        self.is_wsl = is_wsl;
        self
    }

    /// Sets fallback description for remote probe failure.
    pub fn with_fallback_description(mut self, desc: impl Into<String>) -> Self {
        self.fallback_description = Some(desc.into());
        self
    }

    /// Renders the prompt string from this configuration.
    pub fn render(&self) -> String {
        render_environment_prompt(self)
    }
}

/// Renders the environment hints block for the system prompt.
///
/// Ports `agent/prompt_builder.py::build_environment_hints()` with exact byte fidelity.
pub fn render_environment_prompt(inputs: &EnvironmentPromptInputs) -> String {
    let mut hints: Vec<String> = Vec::new();

    // 1. Backend resolution: (_tenv_read("TERMINAL_ENV") or "local").strip().lower()
    let raw_backend = inputs.backend.as_deref().unwrap_or("local");
    let backend = if raw_backend.is_empty() {
        "local".to_string()
    } else {
        raw_backend
            .trim_matches(crate::python_value::python_whitespace)
            .to_lowercase()
    };

    // 2. Determine if remote backend
    let is_remote_backend =
        REMOTE_TERMINAL_BACKENDS.contains(&backend.as_str()) || inputs.is_remote.unwrap_or(false);

    let effective_is_wsl = inputs.is_wsl
        || matches!(
            inputs.local_host.as_ref().map(|h| &h.platform),
            Some(LocalHostPlatform::Wsl)
        );

    if !is_remote_backend {
        // --- Host info block (local backend: host == where tools run) ---
        if let Some(ref host) = inputs.local_host {
            let mut host_lines: Vec<String> = Vec::new();
            let is_win32 = matches!(host.platform, LocalHostPlatform::Windows { .. });

            if effective_is_wsl {
                host_lines.push("Host: WSL (Windows Subsystem for Linux)".to_string());
            } else {
                match &host.platform {
                    LocalHostPlatform::Wsl => {
                        host_lines.push("Host: WSL (Windows Subsystem for Linux)".to_string());
                    }
                    LocalHostPlatform::Windows { marketing_version } => {
                        host_lines.push(format!("Host: Windows ({marketing_version})"));
                    }
                    LocalHostPlatform::MacOS { version } => {
                        host_lines.push(format!("Host: macOS ({version})"));
                    }
                    LocalHostPlatform::Other { system, release } => {
                        host_lines.push(format!("Host: {system} ({release})"));
                    }
                }
            }

            host_lines.push(format!("User home directory: {}", host.user_home));
            if let Some(ref cwd) = host.cwd {
                host_lines.push(format!("Current working directory: {cwd}"));
            }

            if is_win32 && !effective_is_wsl {
                host_lines.push(WINDOWS_HOSTNAME_NOTE.to_string());
            }

            hints.push(host_lines.join("\n"));

            if is_win32 && !effective_is_wsl {
                hints.push(WINDOWS_BASH_SHELL_HINT.to_string());
            }
        }
    } else {
        // --- Remote backend block (host info suppressed) ---
        let probe = inputs.remote_probe.as_ref().and_then(|p| p.to_formatted());
        if let Some(probe_str) = probe {
            hints.push(format!(
                "Terminal backend: {backend}. Your `terminal`, `read_file`, `write_file`, \
                `patch`, and `search_files` tools all operate inside this {backend} environment — \
                NOT on the machine where Hermes itself is running. The host OS, home, and cwd of \
                the Hermes process are irrelevant; only the following backend state matters:\n\
                {probe_str}"
            ));
        } else {
            let description = backend_fallback_description(&backend)
                .map(String::from)
                .or_else(|| {
                    inputs
                        .fallback_description
                        .clone()
                        .filter(|text| !text.is_empty())
                })
                .unwrap_or_else(|| format!("a {backend} environment (likely Linux)"));

            hints.push(format!(
                "Terminal backend: {backend}. Your `terminal`, `read_file`, `write_file`, \
                `patch`, and `search_files` tools all operate inside {description} — \
                NOT on the machine where Hermes itself runs. The backend probe didn't \
                respond at prompt-build time, so the sandbox's current user, $HOME, \
                and working directory are unknown from here. If you need them, probe \
                directly with a terminal call like `uname -a && whoami && pwd`."
            ));
        }
    }

    if effective_is_wsl {
        hints.push(WSL_ENVIRONMENT_HINT.to_string());
    }

    if let Some(ref extra) = inputs.extra_hint {
        let trimmed = extra.trim_matches(crate::python_value::python_whitespace);
        if !trimmed.is_empty() {
            hints.push(trimmed.to_string());
        }
    }

    hints.join("\n\n")
}

/// Alias for `render_environment_prompt` matching Python's function name.
pub fn build_environment_hints(inputs: &EnvironmentPromptInputs) -> String {
    render_environment_prompt(inputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct GoldenCase {
        id: String,
        inputs: EnvironmentPromptInputs,
        expected: String,
    }

    #[test]
    fn test_goldens_match() {
        let text = include_str!("../../../tools/environment-prompt-goldens.json");
        let cases: Vec<GoldenCase> = serde_json::from_str(text).expect("valid goldens JSON");
        for case in cases {
            let rendered = render_environment_prompt(&case.inputs);
            assert_eq!(
                rendered, case.expected,
                "Rendered output mismatch on golden case: {}",
                case.id
            );
        }
    }

    #[test]
    fn test_probe_state_parse_and_format() {
        let raw = "os=Linux\nkernel=6.6.0\nhome=/root\ncwd=/workspace\nuser=root\n";
        let state = RemoteProbeState::parse_raw(raw).expect("parsed probe state");
        assert_eq!(state.os.as_deref(), Some("Linux"));
        assert_eq!(state.kernel.as_deref(), Some("6.6.0"));
        assert_eq!(state.user.as_deref(), Some("root"));
        assert_eq!(state.home.as_deref(), Some("/root"));
        assert_eq!(state.cwd.as_deref(), Some("/workspace"));

        let formatted = state.format_probe().expect("formatted probe");
        assert_eq!(
            formatted,
            "  OS: Linux 6.6.0\n  User: root\n  Home: /root\n  Working directory: /workspace"
        );

        // Test unknown filtering
        let raw_unknown = "os=Linux\nkernel=unknown\nhome=/home/dev\nuser=unknown\n";
        let state_unknown = RemoteProbeState::parse_raw(raw_unknown).unwrap();
        let formatted_unknown = state_unknown.format_probe().unwrap();
        assert_eq!(formatted_unknown, "  OS: Linux\n  Home: /home/dev");

        // Test empty / malformed
        assert!(RemoteProbeState::parse_raw("").is_none());
        assert!(RemoteProbeState::parse_raw("no equals sign here\n").is_none());
        assert!(RemoteProbeState::parse_raw("os=\nkernel=\n").is_none());
    }

    #[test]
    fn test_backend_fallback_descriptions() {
        assert_eq!(
            backend_fallback_description("docker"),
            Some("a Docker container (Linux)")
        );
        assert_eq!(
            backend_fallback_description("singularity"),
            Some("a Singularity container (Linux)")
        );
        assert_eq!(
            backend_fallback_description("modal"),
            Some("a Modal sandbox (Linux)")
        );
        assert_eq!(
            backend_fallback_description("managed_modal"),
            Some("a managed Modal sandbox (Linux)")
        );
        assert_eq!(
            backend_fallback_description("daytona"),
            Some("a Daytona workspace (Linux)")
        );
        assert_eq!(
            backend_fallback_description("vercel_sandbox"),
            Some("a Vercel sandbox (Linux)")
        );
        assert_eq!(
            backend_fallback_description("ssh"),
            Some("a remote host reached over SSH (likely Linux)")
        );
        assert_eq!(backend_fallback_description("custom"), None);
        assert_eq!(
            default_fallback_description("custom"),
            "a custom environment (likely Linux)"
        );
    }

    #[test]
    fn test_windows_marketing_version() {
        assert_eq!(windows_marketing_version_from_build(22631), "11");
        assert_eq!(windows_marketing_version_from_build(22000), "11");
        assert_eq!(windows_marketing_version_from_build(21999), "10");
        assert_eq!(windows_marketing_version_from_build(19045), "10");
    }

    #[test]
    fn test_stored_prompt_runtime_anchors() {
        let inputs = EnvironmentPromptInputs::local(
            LocalHostPlatform::Other {
                system: "Linux".to_string(),
                release: "6.8.0".to_string(),
            },
            "/home/user",
            Some("/home/user/project"),
        );
        let rendered = render_environment_prompt(&inputs);
        let lines: Vec<&str> = rendered.lines().collect();
        let home_idx = lines
            .iter()
            .position(|l| l.starts_with("User home directory:"))
            .expect("User home directory line present");
        let cwd_line = lines[home_idx + 1];
        assert_eq!(cwd_line, "Current working directory: /home/user/project");
    }

    #[test]
    fn test_extra_hint_resolution() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/environment-extra-hint-goldens.json"
        ))
        .unwrap();
        for case in cases.as_array().unwrap() {
            assert_eq!(
                resolve_extra_hint(case["env"].as_str(), &case["config"]).as_deref(),
                case["expected"].as_str(),
                "{case}"
            );
        }
    }
}
