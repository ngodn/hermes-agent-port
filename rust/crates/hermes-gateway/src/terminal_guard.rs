//! Unconditional security floor for the native local terminal.
//!
//! Approval modes may choose how to handle recoverable risk, but these checks
//! are never bypassed. The scanner is shell-position aware so dangerous prose
//! stays harmless while commands nested in shell carriers are still examined.

use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub description: &'static str,
}

const MAX_SCAN_BYTES: usize = 256 * 1024;
const MAX_SCAN_DEPTH: usize = 8;

/// Return the first unconditional reason a command must not execute.
pub fn unconditional_block(command: &str, sudo_password_configured: bool) -> Option<Block> {
    if command.len() > MAX_SCAN_BYTES {
        return Some(Block {
            description: "command exceeds security parser limit",
        });
    }
    scan(command, sudo_password_configured, 0)
}

fn scan(command: &str, sudo_password_configured: bool, depth: usize) -> Option<Block> {
    if depth > MAX_SCAN_DEPTH {
        return Some(Block {
            description: "command exceeds security parser limit",
        });
    }
    if !sudo_password_configured
        && command_segments(command)
            .iter()
            .any(|s| sudo_stdin(s, depth))
    {
        return Some(Block {
            description: "sudo password guessing via stdin (sudo -S)",
        });
    }
    if is_fork_bomb(&quote_masked(command)) {
        return block("fork bomb");
    }
    if redirects_to_raw_device(&quote_masked(command)) {
        return block("redirect to raw block device");
    }
    for segment in command_segments(command) {
        if let Some(found) = scan_words(&segment, sudo_password_configured, depth) {
            return Some(found);
        }
    }
    for nested in command_substitutions(command) {
        if let Some(found) = scan(&nested, sudo_password_configured, depth + 1) {
            return Some(found);
        }
    }
    None
}

fn block(description: &'static str) -> Option<Block> {
    Some(Block { description })
}

fn scan_words(segment: &str, sudo_password_configured: bool, depth: usize) -> Option<Block> {
    let words = match shell_words::split(segment) {
        Ok(words) => words,
        Err(_) => return block("malformed executable command"),
    };
    if words.is_empty() {
        return None;
    }
    let (command_index, name) = command_word(&words)?;
    let args = &words[command_index + 1..];

    if is_shell_carrier(&name) {
        if let Some(payload) = carrier_payload(&name, args) {
            if let Some(found) = scan(payload, sudo_password_configured, depth + 1) {
                return Some(found);
            }
        }
    }

    if name == "rm" && recursive_rm(args) {
        for target in args.iter().filter(|word| !word.starts_with('-')) {
            if root_target(target) {
                return block("recursive delete of root filesystem");
            }
            if system_target(target) {
                return block("recursive delete of system directory");
            }
            if home_target(target) {
                return block("recursive delete of home directory");
            }
        }
    }
    if name == "mkfs" || name.starts_with("mkfs.") {
        return block("format filesystem (mkfs)");
    }
    if name == "dd" && args.iter().any(|arg| raw_device_assignment(arg, "of=")) {
        return block("dd to raw block device");
    }
    if name == "kill" && args.iter().any(|arg| arg == "-1") {
        return block("kill all processes");
    }
    if matches!(name.as_str(), "shutdown" | "reboot" | "halt" | "poweroff") {
        return block("system shutdown/reboot");
    }
    if name == "init"
        && args
            .first()
            .is_some_and(|arg| matches!(arg.as_str(), "0" | "6"))
    {
        return block("init 0/6 (shutdown/reboot)");
    }
    if name == "telinit"
        && args
            .first()
            .is_some_and(|arg| matches!(arg.as_str(), "0" | "6"))
    {
        return block("telinit 0/6 (shutdown/reboot)");
    }
    if name == "systemctl"
        && args
            .first()
            .is_some_and(|arg| matches!(arg.as_str(), "poweroff" | "reboot" | "halt" | "kexec"))
    {
        return block("systemctl poweroff/reboot");
    }
    None
}

