//! Skill-bundle scan results and text classification from tools/skills_guard.py.
//! Project admission uses the scan verdict, not the installation policy.
#![allow(dead_code)]

use std::collections::BTreeSet;

/// One matcher is shared by structural and text scans. These are the reference
/// scanner's fnmatch-style rules, not a full gitignore implementation.
pub struct IgnoreRules(Vec<String>);

impl IgnoreRules {
    pub fn load(directory: &std::path::Path) -> Self {
        let mut patterns = Vec::new();
        for name in [".skillignore", ".clawhubignore"] {
            let file = directory.join(name);
            if !file.is_file() {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(file) {
                for line in crate::python_value::split_lines(&text) {
                    let line = line.trim_matches(crate::python_value::python_whitespace);
                    if !line.is_empty() && !line.starts_with('#') {
                        patterns.push(line.to_owned());
                    }
                }
            }
        }
        Self(patterns)
    }

    pub fn ignores(&self, relative: &str) -> bool {
        // PurePosixPath removes repeated separators and dot components, while
        // retaining parent components. Scanner callers supply relative paths.
        let path = relative
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect::<Vec<_>>()
            .join("/");
        let base = path.rsplit('/').next().unwrap_or("");
        if base == "SKILL.md" {
            return false;
        }
        if matches!(base, ".skillignore" | ".clawhubignore") {
            return true;
        }
        for pattern in &self.0 {
            let anchored = pattern.starts_with('/');
            let pattern = pattern.trim_start_matches('/');
            let directory = pattern.ends_with('/');
            let pattern = pattern.trim_end_matches('/');
            if pattern.is_empty() {
                continue;
            }
            if directory {
                if path == pattern
                    || path.starts_with(&format!("{pattern}/"))
                    || (!anchored && format!("/{path}/").contains(&format!("/{pattern}/")))
                {
                    return true;
                }
                continue;
            }
            if glob_matches(&path, pattern) {
                return true;
            }
            if !anchored
                && (glob_matches(base, pattern)
                    || (!pattern.contains('/')
                        && path.split('/').any(|part| glob_matches(part, pattern)))
                    || path.starts_with(&format!("{pattern}/")))
            {
                return true;
            }
        }
        false
    }
}

/// POSIX fnmatch treats slash and leading dots as ordinary characters. Dynamic
/// programming avoids exponential backtracking on repeated stars and literals.
fn glob_matches(text: &str, pattern: &str) -> bool {
    enum Token {
        Star,
        Any,
        Literal(char),
        Class(bool, Vec<(char, char)>),
    }
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let character = chars[i];
        i += 1;
        tokens.push(match character {
            '*' => Token::Star,
            '?' => Token::Any,
            '[' => {
                let start = i;
                let negated = chars.get(i) == Some(&'!');
                let mut end = i + usize::from(negated);
                if chars.get(end) == Some(&']') {
                    end += 1;
                }
                while end < chars.len() && chars[end] != ']' {
                    end += 1;
                }
                if end == chars.len() {
                    Token::Literal('[')
                } else {
                    let mut ranges = Vec::new();
                    let mut at = start + usize::from(negated);
                    while at < end {
                        if at + 2 < end && chars[at + 1] == '-' {
                            if chars[at] <= chars[at + 2] {
                                ranges.push((chars[at], chars[at + 2]));
                            }
                            at += 3;
                        } else {
                            ranges.push((chars[at], chars[at]));
                            at += 1;
                        }
                    }
                    i = end + 1;
                    Token::Class(negated, ranges)
                }
            }
            character => Token::Literal(character),
        });
    }
    let text: Vec<_> = text.chars().collect();
    let mut previous = vec![false; text.len() + 1];
    previous[0] = true;
    for token in tokens {
        let mut current = vec![false; text.len() + 1];
        if matches!(token, Token::Star) {
            current[0] = previous[0];
        }
        for (i, character) in text.iter().enumerate() {
            current[i + 1] = match &token {
                Token::Star => previous[i + 1] || current[i],
                Token::Any => previous[i],
                Token::Literal(expected) => previous[i] && character == expected,
                Token::Class(negated, ranges) => {
                    previous[i]
                        && (ranges
                            .iter()
                            .any(|(start, end)| start <= character && character <= end)
                            != *negated)
                }
            };
        }
        previous = current;
    }
    previous[text.len()]
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Finding {
    pub pattern_id: String,
    pub severity: String,
    pub category: String,
    pub file: String,
    pub line: usize,
    #[serde(rename = "match")]
    pub matched: String,
    pub description: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ScanResult {
    pub skill_name: String,
    pub source: String,
    pub trust_level: String,
    pub verdict: String,
    pub findings: Vec<Finding>,
    pub scanned_at: String,
    pub summary: String,
    #[serde(default)]
    pub scan_provenance: serde_json::Map<String, serde_json::Value>,
}

/// Run structural checks before text classification, sharing the ignore rules.
/// Like the reference, a single file bypasses bundle structure checks and a
/// missing path produces an empty scan. Admission must validate the path first.
pub fn scan_skill(path: &std::path::Path, source: &str) -> anyhow::Result<ScanResult> {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-Unicode skill path"))?;
    let mut findings = Vec::new();
    if path.is_dir() {
        let ignore = IgnoreRules::load(path);
        findings.extend(check_structure(path, Some(&ignore))?);
        for file in bundle_paths(path) {
            if !file.is_file() {
                continue;
            }
            let relative = file
                .strip_prefix(path)?
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-Unicode skill path"))?;
            if !ignore.ignores(relative) {
                findings.extend(scan_file(&file, relative)?);
            }
        }
    } else if path.is_file() {
        findings.extend(scan_file(path, name)?);
    }
    let now = chrono::Utc::now();
    let precision = if now.timestamp_subsec_micros() == 0 {
        chrono::SecondsFormat::Secs
    } else {
        chrono::SecondsFormat::Micros
    };
    Ok(ScanResult {
        skill_name: name.into(),
        source: source.into(),
        trust_level: trust_level(source).into(),
        verdict: verdict(&findings).into(),
        summary: summary(name, &findings),
        findings,
        scanned_at: now.to_rfc3339_opts(precision, false),
        scan_provenance: serde_json::Map::new(),
    })
}

/// Hash every file, including ignored artifacts, in POSIX relative-path order.
/// The NUL separator binds each path to its exact bytes, as in skills_hub bundles.
fn content_digest(path: &std::path::Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    if path.is_dir() {
        let mut entries = Vec::new();
        for file in bundle_paths(path) {
            if file.is_file() {
                let relative = file
                    .strip_prefix(path)?
                    .components()
                    .map(|part| {
                        part.as_os_str()
                            .to_str()
                            .ok_or_else(|| anyhow::anyhow!("non-Unicode skill path"))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?
                    .join("/");
                entries.push((relative, file));
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (relative, file) in entries {
            hash.update(relative.as_bytes());
            hash.update(b"\0");
            hash.update(std::fs::read(file)?);
        }
    } else {
        hash.update(std::fs::read(path)?);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn full_content_hash(path: &std::path::Path) -> anyhow::Result<String> {
    Ok(format!("sha256:{}", content_digest(path)?))
}

pub fn content_hash(path: &std::path::Path) -> anyhow::Result<String> {
    Ok(format!("sha256:{}", &content_digest(path)?[..16]))
}

/// Reuse scans only for the same content, scanner version and source identity.
/// Cache write failures are best effort; hashing and malformed accepted records
/// propagate errors so project admission can fail closed.
pub fn scan_skill_cached(
    path: &std::path::Path,
    source: &str,
    source_url: &str,
    cache_dir: Option<&std::path::Path>,
) -> anyhow::Result<ScanResult> {
    use sha2::{Digest, Sha256};
    const VERSION: &str = "skills-guard-v2";
    let bundle_hash = full_content_hash(path)?;
    let cache_root = cache_dir.map(std::path::Path::to_owned).unwrap_or_else(|| {
        path.parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(".scan-cache")
    });
    let identity = format!("{:x}", Sha256::digest(format!("{source}\0{source_url}")));
    let cache_file = cache_root.join(format!("{}-{}.json", &bundle_hash[7..], &identity[..16]));
    let cached = match std::fs::read(&cache_file) {
        // Python suppresses I/O and JSON syntax errors, but not UTF-8 errors.
        Ok(bytes) => serde_json::from_str::<serde_json::Value>(std::str::from_utf8(&bytes)?).ok(),
        Err(_) => None,
    };
    if let Some(serde_json::Value::Object(mut cached)) = cached {
        if cached.get("bundle_hash") == Some(&serde_json::json!(bundle_hash))
            && cached.get("scanner_version") == Some(&serde_json::json!(VERSION))
            && cached.get("source") == Some(&serde_json::json!(source))
            && cached.get("source_url") == Some(&serde_json::json!(source_url))
        {
            let required = |key: &str| -> anyhow::Result<String> {
                cached
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("invalid cached scan field: {key}"))
            };
            let mut result = ScanResult {
                skill_name: path
                    .file_name()
                    .unwrap_or_default()
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("non-Unicode skill path"))?
                    .into(),
                source: source.into(),
                trust_level: required("trust_level")?,
                verdict: required("verdict")?,
                scanned_at: required("scanned_at")?,
                summary: if cached.contains_key("summary") {
                    required("summary")?
                } else {
                    String::new()
                },
                findings: serde_json::from_value(
                    cached
                        .get("findings")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                )?,
                scan_provenance: serde_json::Map::new(),
            };
            cached.insert("fresh".into(), false.into());
            result.scan_provenance = cached;
            return Ok(result);
        }
    }
    let mut result = scan_skill(path, source)?;
    let rules: BTreeSet<_> = result
        .findings
        .iter()
        .map(|finding| finding.pattern_id.as_str())
        .collect();
    let provenance = serde_json::json!({
        "source": source, "source_url": source_url, "bundle_hash": bundle_hash,
        "scanner_version": VERSION, "verdict": result.verdict,
        "trust_level": result.trust_level, "findings": result.findings,
        "rules": rules, "scanned_at": result.scanned_at,
        "summary": result.summary, "fresh": true,
    });
    let mut bytes = serde_json::to_vec_pretty(&provenance)?;
    bytes.push(b'\n');
    if std::fs::create_dir_all(&cache_root).is_ok() {
        let _ = std::fs::write(cache_file, bytes);
    }
    result.scan_provenance = provenance.as_object().unwrap().clone();
    Ok(result)
}

/// pathlib's recursive wildcard emits each directory's entries before walking
/// its children and does not descend through directory symlinks.
fn bundle_paths(directory: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn visit(directory: &std::path::Path, output: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        let Ok(entries) = entries.collect::<std::io::Result<Vec<_>>>() else {
            return;
        };
        let mut children = Vec::new();
        for entry in entries {
            let path = entry.path();
            if path.is_dir() && !path.is_symlink() {
                children.push(path.clone());
            }
            output.push(path);
        }
        for child in children {
            visit(&child, output);
        }
    }
    let mut output = Vec::new();
    visit(directory, &mut output);
    output
}

pub fn check_structure(
    directory: &std::path::Path,
    ignore: Option<&IgnoreRules>,
) -> anyhow::Result<Vec<Finding>> {
    let mut findings = Vec::new();
    let mut count = 0usize;
    let mut total = 0u64;
    let mut add = |id: &str,
                   severity: &str,
                   category: &str,
                   file: &str,
                   matched: String,
                   description: String| {
        findings.push(Finding {
            pattern_id: id.into(),
            severity: severity.into(),
            category: category.into(),
            file: file.into(),
            line: 0,
            matched,
            description,
        });
    };
    for file in bundle_paths(directory) {
        if !file.is_file() && !file.is_symlink() {
            continue;
        }
        let relative = file
            .strip_prefix(directory)?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-Unicode skill path"))?;
        if ignore.is_some_and(|ignore| ignore.ignores(relative)) {
            continue;
        }
        count += 1;
        if file.is_symlink() {
            let resolve = |path: &std::path::Path| -> std::io::Result<std::path::PathBuf> {
                let absolute = if path.is_absolute() {
                    path.to_owned()
                } else {
                    std::env::current_dir()?.join(path)
                };
                crate::file_read_safety::realpath_abs(
                    absolute
                        .to_str()
                        .ok_or_else(|| std::io::Error::other("non-Unicode skill path"))?
                        .to_owned(),
                )
            };
            match resolve(&file).and_then(|target| resolve(directory).map(|root| (target, root))) {
                Ok((target, root)) if !target.starts_with(&root) => add(
                    "symlink_escape",
                    "critical",
                    "traversal",
                    relative,
                    format!("symlink -> {}", target.display()),
                    "symlink points outside the skill directory".into(),
                ),
                Ok(_) => {}
                // Python 3.12 raises RuntimeError for a resolution loop. The
                // reference's OSError handler does not swallow that exception.
                Err(error) if error.to_string() == "symlink loop" => return Err(error.into()),
                Err(_) => add(
                    "broken_symlink",
                    "medium",
                    "traversal",
                    relative,
                    "broken symlink".into(),
                    "broken or circular symlink".into(),
                ),
            }
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&file) else {
            continue;
        };
        let size = metadata.len();
        total += size;
        if size > 256 * 1024 {
            add(
                "oversized_file",
                "medium",
                "structural",
                relative,
                format!("{}KB", size / 1024),
                format!("file is {}KB (limit: 256KB)", size / 1024),
            );
        }
        let extension = file
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| format!(".{}", s.to_lowercase()))
            .unwrap_or_default();
        if [
            ".exe", ".dll", ".so", ".dylib", ".bin", ".dat", ".com", ".msi", ".dmg", ".app",
            ".deb", ".rpm",
        ]
        .contains(&extension.as_str())
        {
            add(
                "binary_file",
                "critical",
                "structural",
                relative,
                format!("binary: {extension}"),
                format!("binary/executable file ({extension}) should not be in a skill"),
            );
        }
        if ![".sh", ".bash", ".py", ".rb", ".pl"].contains(&extension.as_str()) {
            // Keep the second stat's error propagation, unlike the size read.
            let metadata = std::fs::metadata(&file)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 != 0 {
                    add(
                        "unexpected_executable",
                        "medium",
                        "structural",
                        relative,
                        "executable bit set".into(),
                        "file has executable permission but is not a recognized script type".into(),
                    );
                }
            }
            #[cfg(not(unix))]
            let _ = metadata;
        }
    }
    if count > 50 {
        add(
            "too_many_files",
            "medium",
            "structural",
            "(directory)",
            format!("{count} files"),
            format!("skill has {count} files (limit: 50)"),
        );
    }
    if total > 5120 * 1024 {
        add(
            "oversized_skill",
            "low",
            "structural",
            "(directory)",
            format!("{}KB total", total / 1024),
            format!("skill is {}KB total (limit: 5120KB)", total / 1024),
        );
    }
    Ok(findings)
}

#[derive(serde::Deserialize)]
struct Pattern {
    pattern: String,
    pattern_id: String,
    severity: String,
    category: String,
    description: String,
    python_flags_value: u32,
}

#[derive(serde::Deserialize)]
struct Invisible {
    #[serde(rename = "char")]
    character: char,
    hex: String,
    name: String,
}

#[derive(serde::Deserialize)]
struct PatternData {
    scannable_extensions: Vec<String>,
    invisible_chars: Vec<Invisible>,
    threat_patterns: Vec<Pattern>,
}

/// Read and scan a file with the reference scanner's extension and decoding
/// behavior. Regex engine errors propagate so a project caller can quarantine
/// failed scans rather than misreport them as clean.
pub fn scan_file(path: &std::path::Path, relative: &str) -> anyhow::Result<Vec<Finding>> {
    static DATA: std::sync::LazyLock<PatternData> = std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../../../tools/skill-guard-patterns.json"))
            .expect("generated skill guard patterns")
    });
    static REGEXES: std::sync::LazyLock<Result<Vec<fancy_regex::Regex>, String>> =
        std::sync::LazyLock::new(|| {
            DATA.threat_patterns
                .iter()
                .map(|pattern| {
                    if pattern.python_flags_value != 34 {
                        return Err(format!("unsupported flags for {}", pattern.pattern_id));
                    }
                    fancy_regex::Regex::new(&crate::threat_patterns::python_regex_to_fancy(
                        &pattern.pattern,
                    ))
                    .map_err(|error| format!("{}: {error}", pattern.pattern_id))
                })
                .collect()
        });
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| format!(".{}", extension.to_lowercase()))
        .unwrap_or_default();
    if name != "SKILL.md" && !DATA.scannable_extensions.contains(&extension) {
        return Ok(Vec::new());
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    let content = content.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<_> = content.split('\n').collect();
    let docstrings = docstring_lines(&lines);
    let relative = if relative.is_empty() { name } else { relative };
    let regexes = REGEXES
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut findings = Vec::new();
    let mut seen = BTreeSet::new();
    for (pattern, regex) in DATA.threat_patterns.iter().zip(regexes) {
        for (offset, line) in lines.iter().enumerate() {
            let number = offset + 1;
            if docstrings.contains(&number) || seen.contains(&(pattern.pattern_id.as_str(), number))
            {
                continue;
            }
            if regex.is_match(line)? {
                seen.insert((pattern.pattern_id.as_str(), number));
                let matched = line.trim_matches(crate::python_value::python_whitespace);
                let matched = if matched.chars().count() > 120 {
                    format!("{}...", matched.chars().take(117).collect::<String>())
                } else {
                    matched.to_owned()
                };
                findings.push(Finding {
                    pattern_id: pattern.pattern_id.clone(),
                    severity: pattern.severity.clone(),
                    category: pattern.category.clone(),
                    file: relative.to_owned(),
                    line: number,
                    matched,
                    description: pattern.description.clone(),
                });
            }
        }
    }
    for (offset, line) in lines.iter().enumerate() {
        // Python chooses the first member of a hash-randomized set. Use stable
        // generated order when several invisible characters occupy one line;
        // there is still exactly one high-severity finding for that line.
        if let Some(character) = DATA
            .invisible_chars
            .iter()
            .find(|character| line.contains(character.character))
        {
            findings.push(Finding {
                pattern_id: "invisible_unicode".into(),
                severity: "high".into(),
                category: "injection".into(),
                file: relative.to_owned(),
                line: offset + 1,
                matched: format!("{} ({})", character.hex, character.name),
                description: format!(
                    "invisible unicode character {} (possible text hiding/injection)",
                    character.name
                ),
            });
        }
    }
    Ok(findings)
}

pub fn verdict(findings: &[Finding]) -> &'static str {
    if findings
        .iter()
        .any(|finding| finding.severity == "critical")
    {
        "dangerous"
    } else if findings.iter().any(|finding| finding.severity == "high") {
        "caution"
    } else {
        "safe"
    }
}

pub fn summary(name: &str, findings: &[Finding]) -> String {
    if findings.is_empty() {
        return format!("{name}: clean scan, no threats detected");
    }
    let categories: BTreeSet<_> = findings
        .iter()
        .map(|finding| finding.category.as_str())
        .collect();
    format!(
        "{name}: {} \u{2014} {} finding(s) in {}",
        verdict(findings),
        findings.len(),
        categories.into_iter().collect::<Vec<_>>().join(", ")
    )
}

pub fn trust_level(source: &str) -> &'static str {
    let source = ["skills-sh/", "skills.sh/", "skils-sh/", "skils.sh/"]
        .iter()
        .find_map(|prefix| source.strip_prefix(prefix))
        .unwrap_or(source);
    match source {
        "agent-created" => "agent-created",
        "official" => "builtin",
        _ if [
            "openai/skills",
            "anthropics/skills",
            "huggingface/skills",
            "NVIDIA/skills",
        ]
        .iter()
        .any(|repo| {
            source == *repo
                || source
                    .strip_prefix(repo)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }) =>
        {
            "trusted"
        }
        _ => "community",
    }
}

