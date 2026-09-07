//! Project instruction discovery, following agent/prompt_builder.py.
#![allow(dead_code)]

use std::io;
use std::path::{Path, PathBuf};

/// A detached reader bounds startup latency even when the filesystem stalls.
/// Normalize Python text-mode newlines before stripping boundary whitespace.
pub async fn read_text(path: &Path, timeout: std::time::Duration) -> Option<String> {
    let read_path = path.to_owned();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("context-read".into())
        .spawn(move || {
            let _ = sender.send(std::fs::read_to_string(read_path));
        })
        .ok()?;
    let text = match tokio::time::timeout(timeout, receiver).await {
        Ok(Ok(Ok(text))) => text,
        Err(_) => {
            tracing::warn!(path = %path.display(), "Context file read timed out; skipping");
            return None;
        }
        _ => return None,
    };
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim_matches(crate::python_value::python_whitespace);
    (!text.is_empty()).then(|| text.to_owned())
}

/// Text and warnings belong to one build, never to shared process state.
#[derive(Default)]
pub struct LoadedContext {
    pub text: String,
    pub warnings: Vec<String>,
}

/// Paths and settings resolved for the owning agent. Keep launch cwd distinct
/// from an explicit workspace so a daemon's install directory gains no prompt
/// authority merely because the process started there.
pub struct ContextRequest<'a> {
    pub cwd: Option<&'a Path>,
    pub launch_cwd: &'a Path,
    pub install_root: &'a Path,
    pub home: &'a Path,
    pub skip_soul: bool,
    pub allow_install_tree_fallback: bool,
    pub max_chars: usize,
    pub read_timeout: std::time::Duration,
}

/// Wrap one project source and the independent profile identity. The caller
/// passes skip_soul when identity already occupied the initial stable slot.
pub async fn build_context(request: &ContextRequest<'_>) -> io::Result<LoadedContext> {
    let cwd = resolve(request.cwd.unwrap_or(request.launch_cwd))?;
    let in_install = resolve(request.install_root)
        .map(|root| cwd.starts_with(root))
        .unwrap_or(false);
    let suppress_project =
        request.cwd.is_none() && !request.allow_install_tree_fallback && in_install;
    let mut result = if suppress_project {
        tracing::warn!(cwd = %cwd.display(), "Skipping fallback project context inside Hermes install tree");
        LoadedContext::default()
    } else {
        load_project(&cwd, request.max_chars, request.read_timeout).await?
    };
    let mut sections = Vec::new();
    if !result.text.is_empty() {
        sections.push(std::mem::take(&mut result.text));
    }
    if !request.skip_soul {
        let (soul, warning) =
            crate::system_prompt::load_soul(request.home, request.max_chars, request.read_timeout)
                .await;
        if let Some(soul) = soul.filter(|text| !text.is_empty()) {
            sections.push(soul);
        }
        result.warnings.extend(warning);
    }
    if !sections.is_empty() {
        result.text = "# Project Context\n\nThe following project context files have been loaded and should be followed:\n\n".to_owned() + &sections.join("\n");
    }
    Ok(result)
}

fn truncate(
    text: &str,
    label: &str,
    path: &Path,
    limit: usize,
    warnings: &mut Vec<String>,
) -> String {
    let (text, warning) =
        crate::system_prompt::truncate_context_content(text, label, limit, path.to_str());
    warnings.extend(warning);
    text
}

/// Merge one usable spelling per directory, deduplicating raw stripped text
/// before scanning. A duplicate override still prevents lower-priority names.
pub async fn load_agents(
    cwd: &Path,
    limit: usize,
    timeout: std::time::Duration,
) -> io::Result<LoadedContext> {
    let cwd = resolve(cwd)?;
    let mut result = LoadedContext::default();
    let mut seen = std::collections::HashSet::new();
    let mut sections = Vec::new();
    for directory in agents_directory_chain(&cwd)? {
        for name in ["AGENTS.override.md", "AGENTS.md", "agents.md"] {
            let candidate = directory.join(name);
            let Some(content) = read_text(&candidate, timeout).await else {
                continue;
            };
            if !seen.insert(content.clone()) {
                break;
            }
            // Every directory in the chain is an ancestor of the resolved cwd.
            let depth = cwd
                .strip_prefix(&directory)
                .expect("ancestor chain")
                .components()
                .count();
            let label = format!("{}{name}", "../".repeat(depth));
            let scanned = crate::system_prompt::scan_context_content(&content, &label);
            let section = format!("## {label}\n\n{scanned}");
            sections.push(truncate(
                &section,
                &label,
                &candidate,
                limit,
                &mut result.warnings,
            ));
            break;
        }
    }
    result.text = match sections.len() {
        0 => String::new(),
        1 => sections.pop().unwrap(),
        _ => truncate(
            &sections.join("\n\n"),
            "AGENTS.md (directory chain)",
            &cwd.join("AGENTS.md"),
            limit,
            &mut result.warnings,
        ),
    };
    Ok(result)
}

