//! Bounded native classifier for recoverable terminal risk.
//!
//! Pattern order and descriptions are frozen from the Python approval module
//! by `dangerous-command-contract-oracle.py`. Runtime classification never
//! starts Python or executes the candidate command.

use std::path::Path;
use std::sync::LazyLock;

use fancy_regex::Regex;
use serde_json::Value;

const PARSER_LIMIT: &str = "command parser limit exceeded";
const MALFORMED_EXEC: &str = "command parser limit or malformed executable payload";
const MAX_COMMAND_CHARS: usize = 128_000;
const MAX_SEPARATOR_FREE_CHARS: usize = 4_096;
const MAX_SEGMENTS: usize = 25_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub pattern_key: String,
    pub description: String,
}

struct Pattern {
    regex: Regex,
    description: String,
    legacy_key: String,
}

static PATTERNS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    let corpus: Value = serde_json::from_str(include_str!(
        "../../../tools/dangerous-command-contract-goldens.json"
    ))
    .expect("dangerous-command contract is valid JSON");
    corpus["dangerous_patterns_coverage"]
        .as_array()
        .expect("dangerous pattern table is an array")
        .iter()
        .map(|row| {
            let source = row["target_pattern_regex"]
                .as_str()
                .expect("pattern source is a string");
            Pattern {
                regex: Regex::new(&format!("(?is:{source})")).unwrap_or_else(|error| {
                    panic!("invalid dangerous pattern {source:?}: {error}")
                }),
                description: row["target_pattern_description"]
                    .as_str()
                    .expect("pattern description is a string")
                    .to_owned(),
                legacy_key: legacy_pattern_key(source),
            }
        })
        .collect()
});

pub fn detect(command: &str, profile_home: &Path, user_home: Option<&Path>) -> Option<Finding> {
    if parser_limit_exceeded(command) {
        return finding(PARSER_LIMIT);
    }
    if verification_artifact_cleanup(command) {
        return None;
    }

    let mut variants =
        crate::terminal_guard::dangerous_detection_variants(command, profile_home, user_home);
    variants.extend(simple_echo_substitution_variants(command));
    for variant in variants {
        let variant = mask_quoted_pcre_grep_pattern(&variant);
        for pattern in PATTERNS.iter() {
            match pattern.regex.is_match(&variant.to_lowercase()) {
                Ok(true) => return finding(&pattern.description),
                Ok(false) => {}
                Err(_) => return finding(PARSER_LIMIT),
            }
        }
    }

    execution_finding(command).or_else(|| spliced_gateway_lifecycle(command))
}

/// Match a canonical finding description against current and historical
/// approval keys. Python used regex-derived keys before switching to stable
/// descriptions, so existing `command_allowlist` files must keep working.
pub fn approval_key_matches(canonical: &str, candidate: &str) -> bool {
    if canonical == candidate {
        return true;
    }
    if matches!(
        (canonical, candidate),
        (
            "script execution via -e/-c flag",
            "(python[23]?|perl|ruby|node)\\s+-[ec]\\s+"
        ) | (
            "script execution via heredoc",
            "(python[23]?|perl|ruby|node)\\s+<<"
        )
    ) {
        return true;
    }
    PATTERNS
        .iter()
        .any(|pattern| pattern.description == canonical && pattern.legacy_key == candidate)
}

fn legacy_pattern_key(source: &str) -> String {
    source
        .split_once(r"\b")
        .map(|(_, suffix)| suffix.split(r"\b").next().unwrap_or(suffix).to_owned())
        .unwrap_or_else(|| source.chars().take(20).collect())
}

fn simple_echo_substitution_variants(command: &str) -> Vec<String> {
    let mut variants = Vec::new();
    for (open, close) in [("$(", ")"), ("`", "`")] {
        let mut from = 0;
        while let Some(relative_start) = command[from..].find(open) {
            let start = from + relative_start;
            let inner_start = start + open.len();
            let Some(relative_end) = command[inner_start..].find(close) else {
                break;
            };
            let end = inner_start + relative_end;
            if let Ok(words) = shell_words::split(&command[inner_start..end]) {
                if words.len() == 2 && words[0] == "echo" {
                    let mut variant = command.to_owned();
                    variant.replace_range(start..end + close.len(), &words[1]);
                    variants.push(variant);
                }
            }
            from = end + close.len();
        }
    }
    variants
}

