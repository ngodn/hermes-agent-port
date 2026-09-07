//! Profile-scoped cwd precedence from agent/runtime_cwd.py. Inputs are captured
//! by the caller; refusal to access a terminal scope must propagate before
//! constructing these inputs, never fall back to another profile's environment.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

pub struct CwdInputs<'a> {
    pub session: Option<&'a str>,
    pub terminal: &'a str,
    pub launch: &'a Path,
    pub home: &'a Path,
}

impl CwdInputs<'_> {
    fn configured(&self, raw: &str) -> std::io::Result<Option<PathBuf>> {
        let raw = raw.trim_matches(crate::python_value::python_whitespace);
        if raw.is_empty() {
            return Ok(None);
        }
        let path = PathBuf::from(crate::file_read_safety::expand_user(raw, self.home)?);
        // Keep relative spelling in the result, as pathlib does, but validate
        // relative to the captured process cwd rather than mutable global cwd.
        if self.launch.join(&path).is_dir() {
            Ok(Some(path))
        } else {
            tracing::warn!(path = raw, "configured working directory does not exist");
            Ok(None)
        }
    }

    pub fn agent_cwd(&self) -> std::io::Result<PathBuf> {
        if let Some(path) = self.configured(self.session.unwrap_or(""))? {
            return Ok(path);
        }
        Ok(self
            .configured(self.terminal)?
            .unwrap_or_else(|| self.launch.to_owned()))
    }

    /// An invalid session override blocks context fallback to terminal cwd.
    /// Agent cwd intentionally continues through that fallback instead.
    pub fn context_cwd(&self) -> std::io::Result<Option<PathBuf>> {
        let session = self
            .session
            .unwrap_or("")
            .trim_matches(crate::python_value::python_whitespace);
        self.configured(if session.is_empty() {
            self.terminal
        } else {
            session
        })
    }

    /// Explicit coding cwd bypasses directory validation and whitespace trim.
    /// Only errors in the implicit runtime resolver fall back to launch cwd.
    pub fn coding_cwd(&self, explicit: Option<&str>) -> std::io::Result<PathBuf> {
        if let Some(raw) = explicit.filter(|raw| !raw.is_empty()) {
            return crate::file_read_safety::expand_user(raw, self.home).map(PathBuf::from);
        }
        Ok(self.agent_cwd().unwrap_or_else(|_| self.launch.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_consumers_preserve_distinct_fallback_rules() {
        let root = std::env::temp_dir().join(format!("hermes-runtime-cwd-{}", std::process::id()));
        std::fs::create_dir_all(root.join("project")).unwrap();
        let inputs = CwdInputs {
            session: Some(" missing "),
            terminal: " ~/project ",
            launch: &root,
            home: &root,
        };
        assert_eq!(inputs.agent_cwd().unwrap(), root.join("project"));
        assert_eq!(inputs.context_cwd().unwrap(), None);
        std::fs::write(root.join("project/Cargo.toml"), "[package]\n").unwrap();
        let mode = crate::coding_context::RuntimeMode::from_scope(
            "cli",
            Some("~/project"),
            &inputs,
            Some(&std::env::temp_dir()),
            &serde_json::json!({}),
            None,
        )
        .unwrap();
        assert!(mode.profile().is_coding());
        assert_eq!(mode.cwd(), root.join("project"));
        assert_eq!(inputs.coding_cwd(None).unwrap(), root.join("project"));
        assert_eq!(
            inputs.coding_cwd(Some("~/missing")).unwrap(),
            root.join("missing")
        );
        assert_eq!(inputs.coding_cwd(Some("  ")).unwrap(), PathBuf::from("  "));
        let inputs = CwdInputs {
            session: Some(" project "),
            ..inputs
        };
        assert_eq!(inputs.agent_cwd().unwrap(), PathBuf::from("project"));
        assert_eq!(
            inputs.context_cwd().unwrap(),
            Some(PathBuf::from("project"))
        );
        let inputs = CwdInputs {
            session: None,
            terminal: "missing",
            ..inputs
        };
        assert_eq!(inputs.agent_cwd().unwrap(), root);
        assert_eq!(inputs.context_cwd().unwrap(), None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