/// Discover one Hermes instruction file, then strip its frontmatter before
/// scanning. Ancestor files use the basename label, matching Python's fallback
/// when Path.relative_to(cwd) cannot describe an ancestor.
pub async fn load_hermes(
    cwd: &Path,
    limit: usize,
    timeout: std::time::Duration,
) -> io::Result<LoadedContext> {
    let mut result = LoadedContext::default();
    let Some(path) = find_hermes_md(cwd)? else {
        return Ok(result);
    };
    let Some(content) = read_text(&path, timeout).await else {
        return Ok(result);
    };
    let label = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(".hermes.md");
    let scanned = crate::system_prompt::scan_context_content(strip_frontmatter(&content), label);
    result.text = truncate(
        &format!("## {label}\n\n{scanned}"),
        ".hermes.md",
        &path,
        limit,
        &mut result.warnings,
    );
    Ok(result)
}

pub(crate) fn resolve(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let text = absolute
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "context path is not UTF-8"))?;
    crate::file_read_safety::realpath_abs(text.to_owned())
}

/// CLAUDE instructions are cwd-only, with uppercase spelling preferred.
pub async fn load_claude(cwd: &Path, limit: usize, timeout: std::time::Duration) -> LoadedContext {
    let mut result = LoadedContext::default();
    for name in ["CLAUDE.md", "claude.md"] {
        let path = cwd.join(name);
        if let Some(content) = read_text(&path, timeout).await {
            let scanned = crate::system_prompt::scan_context_content(&content, name);
            result.text = truncate(
                &format!("## {name}\n\n{scanned}"),
                "CLAUDE.md",
                &path,
                limit,
                &mut result.warnings,
            );
            break;
        }
    }
    result
}

/// Cursor files share one combined budget. Preserve the trailing double
/// newline and sort immediate .mdc entries before reading, as Python does.
pub async fn load_cursor(
    cwd: &Path,
    limit: usize,
    timeout: std::time::Duration,
) -> io::Result<LoadedContext> {
    let mut result = LoadedContext::default();
    let legacy = cwd.join(".cursorrules");
    let mut files = vec![(legacy.clone(), ".cursorrules".to_owned())];
    let directory = cwd.join(".cursor/rules");
    if directory.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&directory)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".mdc"))
            })
            .collect();
        entries.sort();
        for path in entries {
            let label = format!(
                ".cursor/rules/{}",
                path.file_name().unwrap().to_string_lossy()
            );
            files.push((path, label));
        }
    }
    let mut combined = String::new();
    for (path, label) in files {
        if let Some(content) = read_text(&path, timeout).await {
            let scanned = crate::system_prompt::scan_context_content(&content, &label);
            combined.push_str(&format!("## {label}\n\n{scanned}\n\n"));
        }
    }
    if !combined.is_empty() {
        result.text = truncate(
            &combined,
            ".cursorrules",
            &legacy,
            limit,
            &mut result.warnings,
        );
    }
    Ok(result)
}

/// Load exactly one project instruction type. Higher-priority empty or failed
/// reads permit fallback; nonempty blocked markers still occupy their slot.
pub async fn load_project(
    cwd: &Path,
    limit: usize,
    timeout: std::time::Duration,
) -> io::Result<LoadedContext> {
    let hermes = load_hermes(cwd, limit, timeout).await?;
    if !hermes.text.is_empty() {
        return Ok(hermes);
    }
    let agents = load_agents(cwd, limit, timeout).await?;
    if !agents.text.is_empty() {
        return Ok(agents);
    }
    let claude = load_claude(cwd, limit, timeout).await;
    if !claude.text.is_empty() {
        return Ok(claude);
    }
    load_cursor(cwd, limit, timeout).await
}