/// Preserve the reference's heuristic: both triple-quote styles toggle a single
/// state, and opening, closing and self-contained lines all suppress patterns.
/// This intentionally is not a Python syntax parser; skills include prose too.
fn docstring_lines(lines: &[&str]) -> BTreeSet<usize> {
    let mut inside = false;
    let mut result = BTreeSet::new();
    for (offset, line) in lines.iter().enumerate() {
        let was_inside = inside;
        let mut has_marker = false;
        for marker in ["\"\"\"", "'''"] {
            let count = line.matches(marker).count();
            has_marker |= count > 0;
            if count % 2 == 1 {
                inside = !inside;
            }
        }
        if was_inside || inside || has_marker {
            result.insert(offset + 1);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_scans_bind_content_source_and_scanner_version() {
        let root = std::env::temp_dir().join(format!("hermes-scan-cache-{}", std::process::id()));
        let skill = root.join("skill");
        let cache = root.join("cache");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "# Safe skill\n").unwrap();
        let scan = || scan_skill_cached(&skill, "project-local", "", Some(&cache)).unwrap();
        let fresh = scan();
        assert_eq!(fresh.scan_provenance["fresh"], true);
        let cached = scan();
        assert_eq!(cached.scan_provenance["fresh"], false);
        assert_eq!(fresh.scanned_at, cached.scanned_at);
        assert_eq!(fresh.findings, cached.findings);
        assert_eq!(
            scan_skill_cached(&skill, "community", "", Some(&cache))
                .unwrap()
                .scan_provenance["fresh"],
            true
        );
        assert_eq!(
            scan_skill_cached(&skill, "project-local", "url", Some(&cache))
                .unwrap()
                .scan_provenance["fresh"],
            true
        );
        // Even ignored files invalidate the attestation, since hashing includes them.
        std::fs::write(skill.join(".skillignore"), "notes\n").unwrap();
        std::fs::write(skill.join("notes"), "changed").unwrap();
        let changed = scan();
        assert_eq!(changed.scan_provenance["fresh"], true);
        assert_ne!(
            fresh.scan_provenance["bundle_hash"],
            changed.scan_provenance["bundle_hash"]
        );
        assert!(changed.findings.is_empty());
        let file = std::fs::read_dir(&cache)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                let value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                value["bundle_hash"] == changed.scan_provenance["bundle_hash"]
            })
            .unwrap();
        let mut record = changed.scan_provenance.clone();
        record.insert("scanner_version".into(), "old".into());
        std::fs::write(&file, serde_json::to_vec(&record).unwrap()).unwrap();
        assert_eq!(scan().scan_provenance["fresh"], true);
        std::fs::write(&file, b"{invalid").unwrap();
        assert_eq!(scan().scan_provenance["fresh"], true);
        std::fs::write(&file, b"\xff").unwrap();
        assert!(scan_skill_cached(&skill, "project-local", "", Some(&cache)).is_err());
        // An unwritable cache does not discard a successful scan.
        let blocked = root.join("file-cache");
        std::fs::write(&blocked, b"file").unwrap();
        assert_eq!(
            scan_skill_cached(&skill, "project-local", "", Some(&blocked))
                .unwrap()
                .verdict,
            "safe"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn content_hashes_match_python_and_detect_mutations() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/skill-hash-goldens.json")).unwrap();
        let root = std::env::temp_dir().join(format!("hermes-skill-hash-{}", std::process::id()));
        for case in cases.as_array().unwrap() {
            let directory = root.join(case["name"].as_str().unwrap());
            std::fs::create_dir_all(&directory).unwrap();
            for (relative, bytes) in case["files"].as_object().unwrap() {
                let path = directory.join(relative);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                let bytes = bytes.as_str().unwrap();
                let bytes: Vec<u8> = (0..bytes.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&bytes[i..i + 2], 16).unwrap())
                    .collect();
                std::fs::write(path, bytes).unwrap();
            }
            for (relative, target) in case["links"].as_object().unwrap() {
                std::os::unix::fs::symlink(target.as_str().unwrap(), directory.join(relative))
                    .unwrap();
            }
            let path = if case["single"].as_bool().unwrap() {
                directory.join("data")
            } else {
                directory.clone()
            };
            assert_eq!(
                full_content_hash(&path).unwrap(),
                case["full"],
                "{}",
                case["name"]
            );
            assert_eq!(
                content_hash(&path).unwrap(),
                case["short"],
                "{}",
                case["name"]
            );
            if !case["single"].as_bool().unwrap() {
                std::fs::write(directory.join("added"), b"changed").unwrap();
                assert_ne!(full_content_hash(&path).unwrap(), case["full"]);
            }
        }
        assert!(full_content_hash(&root.join("missing")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bundle_scan_matches_actual_python() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/skill-bundle-goldens.json")).unwrap();
        let root = std::env::temp_dir().join(format!("hermes-bundle-scan-{}", std::process::id()));
        for case in cases.as_array().unwrap() {
            let directory = root.join(case["name"].as_str().unwrap());
            std::fs::create_dir_all(&directory).unwrap();
            for (name, content) in case["files"].as_object().unwrap() {
                std::fs::write(directory.join(name), content.as_str().unwrap()).unwrap();
            }
            let path = if case["single"].as_bool().unwrap() {
                directory.join("SKILL.md")
            } else {
                directory
            };
            let result = scan_skill(&path, case["source"].as_str().unwrap()).unwrap();
            chrono::DateTime::parse_from_rfc3339(&result.scanned_at).unwrap();
            let mut actual = serde_json::to_value(result).unwrap();
            actual.as_object_mut().unwrap().remove("scanned_at");
            assert_eq!(actual, case["expected"], "{}", case["name"]);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn structure_findings_match_actual_python() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let data: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/skill-structure-goldens.json"))
                .unwrap();
        let root =
            std::env::temp_dir().join(format!("hermes-skill-structure-{}", std::process::id()));
        for case in data["cases"].as_array().unwrap() {
            let root = root.join(case["name"].as_str().unwrap());
            let skill = root.join("skill");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::create_dir_all(root.join("outside")).unwrap();
            std::fs::write(root.join("outside/secret.txt"), "secret outside data\n").unwrap();
            std::fs::write(root.join("outside/target.txt"), "target outside data\n").unwrap();
            std::fs::create_dir_all(root.join("skill-backdoor")).unwrap();
            std::fs::write(root.join("skill-backdoor/malicious.py"), "evil()\n").unwrap();
            for (field, name) in [
                ("ignore_file_contents", ".skillignore"),
                ("clawhubignore_contents", ".clawhubignore"),
            ] {
                if let Some(content) = case[field].as_str() {
                    std::fs::write(skill.join(name), content).unwrap();
                }
            }
            for file in case["files"].as_array().unwrap() {
                let path = skill.join(file["path"].as_str().unwrap());
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                if let Some(content) = file["content"].as_str() {
                    std::fs::write(&path, content).unwrap();
                } else {
                    std::fs::File::create(&path)
                        .unwrap()
                        .set_len(file["size"].as_u64().unwrap())
                        .unwrap();
                }
                let mode = if file["executable"].as_bool().unwrap() {
                    0o755
                } else {
                    0o644
                };
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
            }
            for link in case["symlinks"].as_array().unwrap() {
                let path = skill.join(link["path"].as_str().unwrap());
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                symlink(link["target"].as_str().unwrap(), path).unwrap();
            }
            let mut findings = check_structure(&skill, Some(&IgnoreRules::load(&skill))).unwrap();
            for finding in &mut findings {
                finding.matched = finding
                    .matched
                    .replace(root.to_str().unwrap(), "<TEMP_DIR>");
            }
            // Only the oracle comparison normalizes filesystem enumeration order.
            let order = [
                "symlink_escape",
                "broken_symlink",
                "oversized_file",
                "binary_file",
                "unexpected_executable",
                "too_many_files",
                "oversized_skill",
            ];
            findings.sort_by_key(|f| {
                (
                    f.file == "(directory)",
                    f.file.clone(),
                    order.iter().position(|id| *id == f.pattern_id).unwrap(),
                )
            });
            assert_eq!(
                serde_json::to_value(findings).unwrap(),
                case["expected_findings"],
                "{}",
                case["name"]
            );
            std::fs::remove_dir_all(root).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn structure_propagates_symlink_loops_like_python_312() {
        let root = std::env::temp_dir().join(format!("hermes-skill-loop-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink("loop", root.join("loop")).unwrap();
        assert!(check_structure(&root, None)
            .unwrap_err()
            .to_string()
            .contains("symlink loop"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_findings_match_actual_python() {
        use base64::Engine;
        let data: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/skill-guard-file-goldens.json"))
                .unwrap();
        let root =
            std::env::temp_dir().join(format!("hermes-skill-file-oracle-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for case in data["cases"].as_array().unwrap() {
            let relative = case["rel_path"].as_str().unwrap();
            let path = root.join(std::path::Path::new(relative).file_name().unwrap());
            let bytes = if let Some(content) = case["content"].as_str() {
                content.as_bytes().to_vec()
            } else {
                base64::engine::general_purpose::STANDARD
                    .decode(case["content_base64"].as_str().unwrap())
                    .unwrap()
            };
            std::fs::write(&path, bytes).unwrap();
            let actual = scan_file(&path, relative)
                .unwrap_or_else(|error| panic!("{}: {error}", case["name"]));
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                case["expected"],
                "{}",
                case["name"]
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wildcard_and_ignore_files_match_actual_python() {
        let data: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/skill-ignore-goldens.json")).unwrap();
        for case in data["globs"].as_array().unwrap() {
            assert_eq!(
                glob_matches(
                    case["text"].as_str().unwrap(),
                    case["pattern"].as_str().unwrap()
                ),
                case["expected"].as_bool().unwrap(),
                "{case}"
            );
        }
        let root = std::env::temp_dir().join(format!("hermes-skill-ignore-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for case in data["ignores"].as_array().unwrap() {
            std::fs::write(root.join(".skillignore"), case["rules"].as_str().unwrap()).unwrap();
            std::fs::write(
                root.join(".clawhubignore"),
                case["compat_rules"].as_str().unwrap(),
            )
            .unwrap();
            let rules = IgnoreRules::load(&root);
            for (path, expected) in case["paths"]
                .as_array()
                .unwrap()
                .iter()
                .zip(case["expected"].as_array().unwrap())
            {
                assert_eq!(
                    rules.ignores(path.as_str().unwrap()),
                    expected.as_bool().unwrap(),
                    "rules {}, path {path}",
                    case["rules"]
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn docstrings_suppress_open_close_and_single_line_markers() {
        let text = "ordinary\n\"\"\"open\ninside\nclose\"\"\"\ncode\nx = '''one line'''\n\"\"\" mixed '''\ncode";
        assert_eq!(
            docstring_lines(&text.split('\n').collect::<Vec<_>>()),
            BTreeSet::from([2, 3, 4, 6, 7])
        );
    }

    #[test]
    fn trust_requires_repository_boundary_and_preserves_case() {
        for (source, expected) in [
            ("official", "builtin"),
            ("official/fake", "community"),
            ("skills.sh/openai/skills/sub", "trusted"),
            ("openai/skills-evil", "community"),
            ("nvidia/skills", "community"),
            ("NVIDIA/skills", "trusted"),
            ("project-local", "community"),
            ("agent-created", "agent-created"),
        ] {
            assert_eq!(trust_level(source), expected, "{source}");
        }
    }

    #[test]
    fn verdict_uses_highest_severity_and_summary_sorts_categories() {
        let mut findings = Vec::new();
        assert_eq!(
            summary("example", &findings),
            "example: clean scan, no threats detected"
        );
        for (severity, category, expected) in [
            ("medium", "network", "safe"),
            ("high", "injection", "caution"),
            ("critical", "network", "dangerous"),
            ("low", "injection", "dangerous"),
        ] {
            findings.push(Finding {
                pattern_id: "fixture".into(),
                severity: severity.into(),
                category: category.into(),
                file: "SKILL.md".into(),
                line: 1,
                matched: "content".into(),
                description: "fixture".into(),
            });
            assert_eq!(verdict(&findings), expected);
        }
        assert_eq!(
            summary("example", &findings),
            "example: dangerous \u{2014} 4 finding(s) in injection, network"
        );
    }
}
