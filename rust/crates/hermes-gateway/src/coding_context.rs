//! Session-stable coding posture configuration, following agent/coding_context.py.
#![allow(dead_code)]

use serde_json::Value;

#[derive(Default, serde::Serialize)]
pub struct WorkspaceStatus {
    pub branch: std::collections::BTreeMap<String, String>,
    pub counts: std::collections::BTreeMap<String, usize>,
}

/// Parse the reference's porcelain-v2 subset. Malformed tracked or ahead/behind
/// records are errors, as Python raises on missing fields instead of inventing
/// a clean status. The workspace caller owns the failure policy.
pub fn parse_status(porcelain: &str) -> anyhow::Result<WorkspaceStatus> {
    fn split_two(mut text: &str) -> Vec<&str> {
        let mut result = Vec::new();
        for _ in 0..2 {
            text = text.trim_start_matches(crate::python_value::python_whitespace);
            if text.is_empty() {
                return result;
            }
            let end = text
                .find(crate::python_value::python_whitespace)
                .unwrap_or(text.len());
            result.push(&text[..end]);
            text = &text[end..];
        }
        text = text.trim_start_matches(crate::python_value::python_whitespace);
        if !text.is_empty() {
            result.push(text);
        }
        result
    }
    let mut result = WorkspaceStatus::default();
    for name in ["staged", "modified", "untracked", "conflicts"] {
        result.counts.insert(name.into(), 0);
    }
    for line in crate::python_value::split_lines(porcelain) {
        if line.starts_with("# branch.head") || line.starts_with("# branch.upstream") {
            let key = if line.starts_with("# branch.head") {
                "head"
            } else {
                "upstream"
            };
            result
                .branch
                .insert(key.into(), split_two(line).last().unwrap().to_string());
        } else if line.starts_with("# branch.ab") {
            let parts: Vec<_> = line
                .split(crate::python_value::python_whitespace)
                .filter(|part| !part.is_empty())
                .collect();
            anyhow::ensure!(parts.len() >= 4, "incomplete branch ahead/behind record");
            result
                .branch
                .insert("ahead".into(), parts[2].trim_start_matches('+').into());
            result
                .branch
                .insert("behind".into(), parts[3].trim_start_matches('-').into());
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let parts = split_two(line);
            let xy: Vec<_> = parts
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("missing tracked status"))?
                .chars()
                .collect();
            anyhow::ensure!(xy.len() >= 2, "incomplete tracked status");
            if xy[0] != '.' {
                *result.counts.get_mut("staged").unwrap() += 1;
            }
            if xy[1] != '.' {
                *result.counts.get_mut("modified").unwrap() += 1;
            }
        } else if line.starts_with("u ") {
            *result.counts.get_mut("conflicts").unwrap() += 1;
        } else if line.starts_with("? ") {
            *result.counts.get_mut("untracked").unwrap() += 1;
        }
    }
    Ok(result)
}

fn detection_constants() -> &'static Value {
    static DATA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str(include_str!(
            "../../../tools/coding-detection-constants.json"
        ))
        .expect("coding detection constants")
    })
}

fn includes(key: &str, name: &str) -> bool {
    detection_constants()[key]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value.as_str() == Some(name))
}

/// Scan root and immediate children only. Preserve directory iteration order
/// and count every entry toward the reference's shared 500-entry budget.
pub fn has_code_files(root: &std::path::Path) -> bool {
    let mut stack = vec![(root.to_owned(), true)];
    let mut seen = 0;
    while let Some((directory, is_root)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            seen += 1;
            if seen
                > detection_constants()["_CODE_SCAN_MAX_ENTRIES"]
                    .as_u64()
                    .unwrap()
            {
                return false;
            }
            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if path.is_file() {
                // Python splitext treats leading dots as part of the stem.
                if let Some((_, extension)) = name
                    .rsplit_once('.')
                    .filter(|(stem, _)| stem.chars().any(|ch| ch != '.'))
                {
                    if includes(
                        "_CODE_EXTENSIONS",
                        &format!(".{}", extension.to_lowercase()),
                    ) {
                        return true;
                    }
                }
            } else if is_root
                && path.is_dir()
                && !name.starts_with('.')
                && !includes("_CODE_SCAN_SKIP_DIRS", name)
            {
                stack.push((path, false));
            }
        }
    }
    false
}