fn finding(description: &str) -> Option<Finding> {
    Some(Finding {
        pattern_key: description.to_owned(),
        description: description.to_owned(),
    })
}

fn parser_limit_exceeded(command: &str) -> bool {
    if command.chars().count() > MAX_COMMAND_CHARS {
        return true;
    }
    if command.chars().count() > MAX_SEPARATOR_FREE_CHARS
        && !command
            .chars()
            .any(|ch| matches!(ch, ';' | '&' | '|' | '\n'))
    {
        return true;
    }
    command
        .chars()
        .filter(|ch| matches!(ch, ';' | '&' | '|' | '\n'))
        .take(MAX_SEGMENTS)
        .count()
        >= MAX_SEGMENTS
}

fn verification_artifact_cleanup(command: &str) -> bool {
    let Ok(words) = shell_words::split(command) else {
        return false;
    };
    if words.len() != 3 || words[0] != "rm" || words[1] != "-f" {
        return false;
    }
    let operand = Path::new(&words[2]);
    let Some(name) = operand.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if operand != std::env::temp_dir().join(name) {
        return false;
    }
    let Some(suffix) = name
        .strip_prefix("hermes-verify-")
        .or_else(|| name.strip_prefix("hermes-ad-hoc-"))
    else {
        return false;
    };
    !suffix.is_empty()
        && suffix
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
}

/// Python masks only a structurally quoted pattern operand for grep PCRE.
/// This narrow implementation preserves the security-sensitive distinction:
/// single-quoted PCRE prose is data, while standard grep, malformed quotes,
/// and double-quoted substitutions remain inspectable.
fn mask_quoted_pcre_grep_pattern(command: &str) -> String {
    let trimmed = command.trim_start();
    let Some(after_grep) = trimmed.strip_prefix("grep ") else {
        return command.to_owned();
    };
    let pcre = after_grep
        .split_whitespace()
        .any(|word| word == "-P" || word == "--perl-regexp");
    if !pcre {
        return command.to_owned();
    }
    let bytes = command.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\'' {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < bytes.len() && bytes[index] != b'\'' {
            index += 1;
        }
        if index == bytes.len() {
            return command.to_owned();
        }
        let end = index + 1;
        let before = &command[..start];
        let is_pattern = before
            .split_whitespace()
            .last()
            .is_some_and(|word| word == "-e" || word == "--regexp")
            || !before.contains(" -e ") && !before.contains(" --regexp ");
        if is_pattern {
            let mut masked = command.to_owned();
            masked.replace_range(start..end, &" ".repeat(end - start));
            return masked;
        }
        index = end;
    }
    // --regexp='pattern' reaches the same quoted scan above. If no complete
    // single-quoted operand exists, fail closed by preserving the input.
    command.to_owned()
}

fn top_level_segments(command: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in command.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' && quote != Some('\'') {
            escaped = true;
        } else if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
        } else if matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if matches!(ch, ';' | '&' | '|' | '\n') {
            if start < index {
                segments.push(&command[start..index]);
            }
            start = index + ch.len_utf8();
        }
    }
    if start < command.len() {
        segments.push(&command[start..]);
    }
    segments
}

fn execution_finding(command: &str) -> Option<Finding> {
    for segment in top_level_segments(command) {
        let tokens = match shell_words::split(segment) {
            Ok(tokens) => tokens,
            Err(_) => {
                let lower = segment.to_ascii_lowercase();
                if [
                    "python",
                    "node",
                    "perl",
                    "ruby",
                    "php",
                    "powershell",
                    "pwsh",
                    "rg",
                ]
                .iter()
                .any(|name| {
                    lower
                        .split_whitespace()
                        .next()
                        .is_some_and(|word| word.contains(name))
                }) {
                    return finding(MALFORMED_EXEC);
                }
                continue;
            }
        };
        let Some((name, args)) = executable_and_args(&tokens) else {
            continue;
        };
        if interpreter_exec_flag(&name, args) {
            return finding("script execution via -e/-c flag");
        }
        if interpreter_family(&name).is_some() && args.iter().any(|arg| arg.starts_with("<<")) {
            return finding("script execution via heredoc");
        }
        if matches!(name.as_str(), "bash" | "sh" | "zsh" | "ksh") && bash_exec_flag(args) {
            return finding("shell command via -c/-lc flag");
        }
        if let Some(description) = read_tool_exec_flag(&name, args) {
            return finding(&description);
        }
    }
    None
}