fn command_word(words: &[String]) -> Option<(usize, String)> {
    let mut index = 0;
    loop {
        let raw = words.get(index)?;
        let name = Path::new(raw)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(raw)
            .to_ascii_lowercase();
        match name.as_str() {
            "sudo" => {
                index += 1;
                while let Some(flag) = words.get(index).filter(|word| word.starts_with('-')) {
                    let takes_value =
                        matches!(flag.as_str(), "-u" | "-g" | "-h" | "-p" | "-C" | "-T");
                    index += 1;
                    if takes_value {
                        index += 1;
                    }
                }
            }
            "env" => {
                index += 1;
                while words.get(index).is_some_and(|word| {
                    word.contains('=') && !word.starts_with('=') && !word.starts_with('-')
                }) {
                    index += 1;
                }
            }
            "exec" | "nohup" | "setsid" | "time" => index += 1,
            _ => return Some((index, name)),
        }
    }
}

fn is_shell_carrier(name: &str) -> bool {
    matches!(
        name,
        "eval" | "sh" | "bash" | "zsh" | "ksh" | "dash" | "source" | "."
    )
}

fn carrier_payload<'a>(name: &str, args: &'a [String]) -> Option<&'a str> {
    if matches!(name, "eval" | "source" | ".") {
        return args.first().map(String::as_str);
    }
    args.iter()
        .position(|arg| arg == "-c" || arg.ends_with('c') && arg.starts_with('-'))
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

fn recursive_rm(args: &[String]) -> bool {
    args.iter().filter(|arg| arg.starts_with('-')).any(|arg| {
        arg == "--recursive"
            || arg
                .trim_start_matches('-')
                .chars()
                .any(|flag| flag == 'r' || flag == 'R')
    })
}

fn root_target(target: &str) -> bool {
    let target = target.strip_suffix('*').unwrap_or(target);
    if !target.starts_with('/') {
        return false;
    }
    target
        .split('/')
        .all(|component| component.is_empty() || component == "." || component == "..")
}

fn system_target(target: &str) -> bool {
    let target = target
        .strip_suffix("/*")
        .unwrap_or(target)
        .trim_end_matches('/');
    matches!(
        target,
        "/home" | "/root" | "/etc" | "/usr" | "/var" | "/bin" | "/sbin" | "/boot" | "/lib"
    )
}

fn home_target(target: &str) -> bool {
    let target = target
        .strip_suffix("/*")
        .unwrap_or(target)
        .trim_end_matches('/');
    matches!(target, "~" | "$HOME" | "${HOME}")
}

fn raw_device_assignment(word: &str, prefix: &str) -> bool {
    word.strip_prefix(prefix).is_some_and(raw_device)
}

fn raw_device(path: &str) -> bool {
    let Some(name) = path.strip_prefix("/dev/") else {
        return false;
    };
    ["sd", "nvme", "hd", "mmcblk", "vd", "xvd"]
        .iter()
        .any(|prefix| {
            name.starts_with(prefix)
                && name[prefix.len()..]
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric())
        })
}

fn redirects_to_raw_device(command: &str) -> bool {
    for (index, _) in command.match_indices('>') {
        let tail = command[index + 1..].trim_start_matches(['>', ' ', '\t']);
        let target = tail
            .split(|ch: char| ch.is_whitespace() || matches!(ch, ';' | '&' | '|'))
            .next()
            .unwrap_or("");
        if raw_device(target) {
            return true;
        }
    }
    false
}

fn is_fork_bomb(command: &str) -> bool {
    static PATTERN: std::sync::LazyLock<fancy_regex::Regex> = std::sync::LazyLock::new(|| {
        fancy_regex::Regex::new(r":\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;\s*:")
            .expect("fixed fork-bomb pattern")
    });
    PATTERN.is_match(command).unwrap_or(false)
}

