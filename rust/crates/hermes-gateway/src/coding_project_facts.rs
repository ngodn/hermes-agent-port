//! Bounded coding project facts extraction for Hermes gateway.
//!
//! Ports the project facts extraction from `agent/coding_context.py`:
//!   - `_read_small`: Bounded, non-panicking file reading with 256 KiB size cap,
//!     lossy UTF-8 decoding, and universal newline normalization.
//!   - `ProjectFacts`: Structured facts container (`manifests`, `package_managers`,
//!     `verify_commands`, `context_files`).
//!   - `detect_project_facts`: Stat and bounded file reads to detect manifests,
//!     lockfile/package-manager preferences, verify commands (scripts/run_tests.sh,
//!     package.json scripts, pytest, Makefile targets), and context files.
//!   - `_project_facts`: Formats detected facts into session snapshot lines
//!     for the system prompt (`- Project: ...`, `- Verify: ...`, `- Context files: ...`).
//!   - `project_facts_for`: Structured facts mapping for UI/gateway (`project.facts`).
//!
//! # Architecture & Bounded Invariants
//!
//! - **Bounded file size**: No file larger than 256 KiB (`MAX_FACT_FILE_BYTES`) is read.
//! - **Bounded command count**: At most 8 verify commands (`MAX_VERIFY_COMMANDS`).
//! - **Prompt cache stability**: Snapshot output ordering is deterministic and byte-stable.
//! - **Safe I/O**: Missing files, directories, permission errors, and malformed JSON
//!   never panic and gracefully degrade to empty defaults.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Constants (verbatim from agent/coding_context.py)
// ---------------------------------------------------------------------------

/// Maximum file size read for facts inspection (256 KiB = 262,144 bytes).
pub const MAX_FACT_FILE_BYTES: u64 = 256 * 1024;

/// Maximum number of verify commands returned by `detect_project_facts`.
pub const MAX_VERIFY_COMMANDS: usize = 8;

/// Maximum number of manifests rendered in the prompt's `- Project: ...` line.
pub const MAX_PROJECT_MANIFESTS_RENDERED: usize = 6;

/// Project-root signals that mark a directory as a code workspace even when
/// it is not a git repo. Verbatim from `agent/coding_context.py::_PROJECT_MARKERS`.
pub const PROJECT_MARKERS: &[&str] = &[
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "package.json",
    "tsconfig.json",
    "deno.json",
    "Cargo.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Gemfile",
    "composer.json",
    "mix.exs",
    "pubspec.yaml",
    "CMakeLists.txt",
    "Makefile",
    "Dockerfile",
    "AGENTS.md",
    "CLAUDE.md",
    ".cursorrules",
];

/// Agent-instruction files surfaced separately from manifests in the snapshot.
/// Verbatim from `agent/coding_context.py::_CONTEXT_FILES`.
pub const CONTEXT_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".cursorrules"];

/// Candidate project manifests in priority order (`_PROJECT_MARKERS` excluding `_CONTEXT_FILES`).
pub const PROJECT_MANIFESTS: &[&str] = &[
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "package.json",
    "tsconfig.json",
    "deno.json",
    "Cargo.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Gemfile",
    "composer.json",
    "mix.exs",
    "pubspec.yaml",
    "CMakeLists.txt",
    "Makefile",
    "Dockerfile",
];

/// Python lockfile -> package manager mappings in priority order.
/// Verbatim from `agent/coding_context.py::_PY_LOCKFILES`.
pub const PY_LOCKFILES: &[(&str, &str)] = &[
    ("uv.lock", "uv"),
    ("poetry.lock", "poetry"),
    ("Pipfile.lock", "pipenv"),
];

/// JavaScript/TypeScript lockfile -> package manager mappings in priority order.
/// Verbatim from `agent/coding_context.py::_JS_LOCKFILES`.
pub const JS_LOCKFILES: &[(&str, &str)] = &[
    ("pnpm-lock.yaml", "pnpm"),
    ("bun.lockb", "bun"),
    ("bun.lock", "bun"),
    ("yarn.lock", "yarn"),
    ("package-lock.json", "npm"),
];

