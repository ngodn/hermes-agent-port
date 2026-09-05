//! Python-compatible MIME path inference with the host's mime.types overlays.
//! Built-in mapping data is captured from CPython by gen_mime_goldens.py.
#![allow(dead_code)]

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

#[derive(Deserialize)]
struct Defaults {
    types: HashMap<String, String>,
    suffixes: HashMap<String, String>,
    encodings: HashMap<String, String>,
    knownfiles: Vec<String>,
}

pub struct MimeTypes {
    defaults: Defaults,
}

impl Default for MimeTypes {
    fn default() -> Self {
        let defaults = serde_json::from_str(include_str!("../../../tools/mime-defaults.json"))
            .expect("checked CPython MIME defaults");
        Self { defaults }
    }
}

impl MimeTypes {
    /// Read overlays in Python's known-file order. No process environment or
    /// user configuration is consulted, and missing files are skipped.
    pub fn system() -> Self {
        let mut database = Self::default();
        for path in database.defaults.knownfiles.clone() {
            if let Ok(content) = std::fs::read_to_string(path) {
                database.read_types(&content);
            }
        }
        database
    }

    pub fn read_types(&mut self, content: &str) {
        for line in content.lines() {
            let mut words = line
                .split(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
                .filter(|s| !s.is_empty())
                .take_while(|s| !s.starts_with('#'));
            if let Some(mime) = words.next() {
                for suffix in words {
                    self.defaults
                        .types
                        .insert(format!(".{suffix}"), mime.into());
                }
            }
        }
    }

    /// Return (content type, compression encoding). Encoding suffixes are
    /// case-sensitive; aliases and MIME suffixes are case-insensitive.
    pub fn guess_type(&self, input: &str) -> (Option<String>, Option<String>) {
        let (scheme, path) = url_path(input);
        if scheme == "data" {
            let Some((header, _)) = path.split_once(',') else {
                return (None, None);
            };
            let mime = header.split(';').next().unwrap_or("");
            return (
                Some(
                    if mime.contains('=') || !mime.contains('/') {
                        "text/plain"
                    } else {
                        mime
                    }
                    .into(),
                ),
                None,
            );
        }
        let (base, ext) = split_extension(path);
        let mut base = base.to_owned();
        let mut ext = ext.to_owned();
        while let Some(suffix) = self.defaults.suffixes.get(&ext.to_lowercase()) {
            let expanded = format!("{base}{suffix}");
            let (new_base, new_ext) = split_extension(&expanded);
            base = new_base.into();
            ext = new_ext.into();
        }
        let encoding = self.defaults.encodings.get(&ext).cloned();
        if encoding.is_some() {
            ext = split_extension(&base).1.into();
        }
        (
            self.defaults.types.get(&ext.to_lowercase()).cloned(),
            encoding,
        )
    }
}

pub(crate) fn split_extension(path: &str) -> (&str, &str) {
    let start = path.rfind('/').map_or(0, |index| index + 1);
    if let Some(dot) = path.rfind('.') {
        if dot >= start && path[start..dot].chars().any(|c| c != '.') {
            return (&path[..dot], &path[dot..]);
        }
    }
    (path, "")
}

// Only URL paths with a multi-character scheme are substituted by Python.
// A local filename containing '?' or '#' retains those literal characters.
fn url_path(input: &str) -> (String, &str) {
    let clean = input.trim_start_matches(|c: char| c <= ' ');
    let Some((scheme, tail)) = clean.split_once(':') else {
        return (String::new(), input);
    };
    if scheme.len() <= 1
        || !scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return (String::new(), input);
    }
    let scheme = scheme.to_ascii_lowercase();
    let tail = tail.split(['?', '#']).next().unwrap_or("");
    let mut path = if let Some(authority) = tail.strip_prefix("//") {
        authority.find('/').map_or("", |index| &authority[index..])
    } else {
        tail
    };
    if matches!(
        scheme.as_str(),
        "ftp"
            | "hdl"
            | "prospero"
            | "http"
            | "imap"
            | "https"
            | "shttp"
            | "rtsp"
            | "rtsps"
            | "rtspu"
            | "sip"
            | "sips"
            | "mms"
            | "sftp"
            | "tel"
    ) {
        let start = path.rfind('/').map_or(0, |index| index + 1);
        if let Some(semi) = path[start..].find(';') {
            path = &path[..start + semi];
        }
    }
    (scheme, path)
}

static SYSTEM: LazyLock<MimeTypes> = LazyLock::new(MimeTypes::system);

pub fn guess_path_type(path: &Path) -> Option<String> {
    SYSTEM.guess_type(&path.to_string_lossy()).0
}

#[cfg(test)]
mod golden_corpus {
    use super::*;
    use serde_json::Value;

    #[test]
    fn path_inference_matches_python() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../tools/mime-goldens.json")).unwrap();
        let defaults = MimeTypes::default();
        let mut custom = MimeTypes::default();
        custom.read_types(fixture["overrides"].as_str().unwrap());
        for case in fixture["cases"].as_array().unwrap() {
            let database = if case["custom"] == true {
                &custom
            } else {
                &defaults
            };
            let (mime, encoding) = database.guess_type(case["path"].as_str().unwrap());
            assert_eq!(serde_json::json!(mime), case["mime"], "{case}");
            assert_eq!(serde_json::json!(encoding), case["encoding"], "{case}");
        }
    }
}