fn sudo_stdin(segment: &str, depth: usize) -> bool {
    let Ok(words) = shell_words::split(segment) else {
        return false;
    };
    let mut sudo_index = 0;
    while let Some(word) = words.get(sudo_index) {
        if matches!(word.as_str(), "exec" | "nohup" | "setsid" | "time") || word.contains('=') {
            sudo_index += 1;
            continue;
        }
        if word == "env" {
            sudo_index += 1;
            continue;
        }
        break;
    }
    if words.get(sudo_index).is_some_and(|word| {
        Path::new(word)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("sudo"))
    }) && words[sudo_index + 1..].iter().any(|word| word == "-S")
    {
        return true;
    }
    let Some((index, name)) = command_word(&words) else {
        return false;
    };
    if is_shell_carrier(&name) {
        return carrier_payload(&name, &words[index + 1..]).is_some_and(|payload| {
            scan(payload, false, depth + 1).is_some_and(|b| b.description.contains("sudo password"))
        });
    }
    false
}

fn command_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            current.push(ch);
            escaped = true;
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            }
            current.push(ch);
            continue;
        }
        if quote.is_none() && matches!(ch, ';' | '&' | '|' | '\n' | '`') {
            if !current.trim().is_empty() {
                segments.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.trim().is_empty() {
        segments.push(current);
    }
    segments
}

fn quote_masked(command: &str) -> String {
    let mut output = String::with_capacity(command.len());
    let mut quote = None;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            output.push(' ');
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            output.push(if quote.is_some() { ' ' } else { ch });
            escaped = true;
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            }
            output.push(ch);
        } else if quote.is_some() {
            output.push(' ');
        } else {
            output.push(ch);
        }
    }
    output
}

fn command_substitutions(command: &str) -> Vec<String> {
    let bytes = command.as_bytes();
    let mut nested = Vec::new();
    let mut quote = None;
    let mut index = 0;
    while index + 1 < bytes.len() {
        let ch = bytes[index] as char;
        if ch == '\'' && quote != Some('"') {
            quote = if quote == Some('\'') {
                None
            } else {
                Some('\'')
            };
            index += 1;
            continue;
        }
        if ch == '"' && quote != Some('\'') {
            quote = if quote == Some('"') { None } else { Some('"') };
            index += 1;
            continue;
        }
        if quote != Some('\'') && ch == '$' && bytes[index + 1] == b'(' {
            let start = index + 2;
            let mut depth = 1;
            let mut cursor = start;
            while cursor < bytes.len() {
                if cursor + 1 < bytes.len() && bytes[cursor] == b'$' && bytes[cursor + 1] == b'(' {
                    depth += 1;
                    cursor += 2;
                    continue;
                }
                if bytes[cursor] == b')' {
                    depth -= 1;
                    if depth == 0 {
                        nested.push(command[start..cursor].to_string());
                        index = cursor;
                        break;
                    }
                }
                cursor += 1;
            }
        }
        index += 1;
    }
    nested
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn matches_python_hardline_and_sudo_contract_corpus() {
        let corpus: Value = serde_json::from_str(include_str!(
            "../../../tools/terminal-contract-goldens.json"
        ))
        .unwrap();
        for case in corpus["hardline_catastrophic_commands"].as_array().unwrap() {
            let command = case["command"].as_str().unwrap();
            let expected = case["is_hardline"].as_bool().unwrap();
            let actual = unconditional_block(command, true);
            assert_eq!(actual.is_some(), expected, "{}", case["id"]);
            if expected {
                assert_eq!(
                    actual.unwrap().description,
                    case["description"].as_str().unwrap(),
                    "{}",
                    case["id"]
                );
            }
        }
        for case in corpus["sudo_stdin_guessing"].as_array().unwrap() {
            let actual = unconditional_block(
                case["command"].as_str().unwrap(),
                case["sudo_password_configured"].as_bool().unwrap(),
            );
            assert_eq!(
                actual
                    .as_ref()
                    .is_some_and(|block| block.description.contains("sudo password")),
                case["is_blocked"].as_bool().unwrap(),
                "{}",
                case["id"]
            );
        }
    }

    #[test]
    fn scans_nested_commands_without_blocking_quoted_prose() {
        assert!(unconditional_block("echo $(sudo rm -rf /)", true).is_some());
        assert!(unconditional_block("bash -c 'dd if=x of=/dev/sda'", true).is_some());
        assert!(unconditional_block("echo 'rm -rf / and reboot'", true).is_none());
        assert!(unconditional_block("git commit -m 'avoid > /dev/sda'", true).is_none());
    }
}