fn executable_and_args(tokens: &[String]) -> Option<(String, &[String])> {
    let mut index = 0;
    let mut prefix_words = 0;
    let mut skip_wrapper_options = false;
    let mut skip_next_wrapper_arg = false;
    while let Some(token) = tokens.get(index).filter(|_| prefix_words < 12) {
        if skip_next_wrapper_arg {
            skip_next_wrapper_arg = false;
            index += 1;
            prefix_words += 1;
            continue;
        }
        if skip_wrapper_options && token.starts_with('-') {
            let option = token
                .split_once('=')
                .map_or(token.as_str(), |(option, _)| option)
                .to_ascii_lowercase();
            skip_next_wrapper_arg = !token.contains('=')
                && matches!(
                    option.as_str(),
                    "-c" | "--close-from"
                        | "-g"
                        | "--group"
                        | "-h"
                        | "--host"
                        | "-p"
                        | "--prompt"
                        | "-u"
                        | "--user"
                );
            index += 1;
            prefix_words += 1;
            continue;
        }
        let name = Path::new(token)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(token)
            .to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "exec" | "nohup" | "setsid" | "time" | "command" | "builtin" | "sudo" | "env"
        ) {
            skip_wrapper_options = matches!(name.as_str(), "sudo" | "env");
            index += 1;
            prefix_words += 1;
            continue;
        }
        if environment_assignment(token) {
            skip_wrapper_options = false;
            index += 1;
            prefix_words += 1;
            continue;
        }
        return Some((name, &tokens[index + 1..]));
    }
    None
}

fn environment_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn interpreter_family(name: &str) -> Option<&'static str> {
    static PYTHON: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(?:py(?:\.exe)?|python[23]?(?:\.\d+)*(?:\.exe)?)$").unwrap()
    });
    static NODE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^node(?:js)?(?:\.exe)?$").unwrap());
    static PERL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^perl[0-9]*(?:\.\d+)*(?:\.exe)?$").unwrap());
    static RUBY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^ruby[0-9.]*(?:\.exe)?$").unwrap());
    static PHP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^php(?:\.exe)?$").unwrap());
    static POWERSHELL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(?:powershell|pwsh)(?:\.exe)?$").unwrap());
    let matches = |regex: &Regex| regex.is_match(name).unwrap_or(false);
    if matches(&PYTHON) {
        Some("python")
    } else if matches(&NODE) {
        Some("node")
    } else if matches(&PERL) {
        Some("perl")
    } else if matches(&RUBY) {
        Some("ruby")
    } else if matches(&PHP) {
        Some("php")
    } else if matches(&POWERSHELL) {
        Some("powershell")
    } else {
        None
    }
}

fn interpreter_exec_flag(name: &str, args: &[String]) -> bool {
    let Some(family) = interpreter_family(name) else {
        return false;
    };
    let flags: &[&str] = match family {
        "python" => &["-c"],
        "node" => &["-e", "--eval", "-p", "--print"],
        "perl" => &["-e", "--eval"],
        "ruby" => &["-e"],
        "php" => &["-r"],
        "powershell" => &["-command", "-c", "-file", "-f"],
        _ => &[],
    };
    let with_arg: &[&str] = match family {
        "python" => &["-W", "-X", "--check-hash-based-pycs"],
        "node" => &[
            "-C",
            "--conditions",
            "--cpu-prof-dir",
            "--diagnostic-dir",
            "--icu-data-dir",
            "--import",
            "--loader",
            "--openssl-config",
            "--require",
            "--title",
        ],
        "perl" => &["-0", "-F", "-I", "-M", "-m", "-x"],
        "ruby" => &["-C", "-E", "-F", "-I", "-K", "-r"],
        "php" => &["-c", "-d", "-z"],
        "powershell" => &[
            "-configurationname",
            "-custompipename",
            "-executionpolicy",
            "-inputformat",
            "-outputformat",
            "-settingsfile",
            "-version",
            "-windowstyle",
            "-workingdirectory",
        ],
        _ => &[],
    };
    let mut skip_value = false;
    for token in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        if token == "--" || family != "powershell" && !token.starts_with('-') {
            break;
        }
        let (option, attached) = token
            .split_once('=')
            .map_or((token.as_str(), None), |(option, value)| {
                (option, Some(value))
            });
        let comparable = if family == "powershell" {
            option.to_ascii_lowercase()
        } else {
            option.to_owned()
        };
        if flags.iter().any(|flag| comparable == *flag) {
            return true;
        }
        let attached_option_value = with_arg.iter().any(|short| {
            short.starts_with('-')
                && !short.starts_with("--")
                && option.starts_with(short)
                && option.len() > short.len()
        });
        if family != "powershell"
            && !option.starts_with("--")
            && option.len() > 2
            && !attached_option_value
            && option[1..].chars().any(|ch| {
                flags
                    .iter()
                    .any(|flag| flag.len() == 2 && flag.as_bytes()[1] as char == ch)
            })
        {
            return true;
        }
        if with_arg.iter().any(|item| comparable == *item) && attached.is_none() {
            skip_value = true;
        }
    }
    false
}