/// Detect once when constructing a session. Home and shared temp roots are
/// explicit resolved inputs, preventing manifests there from acting as global
/// coding signals. A home-rooted dotfiles repository is excluded too.
pub fn detect_coding(
    mode: &str,
    platform: &str,
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    temp_root: Option<&std::path::Path>,
) -> std::io::Result<bool> {
    if mode == "off" {
        return Ok(false);
    }
    if mode == "on" {
        return Ok(true);
    }
    let platform = platform
        .trim_matches(crate::python_value::python_whitespace)
        .to_lowercase();
    if !includes("INTERACTIVE_CODING_PLATFORMS", &platform) {
        return Ok(false);
    }
    let cwd = crate::context_files::resolve(cwd)?;
    let home = home.and_then(|path| crate::context_files::resolve(path).ok());
    let temp_root = temp_root.and_then(|path| crate::context_files::resolve(path).ok());
    if marker_root(&cwd, home.as_deref(), temp_root.as_deref())?.is_some() {
        return Ok(true);
    }
    let root = crate::context_files::find_git_root(&cwd)?;
    Ok(root
        .filter(|path| home.as_ref() != Some(path))
        .is_some_and(|path| has_code_files(&path)))
}

/// Nearest recognized project marker, excluding home and shared temp roots.
pub fn marker_root(
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    temp_root: Option<&std::path::Path>,
) -> std::io::Result<Option<std::path::PathBuf>> {
    let cwd = crate::context_files::resolve(cwd)?;
    let home = home.and_then(|path| crate::context_files::resolve(path).ok());
    let temp_root = temp_root.and_then(|path| crate::context_files::resolve(path).ok());
    for parent in cwd.ancestors().take(7) {
        if home.as_deref() == Some(parent) || temp_root.as_deref() == Some(parent) {
            continue;
        }
        if detection_constants()["_PROJECT_MARKERS"]
            .as_array()
            .unwrap()
            .iter()
            .any(|marker| parent.join(marker.as_str().unwrap()).exists())
        {
            return Ok(Some(parent.to_owned()));
        }
    }
    Ok(None)
}

pub struct CodingSettings {
    pub mode: &'static str,
    pub instructions: String,
}

/// Session-owned posture. Resolve once, then share read-only with prompt,
/// toolset and memory consumers so later config or filesystem changes cannot
/// silently change the conversation's operating instructions.
pub struct RuntimeMode {
    profile: crate::coding_prompt::ContextProfile,
    surface: String,
    cwd: std::path::PathBuf,
    settings: CodingSettings,
    model: Option<String>,
}

impl RuntimeMode {
    /// Apply Python's explicit/implicit cwd precedence using captured scope
    /// inputs, then anchor relative paths for filesystem discovery.
    pub fn from_scope(
        platform: &str,
        cwd: Option<&str>,
        scope: &crate::runtime_cwd::CwdInputs<'_>,
        temp_root: Option<&std::path::Path>,
        config: &Value,
        model: Option<&str>,
    ) -> anyhow::Result<Self> {
        let cwd = scope.coding_cwd(cwd)?;
        Self::resolve(
            platform,
            &scope.launch.join(cwd),
            Some(scope.home),
            temp_root,
            config,
            model,
        )
    }

    /// Paths and config are explicit profile-scoped inputs. The caller resolves
    /// absent cwd to its captured launch directory before constructing a mode.
    pub fn resolve(
        platform: &str,
        cwd: &std::path::Path,
        home: Option<&std::path::Path>,
        temp_root: Option<&std::path::Path>,
        config: &Value,
        model: Option<&str>,
    ) -> anyhow::Result<Self> {
        let cwd = crate::context_files::resolve(cwd)?;
        let settings = settings(config)?;
        let coding = detect_coding(settings.mode, platform, &cwd, home, temp_root)?;
        Ok(Self {
            profile: crate::coding_prompt::ContextProfile::get(if coding {
                "coding"
            } else {
                "general"
            }),
            surface: platform.to_owned(),
            cwd,
            settings,
            model: model.map(str::to_owned),
        })
    }

    pub fn profile(&self) -> &crate::coding_prompt::ContextProfile {
        &self.profile
    }
    pub fn surface(&self) -> &str {
        &self.surface
    }
    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }
    pub fn config_mode(&self) -> &str {
        self.settings.mode
    }

    /// A proposal only; callers must retain an explicit user toolset selection.
    pub fn toolset_selection(&self, raw_config: &Value) -> Option<Vec<String>> {
        focus_toolsets(
            self.settings.mode,
            self.profile.toolset.as_deref(),
            raw_config,
        )
    }

    /// Called at prompt construction, not for each model request. Keep the
    /// resulting blocks with the session's cached system prompt.
    pub async fn system_prompt_parts(
        &self,
        valid_tool_names: Option<&[String]>,
        home: Option<&std::path::Path>,
        temp_root: Option<&std::path::Path>,
        env: &std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    ) -> anyhow::Result<crate::coding_prompt::CodingPromptParts> {
        let workspace_text = if self.profile.is_coding() {
            Some(workspace_snapshot(&self.cwd, home, temp_root, env).await?)
        } else {
            None
        };
        Ok(crate::coding_prompt::CodingPromptInputs {
            posture: (&self.profile).into(),
            model: self.model.clone(),
            valid_tool_names: valid_tool_names.map(<[String]>::to_vec),
            instructions: Some(self.settings.instructions.clone()),
            workspace_text,
        }
        .render())
    }
}