/// A .git file counts too, allowing worktrees and submodules to define roots.
pub fn find_git_root(start: &Path) -> io::Result<Option<PathBuf>> {
    Ok(resolve(start)?
        .ancestors()
        .find(|path| path.join(".git").exists())
        .map(Path::to_owned))
}

/// The nearest instruction file wins, with the hidden spelling preferred at
/// each level. Without a repository root, never inherit from parent folders.
pub fn find_hermes_md(cwd: &Path) -> io::Result<Option<PathBuf>> {
    let current = resolve(cwd)?;
    let root = find_git_root(&current)?;
    for directory in current.ancestors() {
        for name in [".hermes.md", "HERMES.md"] {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Ok(Some(candidate));
            }
        }
        if root.is_none() || root.as_deref() == Some(directory) {
            break;
        }
    }
    Ok(None)
}

/// AGENTS instructions accumulate from the repository root down to cwd.
pub fn agents_directory_chain(cwd: &Path) -> io::Result<Vec<PathBuf>> {
    let current = resolve(cwd)?;
    let Some(root) = find_git_root(&current)? else {
        return Ok(vec![current]);
    };
    let mut chain = Vec::new();
    for directory in current.ancestors() {
        chain.push(directory.to_owned());
        if directory == root {
            break;
        }
    }
    chain.reverse();
    Ok(chain)
}