fn bash_exec_flag(args: &[String]) -> bool {
    let mut index = 0;
    while let Some(token) = args.get(index) {
        if token == "--" || !token.starts_with(['-', '+']) {
            break;
        }
        if matches!(
            token.as_str(),
            "-O" | "+O" | "-o" | "+o" | "--init-file" | "--rcfile"
        ) {
            index += 2;
            continue;
        }
        if token.starts_with("--") {
            index += 1;
            continue;
        }
        let chars = &token[1..];
        if !chars
            .chars()
            .all(|ch| "ilrsDcabefhkmnptuvxBCEHPTOo".contains(ch))
        {
            index += 1;
            continue;
        }
        let consumes_arg = chars.contains('O') || chars.contains('o');
        if chars.contains('c') {
            return args.get(index + 1 + usize::from(consumes_arg)).is_some();
        }
        index += 1 + usize::from(consumes_arg);
    }
    false
}

fn read_tool_exec_flag(name: &str, args: &[String]) -> Option<String> {
    let wanted: &[&str] = match name {
        "sort" => &["--compress-program"],
        "rg" => &["--pre", "--hostname-bin"],
        "ag" => &["--pager"],
        "man" => &["--pager", "--html", "-P", "-H"],
        _ => return None,
    };
    let mut index = 0;
    while let Some(token) = args.get(index) {
        if token == "--" {
            break;
        }
        let (option, mut payload) = token
            .split_once('=')
            .map_or((token.as_str(), None), |(option, value)| {
                (option, Some(value))
            });
        let mut matched = wanted.contains(&option).then_some(option);
        if name == "man" && token.len() > 2 && (token.starts_with("-P") || token.starts_with("-H"))
        {
            matched = Some(&token[..2]);
            payload = Some(&token[2..]);
        }
        if let Some(matched) = matched {
            if payload.is_none() {
                payload = args.get(index + 1).map(String::as_str);
            }
            if payload.is_some_and(|value| !value.is_empty()) {
                return Some(format!("arbitrary program execution via {name} {matched}"));
            }
            index += if payload.is_some() && !token.contains('=') {
                2
            } else {
                1
            };
            continue;
        }
        if read_tool_long_option_has_arg(name, option) && !token.contains('=') {
            index += 2;
            continue;
        }
        if token.starts_with('-') && !token.starts_with("--") && token.len() > 1 {
            let chars = &token[1..];
            if let Some((position, _)) = chars
                .char_indices()
                .find(|(_, ch)| read_tool_short_option_has_arg(name, *ch))
            {
                let is_last =
                    position + chars[position..].chars().next().unwrap().len_utf8() == chars.len();
                index += if is_last { 2 } else { 1 };
                continue;
            }
        }
        index += 1;
    }
    None
}