/// package.json scripts / Makefile targets worth surfacing as verify commands.
/// Verbatim from `agent/coding_context.py::_VERIFY_TARGETS`.
pub const VERIFY_TARGETS: &[&str] = &[
    "test",
    "tests",
    "lint",
    "typecheck",
    "check",
    "build",
    "fmt",
    "format",
];

/// Precompiled regex patterns for Makefile target detection: `^{name}\s*:`.
static MAKEFILE_PATTERNS: LazyLock<Vec<(&'static str, fancy_regex::Regex)>> = LazyLock::new(|| {
    VERIFY_TARGETS
        .iter()
        .map(|&target| {
            let pat = format!(r"(?m)^{}[\s\x1c-\x1f]*:", fancy_regex::escape(target));
            let re = fancy_regex::Regex::new(&pat).expect("valid Makefile target regex");
            (target, re)
        })
        .collect()
});

// ---------------------------------------------------------------------------
// File Reading Helper
// ---------------------------------------------------------------------------

/// Read a small text file, or `""` — never raises, never reads huge files.
///
/// Ports `agent/coding_context.py::_read_small`.
///
/// Exact reference semantics:
/// - If `path` does not exist or is not a regular file (e.g. directory), returns `""`.
/// - If `path.stat().st_size > MAX_FACT_FILE_BYTES` (256 KiB), returns `""`.
/// - If reading fails for any I/O reason (permissions, broken symlink, etc.), returns `""`.
/// - Invalid UTF-8 bytes are replaced with `\u{FFFD}` (lossy conversion).
/// - Universal newlines: replaces `\r\n` and lone `\r` with `\n`, matching Python's
///   default `read_text(encoding="utf-8", errors="replace")`.
pub fn read_small(path: &Path) -> String {
    let Ok(meta) = std::fs::metadata(path) else {
        return String::new();
    };
    if !meta.is_file() || meta.len() > MAX_FACT_FILE_BYTES {
        return String::new();
    }
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    if bytes.len() > MAX_FACT_FILE_BYTES as usize {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    if text.contains('\r') {
        normalize_universal_newlines(&text)
    } else {
        text.into_owned()
    }
}

/// Normalize CRLF (`\r\n`) and lone CR (`\r`) to LF (`\n`), matching Python universal newlines.
fn normalize_universal_newlines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// ProjectFacts Data Structure
// ---------------------------------------------------------------------------

/// Structured project facts — the model's verify loop, detected once.
///
/// Ports `agent/coding_context.py::ProjectFacts`.
///
/// The same data that feeds the workspace snapshot, exposed structurally so
/// non-prompt consumers (e.g. the desktop verify UI) read it instead of
/// re-detecting and drifting from the prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFacts {
    #[serde(default)]
    pub manifests: Vec<String>,
    #[serde(default, alias = "packageManagers")]
    pub package_managers: Vec<String>,
    #[serde(default, alias = "verifyCommands")]
    pub verify_commands: Vec<String>,
    #[serde(default, alias = "contextFiles")]
    pub context_files: Vec<String>,
}

impl ProjectFacts {
    /// Construct a new `ProjectFacts` instance.
    pub fn new(
        manifests: Vec<String>,
        package_managers: Vec<String>,
        verify_commands: Vec<String>,
        context_files: Vec<String>,
    ) -> Self {
        Self {
            manifests,
            package_managers,
            verify_commands,
            context_files,
        }
    }