/// Match the reference's delimiter slicing rather than interpreting YAML.
/// An empty body preserves the original document after leading BOM removal.
pub fn strip_frontmatter(content: &str) -> &str {
    let content = content.trim_start_matches('\u{feff}');
    if let Some(rest) = content.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let body = content[end + 7..].trim_start_matches('\n');
            if !body.is_empty() {
                return body;
            }
        }
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn context_wrapper_guards_fallback_install_tree_without_suppressing_soul() {
        let root =
            std::env::temp_dir().join(format!("hermes-context-wrapper-{}", std::process::id()));
        let install = root.join("install");
        let child = install.join("child");
        let sibling = root.join("install-other");
        let home = root.join("profile");
        for path in [&root, &install, &child, &sibling, &home] {
            std::fs::create_dir_all(path).unwrap();
            std::fs::write(path.join("CLAUDE.md"), "project").unwrap();
        }
        std::fs::write(home.join("SOUL.md"), "persona").unwrap();
        for (launch, inside) in [
            (&root, false),
            (&install, true),
            (&child, true),
            (&sibling, false),
        ] {
            for explicit in [false, true] {
                for allow in [false, true] {
                    for skip_soul in [false, true] {
                        let loaded = build_context(&ContextRequest {
                            cwd: explicit.then_some(launch.as_path()),
                            launch_cwd: launch,
                            install_root: &install,
                            home: &home,
                            skip_soul,
                            allow_install_tree_fallback: allow,
                            max_chars: 20000,
                            read_timeout: std::time::Duration::from_secs(2),
                        })
                        .await
                        .unwrap();
                        let project = explicit || allow || !inside;
                        let mut expected = Vec::new();
                        if project {
                            expected.push("## CLAUDE.md\n\nproject");
                        }
                        if !skip_soul {
                            expected.push("persona");
                        }
                        let expected = if expected.is_empty() {
                            String::new()
                        } else {
                            format!("# Project Context\n\nThe following project context files have been loaded and should be followed:\n\n{}", expected.join("\n"))
                        };
                        assert_eq!(loaded.text, expected);
                        assert!(loaded.warnings.is_empty());
                    }
                }
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn project_type_precedence_and_cursor_order() {
        let root =
            std::env::temp_dir().join(format!("hermes-context-types-{}", std::process::id()));
        let rules = root.join(".cursor/rules");
        std::fs::create_dir_all(&rules).unwrap();
        for (path, text) in [
            (root.join(".cursorrules"), "legacy"),
            (rules.join("z.mdc"), "last"),
            (rules.join("a.mdc"), "first"),
            (rules.join("ignored.md"), "ignore"),
        ] {
            std::fs::write(path, text).unwrap();
        }
        let timeout = std::time::Duration::from_secs(2);
        let cursor = load_project(&root, 20000, timeout).await.unwrap();
        assert_eq!(cursor.text, "## .cursorrules\n\nlegacy\n\n## .cursor/rules/a.mdc\n\nfirst\n\n## .cursor/rules/z.mdc\n\nlast\n\n");
        let capped = load_cursor(&root, 40, timeout).await.unwrap();
        assert_eq!(capped.warnings.len(), 1);
        for (file, text, expected) in [
            ("claude.md", "lower", "## claude.md\n\nlower"),
            ("CLAUDE.md", "upper", "## CLAUDE.md\n\nupper"),
            ("AGENTS.md", "agents", "## AGENTS.md\n\nagents"),
            (".hermes.md", "hermes", "## .hermes.md\n\nhermes"),
        ] {
            std::fs::write(root.join(file), text).unwrap();
            assert_eq!(
                load_project(&root, 20000, timeout).await.unwrap().text,
                expected
            );
        }
        std::fs::write(root.join(".hermes.md"), " \n").unwrap();
        assert_eq!(
            load_project(&root, 20000, timeout).await.unwrap().text,
            "## AGENTS.md\n\nagents"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn loaded_instructions_preserve_precedence_deduplication_and_budgets() {
        let root = std::env::temp_dir().join(format!("hermes-context-load-{}", std::process::id()));
        let cwd = root.join("child");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(root.join(".git"), "worktree").unwrap();
        std::fs::write(root.join("AGENTS.override.md"), "shared").unwrap();
        std::fs::write(root.join("AGENTS.md"), "must not load root fallback").unwrap();
        std::fs::write(cwd.join("AGENTS.override.md"), " shared\r\n").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "must not load duplicate fallback").unwrap();
        let timeout = std::time::Duration::from_secs(2);
        let loaded = load_agents(&cwd, 20000, timeout).await.unwrap();
        assert_eq!(loaded.text, "## ../AGENTS.override.md\n\nshared");
        assert!(loaded.warnings.is_empty());
        std::fs::write(cwd.join("AGENTS.override.md"), " \n").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "child").unwrap();
        let loaded = load_agents(&cwd, 20000, timeout).await.unwrap();
        assert_eq!(
            loaded.text,
            "## ../AGENTS.override.md\n\nshared\n\n## AGENTS.md\n\nchild"
        );
        let capped = load_agents(&cwd, 40, timeout).await.unwrap();
        assert_eq!(capped.warnings.len(), 1);
        assert!(capped
            .text
            .contains("truncated AGENTS.md (directory chain)"));
        assert!(capped
            .text
            .contains(cwd.join("AGENTS.md").to_str().unwrap()));
        std::fs::write(
            root.join(".hermes.md"),
            "---\nmodel: ignored\n---\nbody\r\n",
        )
        .unwrap();
        let loaded = load_hermes(&cwd, 20000, timeout).await.unwrap();
        assert_eq!(loaded.text, "## .hermes.md\n\nbody");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovery_stops_at_repository_and_preserves_directory_precedence() {
        let root =
            std::env::temp_dir().join(format!("hermes-context-discovery-{}", std::process::id()));
        let repo = root.join("repo");
        let cwd = repo.join("a/b");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(root.join(".hermes.md"), "outside").unwrap();
        assert_eq!(find_hermes_md(&cwd).unwrap(), None);
        assert_eq!(agents_directory_chain(&cwd).unwrap(), vec![cwd.clone()]);
        std::fs::write(repo.join(".git"), "gitdir: /worktree/metadata").unwrap();
        assert_eq!(find_git_root(&cwd).unwrap(), Some(repo.clone()));
        assert_eq!(
            agents_directory_chain(&cwd).unwrap(),
            vec![repo.clone(), repo.join("a"), cwd.clone()]
        );
        assert_eq!(find_hermes_md(&cwd).unwrap(), None);
        std::fs::write(repo.join(".hermes.md"), "root").unwrap();
        std::fs::write(cwd.join("HERMES.md"), "nearest").unwrap();
        assert_eq!(find_hermes_md(&cwd).unwrap(), Some(cwd.join("HERMES.md")));
        std::fs::write(cwd.join(".hermes.md"), "preferred").unwrap();
        assert_eq!(find_hermes_md(&cwd).unwrap(), Some(cwd.join(".hermes.md")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn frontmatter_matches_python() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/context-frontmatter-goldens.json"
        ))
        .unwrap();
        for case in cases.as_array().unwrap() {
            assert_eq!(
                strip_frontmatter(case["input"].as_str().unwrap()),
                case["expected"].as_str().unwrap()
            );
        }
    }
}