/// Capture workspace state at prompt construction. Preserve probe and output
/// ordering, and expose only the linked-worktree fact, never a second worktree
/// path that could misdirect subsequent tool calls.
pub async fn workspace_snapshot(
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    temp_root: Option<&std::path::Path>,
    env: &std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
) -> anyhow::Result<String> {
    let git_root = crate::context_files::find_git_root(cwd)?;
    let root = match &git_root {
        Some(root) => Some(root.clone()),
        None => marker_root(cwd, home, temp_root)?,
    };
    let Some(root) = root else {
        return Ok(String::new());
    };
    let mut lines = vec![
        "Workspace (snapshot at session start \u{2014} re-check with `git` before acting on it):"
            .to_owned(),
        format!("- Root: {}", root.display()),
    ];
    if git_root.is_some() {
        let status =
            crate::git_probe::git(&root, &["status", "--porcelain=2", "--branch"], env.clone())
                .await;
        let status = parse_status(&status)?;
        let branch = |key: &str| status.branch.get(key).map(String::as_str).unwrap_or("");
        let head = branch("head");
        if head == "(detached)" {
            lines.push("- Branch: (detached HEAD)".into());
        } else if !head.is_empty() {
            let mut line = format!("- Branch: {head}");
            if !branch("upstream").is_empty() {
                line.push_str(&format!(" → {}", branch("upstream")));
                let ahead = status
                    .branch
                    .get("ahead")
                    .map(String::as_str)
                    .unwrap_or("0");
                let behind = status
                    .branch
                    .get("behind")
                    .map(String::as_str)
                    .unwrap_or("0");
                if ahead != "0" || behind != "0" {
                    line.push_str(&format!(" (ahead {ahead}, behind {behind})"));
                }
            }
            lines.push(line);
        }
        let git_dir = crate::git_probe::git(&root, &["rev-parse", "--git-dir"], env.clone()).await;
        let common =
            crate::git_probe::git(&root, &["rev-parse", "--git-common-dir"], env.clone()).await;
        if !git_dir.is_empty()
            && !common.is_empty()
            && crate::context_files::resolve(std::path::Path::new(&git_dir))?
                != crate::context_files::resolve(std::path::Path::new(&common))?
        {
            lines.push("- Worktree: linked (git state shared with primary tree)".into());
        }
        let dirty: Vec<_> = ["staged", "modified", "untracked", "conflicts"]
            .into_iter()
            .filter_map(|label| {
                let count = status.counts[label];
                (count != 0).then(|| format!("{count} {label}"))
            })
            .collect();
        lines.push(format!(
            "- Status: {}",
            if dirty.is_empty() {
                "clean".into()
            } else {
                dirty.join(", ")
            }
        ));
        let recent =
            crate::git_probe::git(&root, &["log", "-3", "--pretty=%h %s"], env.clone()).await;
        if !recent.is_empty() {
            lines.push("- Recent commits:".into());
            lines.extend(
                crate::python_value::split_lines(&recent)
                    .into_iter()
                    .map(|line| format!("    {line}")),
            );
        }
    }
    lines.extend(crate::coding_project_facts::project_facts(&root)?);
    Ok(lines.join("\n"))
}

/// Focus may propose a narrower toolset, but the caller must preserve explicit
/// user selections. Pass raw profile config here, matching read_raw_config;
/// merged defaults and plugin-injected server lists are different inputs.
pub fn focus_toolsets(
    mode: &str,
    profile_toolset: Option<&str>,
    raw_config: &Value,
) -> Option<Vec<String>> {
    if mode != "focus" {
        return None;
    }
    let mut selected = vec![profile_toolset?.to_owned()];
    let servers = raw_config.get("mcp_servers").and_then(Value::as_object);
    if let Some(servers) = servers {
        for (name, config) in servers {
            let Some(config) = config.as_object() else {
                continue;
            };
            let enabled = match config.get("enabled") {
                Some(Value::Bool(value)) => *value,
                Some(Value::Number(value)) if value.is_i64() || value.is_u64() => {
                    value.as_i64() != Some(0)
                }
                Some(Value::String(value)) => !["false", "0", "no", "off"].contains(
                    &value
                        .trim_matches(crate::python_value::python_whitespace)
                        .to_lowercase()
                        .as_str(),
                ),
                _ => true,
            };
            if enabled {
                selected.push(name.clone());
            }
        }
    }
    Some(selected)
}