    /// Render structured facts as workspace snapshot lines for the system prompt.
    ///
    /// Ports `agent/coding_context.py::_project_facts`.
    ///
    /// Formats up to 6 manifests, package manager list, verify commands, and
    /// context files into stable prompt lines.
    pub fn render_lines(&self) -> Vec<String> {
        let mut facts = Vec::new();

        if !self.manifests.is_empty() {
            let limit = self.manifests.len().min(MAX_PROJECT_MANIFESTS_RENDERED);
            let mut line = format!("- Project: {}", self.manifests[..limit].join(", "));
            if !self.package_managers.is_empty() {
                line.push_str(&format!(" ({})", self.package_managers.join("/")));
            }
            facts.push(line);
        }

        if !self.verify_commands.is_empty() {
            facts.push(format!("- Verify: {}", self.verify_commands.join("; ")));
        }

        if !self.context_files.is_empty() {
            facts.push(format!(
                "- Context files: {}",
                self.context_files.join(", ")
            ));
        }

        facts
    }

    /// Format structured facts as a JSON Value for desktop/gateway UI consumption.
    ///
    /// The caller resolves the workspace root before invoking this helper.
    pub fn to_gateway_json(&self, root: &Path) -> serde_json::Value {
        serde_json::json!({
            "root": root.to_string_lossy(),
            "manifests": self.manifests,
            "packageManagers": self.package_managers,
            "verifyCommands": self.verify_commands,
            "contextFiles": self.context_files,
        })
    }

