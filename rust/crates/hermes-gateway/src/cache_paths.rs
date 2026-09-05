//! Cache mount/path contracts from tools/credential_files.py. The caller supplies
//! the active session's home and backend; this module does not change process env.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use crate::config_file::get_hermes_dir;

const CACHE_DIRS: &[(&str, &str)] = &[
    ("cache/documents", "document_cache"),
    ("cache/images", "image_cache"),
    ("cache/audio", "audio_cache"),
    ("cache/videos", "video_cache"),
    ("cache/screenshots", "browser_screenshots"),
    ("cache/web", "web_cache"),
    ("cache/delegation", "delegation_cache"),
    ("cache/spillover", "cache/spillover"),
    ("images", "images"),
    ("attachments", "attachments"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheMount {
    pub host_path: PathBuf,
    pub container_path: String,
}

/// Resolve the legacy layout before creating staging directories. Creating an
/// empty preferred directory first must not shadow populated legacy uploads.
pub fn get_cache_directory_mounts(home: &Path, container_base: &str) -> Vec<CacheMount> {
    CACHE_DIRS
        .iter()
        .filter_map(|(current, legacy)| {
            let host_path = get_hermes_dir(current, legacy, Some(home));
            if !host_path.is_dir() && std::fs::create_dir_all(&host_path).is_err() {
                return None;
            }
            Some(CacheMount {
                host_path,
                container_path: format!("{}/{current}", container_base.trim_end_matches('/')),
            })
        })
        .collect()
}

fn relative_posix(path: &Path, root: &Path) -> Option<String> {
    let suffix = path.strip_prefix(root).ok()?;
    if suffix.as_os_str().is_empty() {
        return Some(".".into());
    }
    Some(
        suffix
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// Lexical mapping, matching pathlib.relative_to, not a security/path-validation
/// gate. Existing media access validation remains responsible for authorization.
pub fn map_cache_path_to_container(
    home: &Path,
    host_path: &str,
    container_base: &str,
) -> Option<String> {
    let path = Path::new(host_path);
    get_cache_directory_mounts(home, container_base)
        .into_iter()
        .find_map(|mount| {
            relative_posix(path, &mount.host_path)
                .map(|suffix| format!("{}/{suffix}", mount.container_path))
        })
}

/// The plugin base is the already-resolved cache_path_base provider flag.
pub fn to_agent_visible_cache_path(
    home: &Path,
    host_path: &str,
    backend: &str,
    container_base: &str,
    plugin_base: Option<&str>,
) -> String {
    let normalized = backend
        .trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
        .to_lowercase();
    let base = match normalized.as_str() {
        "docker" | "modal" => container_base,
        "ssh" | "daytona" | "vercel_sandbox" => "~/.hermes",
        _ => match plugin_base.filter(|base| !base.is_empty()) {
            Some(base) => base,
            None => return host_path.into(),
        },
    };
    map_cache_path_to_container(home, host_path, base).unwrap_or_else(|| host_path.into())
}

/// Python's inverse applies only to an exact lowercase "docker" backend value.
pub fn from_agent_visible_cache_path(
    home: &Path,
    container_path: &str,
    backend: &str,
    container_base: &str,
) -> String {
    if backend != "docker" {
        return container_path.into();
    }
    let path = Path::new(container_path);
    for mount in get_cache_directory_mounts(home, container_base) {
        if let Ok(suffix) = path.strip_prefix(&mount.container_path) {
            let host = if suffix.as_os_str().is_empty() {
                mount.host_path
            } else {
                mount.host_path.join(suffix)
            };
            return host.to_string_lossy().into_owned();
        }
    }
    container_path.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Home(PathBuf);
    impl Home {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hermes-cache-path-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cache_paths_match_real_python_imports() {
        use serde_json::{json, Value};
        let cases: Value =
            serde_json::from_str(include_str!("../../../tools/cache-path-goldens.json")).unwrap();
        for case in cases.as_array().unwrap() {
            let home = Home::new();
            let old = home.0.join("image_cache");
            match case["layout"].as_str().unwrap() {
                "empty-legacy" => std::fs::create_dir_all(&old).unwrap(),
                "populated-legacy" => {
                    std::fs::create_dir_all(&old).unwrap();
                    std::fs::write(old.join("existing.png"), b"image").unwrap();
                }
                "file-legacy" => std::fs::write(&old, b"not a directory").unwrap(),
                "fresh" => {}
                layout => panic!("unknown layout {layout}"),
            }
            let normalize = |value: &str| value.replace(home.0.to_str().unwrap(), "__HOME__");
            let mounts: Vec<_> = get_cache_directory_mounts(&home.0, "/remote/").into_iter()
                .map(|m| json!({"host_path": normalize(m.host_path.to_str().unwrap()), "container_path": m.container_path})).collect();
            assert_eq!(json!(mounts), case["mounts"], "layout {}", case["layout"]);
            for check in case["checks"].as_array().unwrap() {
                let host = check["host"]
                    .as_str()
                    .unwrap()
                    .replace("__HOME__", home.0.to_str().unwrap());
                let backend = check["backend"].as_str().unwrap();
                let mapped = to_agent_visible_cache_path(&home.0, &host, backend, "/remote/", None);
                assert_eq!(
                    normalize(&mapped),
                    check["mapped"].as_str().unwrap(),
                    "{check}"
                );
                let inverse = from_agent_visible_cache_path(&home.0, &mapped, backend, "/remote/");
                assert_eq!(
                    normalize(&inverse),
                    check["inverse"].as_str().unwrap(),
                    "{check}"
                );
            }
        }
    }

    #[test]
    fn existing_legacy_uploads_map_to_current_remote_layout() {
        let home = Home::new();
        std::fs::create_dir_all(home.0.join("image_cache")).unwrap();
        let upload = home.0.join("image_cache/photo.png");
        std::fs::write(&upload, b"image").unwrap();
        let host = upload.to_str().unwrap();
        assert_eq!(
            to_agent_visible_cache_path(&home.0, host, " docker ", "/root/.hermes/", None),
            "/root/.hermes/cache/images/photo.png"
        );
        assert_eq!(
            from_agent_visible_cache_path(
                &home.0,
                "/root/.hermes/cache/images/photo.png",
                "docker",
                "/root/.hermes"
            ),
            host
        );
        assert!(!home.0.join("cache/images").exists());
    }

    #[test]
    fn empty_legacy_stubs_do_not_shadow_staging_and_remote_backends() {
        let home = Home::new();
        std::fs::create_dir_all(home.0.join("image_cache")).unwrap();
        let host = home.0.join("cache/images/photo.png");
        let host = host.to_str().unwrap();
        for backend in ["ssh", "daytona", "vercel_sandbox"] {
            assert_eq!(
                to_agent_visible_cache_path(&home.0, host, backend, "/ignored", None),
                "~/.hermes/cache/images/photo.png"
            );
        }
        assert!(home.0.join("cache/images").is_dir());
        for backend in ["local", "singularity", "unknown"] {
            assert_eq!(
                to_agent_visible_cache_path(&home.0, host, backend, "/ignored", None),
                host
            );
        }
        assert_eq!(
            to_agent_visible_cache_path(&home.0, host, "plugin", "/ignored", Some("/remote/cache")),
            "/remote/cache/cache/images/photo.png"
        );
    }

    #[test]
    fn exact_roots_siblings_and_profiles_remain_distinct() {
        let home = Home::new();
        let root = home.0.join("attachments");
        assert_eq!(
            map_cache_path_to_container(&home.0, root.to_str().unwrap(), "/remote"),
            Some("/remote/attachments/.".into())
        );
        let sibling = home.0.join("attachments-other/file");
        assert_eq!(
            map_cache_path_to_container(&home.0, sibling.to_str().unwrap(), "/remote"),
            None
        );
        let other = Home::new();
        assert_eq!(
            map_cache_path_to_container(&other.0, root.to_str().unwrap(), "/remote"),
            None
        );
        assert_eq!(
            from_agent_visible_cache_path(&home.0, "/remote/images/a", " Docker ", "/remote"),
            "/remote/images/a"
        );
    }

    #[test]
    fn local_paths_do_not_create_mount_directories() {
        let home = Home::new();
        assert_eq!(
            to_agent_visible_cache_path(&home.0, "relative/file", "local", "/remote", None),
            "relative/file"
        );
        assert_eq!(std::fs::read_dir(&home.0).unwrap().count(), 0);
    }
}
