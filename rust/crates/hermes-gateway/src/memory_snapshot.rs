//! Frozen, read-only memory capture from tools/memory_tool.py. Memory-tool
//! writes must maintain separate live state and never mutate this snapshot.
#![allow(dead_code)]

use std::path::Path;
const DELIMITER: &str = "\n§\n";

#[derive(Default)]
pub struct MemorySnapshot {
    memory: String,
    user: String,
}

fn read_entries(path: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let raw = raw
        .strip_prefix('\u{feff}')
        .unwrap_or(&raw)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut seen = std::collections::HashSet::new();
    raw.split(DELIMITER)
        .map(|entry| entry.trim_matches(crate::python_value::python_whitespace))
        .filter(|entry| !entry.is_empty() && seen.insert((*entry).to_owned()))
        .map(str::to_owned)
        .collect()
}

fn comma_number(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = if value < 0 { "-".into() } else { String::new() };
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn render(entries: Vec<String>, filename: &str, header: &str, limit: i64) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let sanitized: Vec<_> = entries.into_iter().map(|entry| {
        if entry.starts_with("[BLOCKED:") { return entry; }
        let findings = crate::threat_patterns::scan_for_threats(&entry,"strict");
        if findings.is_empty() { entry } else {
            tracing::warn!(filename, patterns = %findings.join(", "), "Memory entry blocked in prompt snapshot");
            format!("[BLOCKED: {filename} entry contained threat pattern(s): {}. Removed from system prompt; use memory(action=remove) to delete the original.]",findings.join(", "))
        }
    }).collect();
    let content = sanitized.join(DELIMITER);
    let count = content.chars().count() as i64;
    let percent = if limit > 0 {
        ((count as f64 / limit as f64 * 100.0) as i64).min(100)
    } else {
        0
    };
    let separator = "═".repeat(46);
    format!(
        "{separator}\n{header} [{percent}% \u{2014} {}/{} chars]\n{separator}\n{content}",
        comma_number(count),
        comma_number(limit)
    )
}

impl MemorySnapshot {
    /// Directory creation failures propagate, but unreadable/invalid UTF-8
    /// files become empty snapshots. This API never writes memory contents.
    pub fn load(home: &Path, memory_limit: i64, user_limit: i64) -> std::io::Result<Self> {
        let directory = home.join("memories");
        std::fs::create_dir_all(&directory)?;
        Ok(Self {
            memory: render(
                read_entries(&directory.join("MEMORY.md")),
                "MEMORY.md",
                "MEMORY (your personal notes)",
                memory_limit,
            ),
            user: render(
                read_entries(&directory.join("USER.md")),
                "USER.md",
                "USER PROFILE (who the user is)",
                user_limit,
            ),
        })
    }

    pub fn memory(&self) -> Option<&str> {
        (!self.memory.is_empty()).then_some(&self.memory)
    }
    pub fn user(&self) -> Option<&str> {
        (!self.user.is_empty()).then_some(&self.user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_match_python_filesystem_cases_and_target_gates() {
        let base =
            std::env::temp_dir().join(format!("hermes-memory-oracle-{}", std::process::id()));
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../tools/memory-snapshot-goldens.json"))
                .unwrap();
        for (index, case) in cases.iter().enumerate() {
            let home = base.join(index.to_string());
            std::fs::create_dir_all(home.join("memories")).unwrap();
            for (key, file) in [("memory_text", "MEMORY.md"), ("user_text", "USER.md")] {
                if let Some(text) = case[key].as_str() {
                    std::fs::write(home.join("memories").join(file), text).unwrap();
                }
            }
            let snapshot = MemorySnapshot::load(
                &home,
                case["memory_limit"].as_i64().unwrap(),
                case["user_limit"].as_i64().unwrap(),
            )
            .unwrap();
            assert_eq!(
                snapshot.memory(),
                case["expected_memory"].as_str(),
                "memory case {index}"
            );
            assert_eq!(
                snapshot.user(),
                case["expected_user"].as_str(),
                "user case {index}"
            );
            for memory in [false, true] {
                for user in [false, true] {
                    let mut sections = crate::system_prompt::ResolvedPromptSections::default();
                    sections.set_memory_snapshot(Some(&snapshot), memory, user);
                    assert_eq!(
                        sections.memory.as_deref(),
                        snapshot.memory().filter(|_| memory)
                    );
                    assert_eq!(
                        sections.user_profile.as_deref(),
                        snapshot.user().filter(|_| user)
                    );
                }
            }
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn snapshot_is_frozen_and_loaded_files_are_never_rewritten() {
        let root =
            std::env::temp_dir().join(format!("hermes-memory-snapshot-{}", std::process::id()));
        std::fs::create_dir_all(root.join("memories")).unwrap();
        let path = root.join("memories/MEMORY.md");
        let original = "\u{feff}one\r\n§\r\none\r\n§\r\ntwo § inline";
        std::fs::write(&path, original).unwrap();
        std::fs::write(root.join("memories/USER.md"), b"\xff").unwrap();
        let snapshot = MemorySnapshot::load(&root, 2200, 1375).unwrap();
        assert!(snapshot.memory().unwrap().ends_with("one\n§\ntwo § inline"));
        assert!(snapshot.user().is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        std::fs::write(&path, "new state").unwrap();
        assert!(!snapshot.memory().unwrap().contains("new state"));
        let mut sections = crate::system_prompt::ResolvedPromptSections::default();
        sections.set_memory_snapshot(Some(&snapshot), true, true);
        assert_eq!(sections.assemble().volatile, snapshot.memory().unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }
}