    /// True if all fact categories are empty.
    pub fn is_empty(&self) -> bool {
        self.manifests.is_empty()
            && self.package_managers.is_empty()
            && self.verify_commands.is_empty()
            && self.context_files.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Detection Implementation
// ---------------------------------------------------------------------------

/// Detect manifests, package manager(s), verify commands, and context files.
///
/// Ports `agent/coding_context.py::detect_project_facts`.
///
/// Cheap: stat calls plus reads of a couple of small files. The single source
/// of truth for both the prompt snapshot (`project_facts`) and the
/// gateway's `project.facts` — so the UI never re-sniffs verify commands.
pub fn detect_project_facts(root: &Path) -> anyhow::Result<ProjectFacts> {
    // 1. Manifests: PROJECT_MARKERS in order, excluding CONTEXT_FILES, that are files
    let mut manifests = Vec::new();
    for &marker in PROJECT_MARKERS {
        if !CONTEXT_FILES.contains(&marker) && root.join(marker).is_file() {
            manifests.push(marker.to_string());
        }
    }

    // 2. Package managers: PY_LOCKFILES then JS_LOCKFILES in order, deduplicated
    let mut package_managers = Vec::new();
    for &(lockfile, pm) in PY_LOCKFILES.iter().chain(JS_LOCKFILES.iter()) {
        if root.join(lockfile).is_file() && !package_managers.iter().any(|existing| existing == pm)
        {
            package_managers.push(pm.to_string());
        }
    }

    // 3. Verify commands
    let mut verify = Vec::new();

    // 3a. scripts/run_tests.sh
    if root.join("scripts").join("run_tests.sh").is_file() {
        verify.push("scripts/run_tests.sh".to_string());
    }

    // 3b. package.json scripts
    let package_json_path = root.join("package.json");
    if package_json_path.is_file() {
        let content = read_small(&package_json_path);
        let raw_json = if content.is_empty() { "{}" } else { &content };
        let scripts_val = serde_json::from_str::<serde_json::Value>(raw_json)
            .ok()
            .and_then(|val| {
                let obj = val.as_object()?;
                let scripts = obj.get("scripts")?;
                if scripts.is_null() {
                    None
                } else {
                    Some(scripts.clone())
                }
            });

        if let Some(scripts) = scripts_val {
            anyhow::ensure!(
                !crate::python_value::truthy(&scripts)
                    || scripts.is_object()
                    || scripts.is_array()
                    || scripts.is_string(),
                "package scripts is not iterable"
            );
            // JS package manager preference: first matching JS_LOCKFILES entry, or "npm"
            let js_pm = JS_LOCKFILES
                .iter()
                .find(|(lock, _)| root.join(lock).is_file())
                .map(|(_, pm)| *pm)
                .unwrap_or("npm");

            for &name in VERIFY_TARGETS {
                let found = match &scripts {
                    serde_json::Value::Object(map) => map.contains_key(name),
                    serde_json::Value::Array(list) => {
                        list.iter().any(|item| item.as_str() == Some(name))
                    }
                    serde_json::Value::String(s) => s.contains(name),
                    _ => false,
                };
                if found {
                    verify.push(format!("{js_pm} run {name}"));
                }
            }
        }
    }

    // 3c. pytest: pytest.ini OR "[tool.pytest" in pyproject.toml
    let has_pytest_ini = root.join("pytest.ini").is_file();
    let has_tool_pytest = if !has_pytest_ini {
        let pyproject_content = read_small(&root.join("pyproject.toml"));
        pyproject_content.contains("[tool.pytest")
    } else {
        false
    };
    if has_pytest_ini || has_tool_pytest {
        verify.push("pytest".to_string());
    }

    // 3d. Makefile targets matching ^name\s*:
    let makefile_path = root.join("Makefile");
    let makefile = read_small(&makefile_path);
    if !makefile.is_empty() {
        for (name, re) in MAKEFILE_PATTERNS.iter() {
            if re.is_match(&makefile).unwrap_or(false) {
                verify.push(format!("make {name}"));
            }
        }
    }

    // Deduplicate verify commands while preserving insertion order, capped at MAX_VERIFY_COMMANDS
    let mut unique_verify = Vec::new();
    for cmd in verify {
        if !unique_verify.contains(&cmd) {
            unique_verify.push(cmd);
            if unique_verify.len() >= MAX_VERIFY_COMMANDS {
                break;
            }
        }
    }

    // 4. Context files in CONTEXT_FILES order
    let mut context_files = Vec::new();
    for &context_file in CONTEXT_FILES {
        if root.join(context_file).is_file() {
            context_files.push(context_file.to_string());
        }
    }

    Ok(ProjectFacts {
        manifests,
        package_managers,
        verify_commands: unique_verify,
        context_files,
    })
}

/// Render `detect_project_facts` as workspace-snapshot lines.
///
/// Ports `agent/coding_context.py::_project_facts`.
///
/// Hands the model its verify loop up front — which manifest, which package
/// manager, and the exact test/lint/build commands — instead of making it
/// rediscover them every session. Built once at prompt-build time; the string
/// output must stay byte-stable to preserve the prompt cache.
pub fn project_facts(root: &Path) -> anyhow::Result<Vec<String>> {
    Ok(detect_project_facts(root)?.render_lines())
}

/// Structured project facts for `root` — `None` outside a workspace.
///
/// The caller resolves the workspace root before invoking this helper.
pub fn facts_for_root(root: Option<&Path>) -> anyhow::Result<Option<serde_json::Value>> {
    let Some(root) = root else {
        return Ok(None);
    };
    let facts = detect_project_facts(root)?;
    Ok(Some(facts.to_gateway_json(root)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn decode_hex(s: &str) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(s.len() / 2);
        let chars = s.as_bytes();
        let mut i = 0;
        while i + 1 < chars.len() {
            if let Ok(b) =
                u8::from_str_radix(std::str::from_utf8(&chars[i..i + 2]).unwrap_or(""), 16)
            {
                bytes.push(b);
            }
            i += 2;
        }
        bytes
    }

    struct TempDirGuard(std::path::PathBuf);

    impl TempDirGuard {
        fn new(name: &str) -> Self {
            let count = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "hermes-facts-test-{}-{}-{}",
                std::process::id(),
                name,
                count
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Deserialize)]
    struct GoldenConstants {
        project_markers: Vec<String>,
        context_files: Vec<String>,
        project_manifests: Vec<String>,
        py_lockfiles: Vec<(String, String)>,
        js_lockfiles: Vec<(String, String)>,
        verify_targets: Vec<String>,
        max_verify_commands: usize,
        max_fact_file_bytes: u64,
        max_project_manifests_rendered: usize,
    }

    #[derive(Deserialize)]
    struct GoldenReadSmallCase {
        id: String,
        description: String,
        content_hex: Option<String>,
        #[serde(default)]
        size_exact: Option<usize>,
        is_dir: bool,
        exists: bool,
        expected: String,
    }

    #[derive(Deserialize)]
    struct GoldenProjectFactsExpected {
        manifests: Vec<String>,
        package_managers: Vec<String>,
        verify_commands: Vec<String>,
        context_files: Vec<String>,
        lines: Vec<String>,
    }

    #[derive(Deserialize)]
    struct GoldenProjectFactsCase {
        id: String,
        description: String,
        files: std::collections::HashMap<String, String>,
        dirs: Vec<String>,
        expected: GoldenProjectFactsExpected,
    }

    #[derive(Deserialize)]
    struct GoldensRoot {
        constants: GoldenConstants,
        read_small_cases: Vec<GoldenReadSmallCase>,
        project_facts_cases: Vec<GoldenProjectFactsCase>,
    }

    fn load_goldens() -> GoldensRoot {
        let text = include_str!("../../../tools/coding-project-facts-goldens.json");
        serde_json::from_str(text).expect("valid coding-project-facts-goldens.json")
    }

    #[test]
    fn test_constants_match_goldens() {
        let goldens = load_goldens();
        assert_eq!(
            PROJECT_MARKERS,
            goldens
                .constants
                .project_markers
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .as_slice()
        );
        assert_eq!(
            CONTEXT_FILES,
            goldens
                .constants
                .context_files
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .as_slice()
        );
        assert_eq!(
            PROJECT_MANIFESTS,
            goldens
                .constants
                .project_manifests
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .as_slice()
        );

        let py_goldens: Vec<(&str, &str)> = goldens
            .constants
            .py_lockfiles
            .iter()
            .map(|(l, p)| (l.as_str(), p.as_str()))
            .collect();
        assert_eq!(PY_LOCKFILES, py_goldens.as_slice());

        let js_goldens: Vec<(&str, &str)> = goldens
            .constants
            .js_lockfiles
            .iter()
            .map(|(l, p)| (l.as_str(), p.as_str()))
            .collect();
        assert_eq!(JS_LOCKFILES, js_goldens.as_slice());

        assert_eq!(
            VERIFY_TARGETS,
            goldens
                .constants
                .verify_targets
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .as_slice()
        );
        assert_eq!(MAX_VERIFY_COMMANDS, goldens.constants.max_verify_commands);
        assert_eq!(MAX_FACT_FILE_BYTES, goldens.constants.max_fact_file_bytes);
        assert_eq!(
            MAX_PROJECT_MANIFESTS_RENDERED,
            goldens.constants.max_project_manifests_rendered
        );
    }

    #[test]
    fn test_read_small_golden_cases() {
        let goldens = load_goldens();
        for case in goldens.read_small_cases {
            let tmp = TempDirGuard::new(&case.id);
            let target_path = tmp.path().join("target_file");

            if !case.exists {
                // Do not create the file
                let actual = read_small(&target_path);
                assert_eq!(actual, case.expected, "Case failed: {}", case.id);
                continue;
            }

            if case.is_dir {
                std::fs::create_dir_all(&target_path).unwrap();
                let actual = read_small(&target_path);
                assert_eq!(actual, case.expected, "Case failed: {}", case.id);
                continue;
            }

            if let Some(size) = case.size_exact {
                let data = vec![b'x'; size];
                std::fs::write(&target_path, &data).unwrap();
            } else if let Some(ref hex_str) = case.content_hex {
                let data = decode_hex(hex_str);
                std::fs::write(&target_path, &data).unwrap();
            } else {
                panic!("Case {} must specify size_exact or content_hex", case.id);
            }

            let actual = read_small(&target_path);
            assert_eq!(actual, case.expected, "Case failed: {}", case.id);
        }
    }

    #[test]
    fn test_project_facts_golden_cases() {
        let goldens = load_goldens();
        for case in goldens.project_facts_cases {
            let tmp = TempDirGuard::new(&case.id);
            let root = tmp.path();

            for dir in &case.dirs {
                std::fs::create_dir_all(root.join(dir)).unwrap();
            }

            for (rel_path, content) in &case.files {
                let file_path = root.join(rel_path);
                if let Some(parent) = file_path.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(&file_path, content).unwrap();
            }

            let actual_facts = detect_project_facts(root).unwrap();
            let actual_lines = project_facts(root).unwrap();

            assert_eq!(
                actual_facts.manifests, case.expected.manifests,
                "Manifests mismatch for case '{}': {}",
                case.id, case.description
            );
            assert_eq!(
                actual_facts.package_managers, case.expected.package_managers,
                "Package managers mismatch for case '{}': {}",
                case.id, case.description
            );
            assert_eq!(
                actual_facts.verify_commands, case.expected.verify_commands,
                "Verify commands mismatch for case '{}': {}",
                case.id, case.description
            );
            assert_eq!(
                actual_facts.context_files, case.expected.context_files,
                "Context files mismatch for case '{}': {}",
                case.id, case.description
            );
            assert_eq!(
                actual_lines, case.expected.lines,
                "Snapshot lines mismatch for case '{}': {}",
                case.id, case.description
            );

            // Also test to_gateway_json consistency
            let gateway_json = actual_facts.to_gateway_json(root);
            assert_eq!(
                gateway_json["manifests"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect::<Vec<_>>(),
                case.expected.manifests
            );
            assert_eq!(
                gateway_json["packageManagers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect::<Vec<_>>(),
                case.expected.package_managers
            );
            assert_eq!(
                gateway_json["verifyCommands"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect::<Vec<_>>(),
                case.expected.verify_commands
            );
            assert_eq!(
                gateway_json["contextFiles"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect::<Vec<_>>(),
                case.expected.context_files
            );
        }
    }

    #[test]
    fn test_empty_facts_and_helpers() {
        let facts = ProjectFacts::default();
        assert!(facts.is_empty());
        assert!(facts.render_lines().is_empty());

        let tmp = TempDirGuard::new("none_workspace");
        assert!(facts_for_root(None).unwrap().is_none());
        let gw = facts_for_root(Some(tmp.path())).unwrap().unwrap();
        assert_eq!(gw["root"], tmp.path().to_string_lossy().as_ref());
        assert_eq!(gw["manifests"], serde_json::json!([]));
    }

    #[test]
    fn malformed_scripts_and_python_makefile_whitespace() {
        let tmp = TempDirGuard::new("typed_scripts");
        for scripts in [
            serde_json::json!(true),
            serde_json::json!(1),
            serde_json::json!(1.5),
        ] {
            std::fs::write(
                tmp.path().join("package.json"),
                serde_json::json!({"scripts": scripts}).to_string(),
            )
            .unwrap();
            assert!(detect_project_facts(tmp.path()).is_err());
        }
        for scripts in [serde_json::json!(false), serde_json::json!(0)] {
            std::fs::write(
                tmp.path().join("package.json"),
                serde_json::json!({"scripts": scripts}).to_string(),
            )
            .unwrap();
            assert!(detect_project_facts(tmp.path())
                .unwrap()
                .verify_commands
                .is_empty());
        }
        std::fs::write(
            tmp.path().join("Makefile"),
            "test\u{001c}:\n\ttrue\nlint\u{001f}:\n\ttrue\n",
        )
        .unwrap();
        assert_eq!(
            detect_project_facts(tmp.path()).unwrap().verify_commands,
            vec!["make test", "make lint"]
        );
    }
}