fn text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| crate::python_value::python_repr(value))
}

/// Resolve a loaded config snapshot once per session. Malformed truthy maps
/// remain errors, matching the Python helpers' .get calls. Their caller decides
/// whether failure should disable coding guidance.
pub fn settings(config: &Value) -> anyhow::Result<CodingSettings> {
    let agent = if crate::python_value::truthy(config) {
        config
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("coding config must be a map"))?
            .get("agent")
            .unwrap_or(&Value::Null)
    } else {
        &Value::Null
    };
    let agent = if crate::python_value::truthy(agent) {
        Some(
            agent
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("coding agent config must be a map"))?,
        )
    } else {
        None
    };
    let raw_mode = agent.and_then(|map| map.get("coding_context"));
    let raw_mode = raw_mode.map(text).unwrap_or_else(|| "auto".into());
    let normalized = raw_mode
        .trim_matches(crate::python_value::python_whitespace)
        .to_lowercase();
    let mode = match normalized.as_str() {
        "focus" | "strict" | "lean" => "focus",
        "on" | "true" | "yes" | "1" | "always" => "on",
        "off" | "false" | "no" | "0" | "never" => "off",
        _ => "auto",
    };
    let raw = agent
        .and_then(|map| map.get("coding_instructions"))
        .unwrap_or(&Value::Null);
    let strip = |value: &Value| {
        text(value)
            .trim_matches(crate::python_value::python_whitespace)
            .to_owned()
    };
    let instructions = if let Some(items) = raw.as_array() {
        items
            .iter()
            .map(strip)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    } else if crate::python_value::truthy(raw) {
        strip(raw)
    } else {
        String::new()
    };
    Ok(CodingSettings { mode, instructions })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runtime_posture_keeps_session_settings_and_renders_resolved_workspace() {
        let root =
            std::env::temp_dir().join(format!("hermes-runtime-posture-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        let mut config = serde_json::json!({"agent": {"coding_context": "focus", "coding_instructions": ["Keep tests inline"]}});
        let mode = RuntimeMode::resolve(
            " TUI ",
            &root,
            None,
            Some(&std::env::temp_dir()),
            &config,
            Some("gpt-test"),
        )
        .unwrap();
        assert!(mode.profile().is_coding());
        assert_eq!(mode.surface(), " TUI ");
        assert_eq!(mode.cwd(), root);
        assert_eq!(mode.config_mode(), "focus");
        assert_eq!(
            mode.toolset_selection(&serde_json::json!({"mcp_servers":{"docs":{}}})),
            Some(vec!["coding".into(), "docs".into()])
        );
        config["agent"]["coding_context"] = serde_json::json!("off");
        let parts = mode
            .system_prompt_parts(
                Some(&[]),
                None,
                Some(&std::env::temp_dir()),
                &std::env::vars_os().collect(),
            )
            .await
            .unwrap();
        assert!(!parts.prefix[0].contains("Track multi-step work with `todo_list`"));
        assert!(parts.workspace[0].contains("- Project: Cargo.toml"));
        assert_eq!(
            parts.trailing,
            vec!["Operator instructions (from config):\nKeep tests inline"]
        );
        let fresh = RuntimeMode::resolve(" TUI ", &root, None, None, &config, None).unwrap();
        assert!(!fresh.profile().is_coding());
        assert!(fresh
            .system_prompt_parts(None, None, None, &std::collections::BTreeMap::new())
            .await
            .unwrap()
            .is_empty());
        assert!(fresh.toolset_selection(&Value::Null).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn workspace_snapshot_runs_real_probes_and_includes_project_facts() {
        let root =
            std::env::temp_dir().join(format!("hermes-workspace-snapshot-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let env = std::env::vars_os().collect();
        assert_eq!(
            workspace_snapshot(&root, None, Some(&std::env::temp_dir()), &env)
                .await
                .unwrap(),
            ""
        );
        std::fs::write(root.join("Cargo.toml"), "[package]\nname='sample'\n").unwrap();
        let heading = format!("Workspace (snapshot at session start \u{2014} re-check with `git` before acting on it):\n- Root: {}", root.display());
        assert_eq!(
            workspace_snapshot(&root, None, None, &env).await.unwrap(),
            format!("{heading}\n- Project: Cargo.toml")
        );
        assert!(std::process::Command::new("git")
            .args(["init", "--quiet", "--initial-branch=main"])
            .arg(&root)
            .status()
            .unwrap()
            .success());
        let snapshot = workspace_snapshot(&root, None, None, &env).await.unwrap();
        assert_eq!(
            snapshot,
            format!("{heading}\n- Branch: main\n- Status: 1 untracked\n- Project: Cargo.toml")
        );
        // Exercise actual worktree metadata and commit output. The snapshot
        // must identify shared Git state without revealing the primary path.
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args([
                    "-c",
                    "user.name=Snapshot Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["add", "Cargo.toml"]);
        git(&["commit", "--quiet", "-m", "Initial project"]);
        let commit = git(&["log", "-1", "--pretty=%h %s"]);
        let linked = root.join("linked");
        git(&[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.to_str().unwrap(),
        ]);
        let snapshot = workspace_snapshot(&linked, None, None, &env).await.unwrap();
        let linked_heading = format!("Workspace (snapshot at session start \u{2014} re-check with `git` before acting on it):\n- Root: {}", linked.display());
        assert_eq!(snapshot, format!("{linked_heading}\n- Branch: (detached HEAD)\n- Worktree: linked (git state shared with primary tree)\n- Status: clean\n- Recent commits:\n    {commit}\n- Project: Cargo.toml"));
        std::fs::write(linked.join("Cargo.toml"), "changed\n").unwrap();
        std::fs::write(linked.join("untracked.txt"), "new\n").unwrap();
        assert!(workspace_snapshot(&linked, None, None, &env)
            .await
            .unwrap()
            .contains("- Status: 1 modified, 1 untracked\n"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_status_matches_python() {
        let cases: Value =
            serde_json::from_str(include_str!("../../../tools/coding-status-goldens.json"))
                .unwrap();
        for case in cases.as_array().unwrap() {
            let actual = parse_status(case["input"].as_str().unwrap());
            if case["error"].as_bool() == Some(true) {
                assert!(actual.is_err(), "{case}");
            } else {
                assert_eq!(
                    serde_json::to_value(actual.unwrap()).unwrap(),
                    case["expected"],
                    "{case}"
                );
            }
        }
    }

    #[test]
    fn focus_toolsets_match_python() {
        let cases: Value =
            serde_json::from_str(include_str!("../../../tools/focus-toolsets-goldens.json"))
                .unwrap();
        for case in cases.as_array().unwrap() {
            let actual = focus_toolsets(
                case["mode"].as_str().unwrap(),
                case["toolset"].as_str(),
                &case["raw_config"],
            );
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                case["expected"],
                "{case}"
            );
        }
    }

    #[test]
    fn coding_detection_separates_workspaces_from_notes_and_global_markers() {
        let root =
            std::env::temp_dir().join(format!("hermes-coding-detect-{}", std::process::id()));
        let home = root.join("home");
        let cwd = home.join("notes");
        std::fs::create_dir_all(cwd.join("src/deep")).unwrap();
        std::fs::write(home.join(".git"), "dotfiles").unwrap();
        std::fs::write(home.join("Makefile"), "global").unwrap();
        let detect =
            |mode, platform| detect_coding(mode, platform, &cwd, Some(&home), Some(&root)).unwrap();
        assert!(!detect("auto", "cli"));
        assert!(detect("on", "telegram"));
        std::fs::write(cwd.join(".git"), "notes repo").unwrap();
        std::fs::write(cwd.join("notes.md"), "notes").unwrap();
        assert!(!detect("auto", "cli"));
        std::fs::write(cwd.join("src/deep/main.rs"), "too deep").unwrap();
        assert!(!detect("auto", "cli"));
        std::fs::write(cwd.join("src/main.RS"), "code").unwrap();
        assert!(detect("auto", " CLI\u{001c}"));
        assert!(!detect("auto", "telegram"));
        assert!(!detect("off", "cli"));
        assert!(detect("focus", "desktop"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_match_python() {
        let cases: Value =
            serde_json::from_str(include_str!("../../../tools/coding-settings-goldens.json"))
                .unwrap();
        for case in cases.as_array().unwrap() {
            let actual = settings(&case["config"]);
            if case["error"].as_bool() == Some(true) {
                assert!(actual.is_err(), "{case}");
            } else {
                let actual = actual.unwrap();
                assert_eq!(actual.mode, case["mode"].as_str().unwrap(), "{case}");
                assert_eq!(
                    actual.instructions,
                    case["instructions"].as_str().unwrap(),
                    "{case}"
                );
            }
        }
    }
}