fn read_tool_long_option_has_arg(name: &str, option: &str) -> bool {
    let options: &[&str] = match name {
        "rg" => &[
            "--after-context",
            "--before-context",
            "--color",
            "--colors",
            "--context",
            "--context-separator",
            "--dfa-size-limit",
            "--encoding",
            "--engine",
            "--field-context-separator",
            "--field-match-separator",
            "--file",
            "--generate",
            "--glob",
            "--hostname-bin",
            "--hyperlink-format",
            "--iglob",
            "--ignore-file",
            "--max-columns",
            "--max-count",
            "--max-depth",
            "--max-filesize",
            "--path-separator",
            "--pre",
            "--pre-glob",
            "--regex-size-limit",
            "--regexp",
            "--replace",
            "--sort",
            "--sortr",
            "--threads",
            "--type",
            "--type-add",
            "--type-clear",
            "--type-not",
        ],
        "sort" => &[
            "--batch-size",
            "--buffer-size",
            "--compress-program",
            "--field-separator",
            "--files0-from",
            "--key",
            "--output",
            "--parallel",
            "--random-source",
            "--sort",
            "--temporary-directory",
        ],
        "man" => &[
            "--config-file",
            "--encoding",
            "--extension",
            "--locale",
            "--manpath",
            "--pager",
            "--preprocessor",
            "--prompt",
            "--recode",
            "--sections",
            "--systems",
        ],
        "ag" => &[
            "--ackmate-dir-filter",
            "--color-line-number",
            "--color-match",
            "--color-path",
            "--depth",
            "--filename-pattern",
            "--file-search-regex",
            "--ignore",
            "--ignore-dir",
            "--max-count",
            "--pager",
            "--path-to-ignore",
            "--width",
            "--workers",
        ],
        _ => &[],
    };
    options.contains(&option)
}

fn read_tool_short_option_has_arg(name: &str, option: char) -> bool {
    match name {
        "rg" => "efEmjgdtTABCMr".contains(option),
        "sort" => "koStT".contains(option),
        "man" => "CRLmMSserEPp".contains(option),
        "ag" => "gGmpW".contains(option),
        _ => false,
    }
}

fn spliced_gateway_lifecycle(command: &str) -> Option<Finding> {
    let Ok(tokens) = shell_words::split(command) else {
        return None;
    };
    let lifecycle = tokens.windows(2).any(|pair| {
        pair[0].eq_ignore_ascii_case("launchctl")
            && matches!(
                pair[1].to_ascii_lowercase().as_str(),
                "stop" | "kickstart" | "bootout" | "unload" | "kill" | "disable" | "remove"
            )
    });
    let hermes = tokens
        .iter()
        .any(|token| token.to_ascii_lowercase().contains("hermes"));
    (lifecycle && hermes).then(|| Finding {
        pattern_key: "stop/restart hermes gateway via shell-spliced verb (kills running agents)"
            .into(),
        description: "stop/restart hermes gateway via shell-spliced verb (kills running agents)"
            .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_source_executed_command_corpus() {
        let corpus: Value = serde_json::from_str(include_str!(
            "../../../tools/dangerous-command-contract-goldens.json"
        ))
        .unwrap();
        let home = Path::new("/home/test/.hermes");
        let user_home = Path::new("/home/test");
        for suite in [
            "dangerous_patterns_coverage",
            "execution_bearing_flags",
            "grep_quoted_pattern_exclusions",
            "normalization_and_deobfuscation",
            "windows_paths_and_tools",
            "safe_boundaries_and_near_misses",
            "spliced_gateway_lifecycle",
            "verification_artifact_cleanup",
        ] {
            for case in corpus[suite].as_array().unwrap() {
                let command = case["command"].as_str().unwrap();
                let expected = case["is_dangerous"].as_bool().unwrap();
                let actual = detect(command, home, Some(user_home));
                assert_eq!(actual.is_some(), expected, "{}", case["id"]);
                if expected {
                    let actual = actual.unwrap();
                    let expected_key = case
                        .get("matched_pattern_key")
                        .or_else(|| case.get("pattern_key"))
                        .and_then(Value::as_str)
                        .unwrap();
                    assert_eq!(actual.pattern_key, expected_key, "{}", case["id"]);
                }
            }
        }
    }

    #[test]
    fn parser_limits_fail_closed() {
        let home = Path::new("/tmp/.hermes");
        for command in [
            "x".repeat(MAX_COMMAND_CHARS + 1),
            "x".repeat(MAX_SEPARATOR_FREE_CHARS + 1),
            ";".repeat(MAX_SEGMENTS),
        ] {
            assert_eq!(
                detect(&command, home, None).unwrap().pattern_key,
                PARSER_LIMIT
            );
        }
    }

    #[test]
    fn historical_approval_keys_alias_to_canonical_descriptions() {
        assert!(approval_key_matches("recursive delete", r"rm\s+-[^\s]*r"));
        assert!(approval_key_matches(
            "script execution via -e/-c flag",
            r"(python[23]?|perl|ruby|node)\s+-[ec]\s+"
        ));
        assert!(!approval_key_matches("recursive delete", "unrelated"));
    }
}
