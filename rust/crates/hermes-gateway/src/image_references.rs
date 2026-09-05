//! Inbound image reference extraction ported from `agent/image_routing.py`.
// Runtime consumer follows rich inbound event transport.
#![allow(dead_code)]
//
// Scans free-form user/task text for image references:
// 1. Local filesystem image paths (starting with `/` or `~/`) that actually
//    exist on disk as regular files.
// 2. Remote `http://` or `https://` URLs ending in recognized image extensions
//    (with optional query parameters).
//
// References inside fenced code blocks or inline backtick code spans are ignored.
// Exact input order is preserved and duplicates are removed.

use fancy_regex::Regex;
use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

/// Image extensions recognized for model attachment, mirroring `_IMAGE_EXTS`
/// in `agent/image_routing.py`.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tiff", ".tif", ".heic",
];

// Fenced code blocks across multiple lines: ```...``` with non-greedy content.
static FENCED_CODE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```[^\n]*\n.*?```").expect("valid fenced code regex"));

// Inline code spans: `...` within a single line.
static INLINE_CODE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`[^`\n]+`").expect("valid inline code regex"));

// Local image path regex:
// Anchored to `~/` or `/`, ignoring matches inside URLs or word characters
// via negative lookbehind `(?<![/:.\p{L}\p{N}_])`.
//
// Python's Unicode \w is strictly alphanumeric letters (\p{L}), numbers (\p{N}),
// and underscore '_', excluding combining marks (\p{M}) which Rust's \w includes.
// We explicitly use `[\p{L}\p{N}_\.\-]` for path segments and `(?![\p{L}\p{N}_])`
// as the trailing word boundary after the extension.
//
// Notice that case insensitivity must be scoped only to the extension group:
// applying (?i) globally causes Rust regex to treat combining marks (e.g. U+0345)
// as matching \p{L} because their uppercase fold is a letter. Python \w never
// matches combining marks regardless of re.IGNORECASE.
//
// Under Python re.IGNORECASE, the letter 'i' folds with ASCII I, Turkish dotted I
// (U+0130, 'İ'), and Turkish dotless i (U+0131, 'ı'). We explicitly list `[iİı]`
// in extensions containing 'i' (gif, tiff, tif, heic) so Unicode case folding matches
// Python behavior exactly.
static LOCAL_IMAGE_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?<![/:.\p{L}\p{N}_])(?:~/|/)(?:[\p{L}\p{N}_\.\-]+/)*[\p{L}\p{N}_\.\-]+\.(?i:png|jpg|jpeg|g[iİı]f|webp|bmp|t[iİı]ff|t[iİı]f|he[iİı]c)(?![\p{L}\p{N}_])"#,
    )
    .expect("valid local image path regex")
});

// Remote image URL regex:
// Strict http(s) scheme, non-greedy path ending in image extension, optional query string.
// Note: In Python, \s includes the four ASCII separators U+001C..U+001F in addition
// to Unicode whitespace. We exclude `[\s\x1c-\x1f<>"']` matching Python source semantics.
// Note also that Python's _IMAGE_URL_RE does not include a trailing \b anchor, so
// URLs like `http://example.com/foo.pngabc` match up through `.png`.
static IMAGE_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i:https?://)[^\s\x1c-\x1f<>"']+?\.(?i:png|jpg|jpeg|g[iİı]f|webp|bmp|t[iİı]ff|t[iİı]f|he[iİı]c)(?:\?[^\s\x1c-\x1f<>"']*)?"#,
    )
    .expect("valid image url regex")
});

/// Expand leading `~/` using the provided home directory according to POSIX conventions.
///
/// If `home` is `/` or empty, `~/foo` becomes `/foo`.
/// If `home` has trailing slashes (e.g. `/home/user/`), they are trimmed to avoid double slashes.
/// Absolute paths (starting with `/`) are returned as-is.
fn expand_tilde(raw: &str, home: &Path) -> String {
    if let Some(rel) = raw.strip_prefix("~/") {
        let home_str = home.to_string_lossy();
        let trimmed_home = home_str.trim_end_matches('/');
        if trimmed_home.is_empty() {
            format!("/{}", rel)
        } else {
            format!("{}/{}", trimmed_home, rel)
        }
    } else {
        raw.to_string()
    }
}

/// Scan free-form text for image references the model should see.
///
/// Returns `(local_paths, urls)`:
/// - `local_paths`: absolute (`/`) or home-relative (`~/`) paths ending in an image
///   extension whose expanded form exists on disk as a file. Order-preserving, deduplicated.
/// - `urls`: `http(s)://...` URLs whose path ends in an image extension (optional `?query`
///   allowed). Order-preserving, deduplicated, trailing punctuation stripped.
///
/// Matches inside fenced code blocks (```...```) and inline code (`...`) are skipped.
pub fn extract_image_refs(text: &str, home: &Path) -> (Vec<String>, Vec<String>) {
    if text.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // Identify code block boundaries to ignore embedded example paths/URLs.
    let mut code_spans: Vec<(usize, usize)> = Vec::new();
    for m in FENCED_CODE_RE.find_iter(text).flatten() {
        code_spans.push((m.start(), m.end()));
    }
    for m in INLINE_CODE_RE.find_iter(text).flatten() {
        code_spans.push((m.start(), m.end()));
    }

    let in_code = |pos: usize| -> bool { code_spans.iter().any(|&(s, e)| s <= pos && pos < e) };

    let mut local_paths = Vec::new();
    let mut seen_paths = HashSet::new();
    for m in LOCAL_IMAGE_PATH_RE.find_iter(text).flatten() {
        if in_code(m.start()) {
            continue;
        }
        let raw = m.as_str();
        let expanded = expand_tilde(raw, home);
        // Validate against filesystem: regular file check.
        if !Path::new(&expanded).is_file() {
            continue;
        }
        if seen_paths.insert(expanded.clone()) {
            local_paths.push(expanded);
        }
    }

    let mut urls = Vec::new();
    let mut seen_urls = HashSet::new();
    for m in IMAGE_URL_RE.find_iter(text).flatten() {
        if in_code(m.start()) {
            continue;
        }
        let raw_url = m.as_str();
        // Strip trailing punctuation that belongs to surrounding prose.
        let url = raw_url
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '>'])
            .to_string();
        if seen_urls.insert(url.clone()) {
            urls.push(url);
        }
    }

    (local_paths, urls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempTestDir {
        path: std::path::PathBuf,
    }

    impl TempTestDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("hermes_test_{}_{}", name, std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempTestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn test_empty_input() {
        let home = Path::new("/home/user");
        assert_eq!(extract_image_refs("", home), (vec![], vec![]));
        assert_eq!(
            extract_image_refs("no images here at all", home),
            (vec![], vec![])
        );
    }

    #[test]
    fn test_absolute_and_home_relative_paths() {
        let fixture = TempTestDir::new("paths");
        let abs_img = fixture.path.join("screenshot.png");
        fs::write(&abs_img, b"fake png").unwrap();
        let home_img = fixture.path.join("avatar.jpg");
        fs::write(&home_img, b"fake jpg").unwrap();

        let text = format!(
            "Check absolute {} and home-relative ~/avatar.jpg for review.",
            abs_img.display()
        );
        let (paths, urls) = extract_image_refs(&text, &fixture.path);
        assert_eq!(
            paths,
            vec![
                abs_img.to_string_lossy().to_string(),
                home_img.to_string_lossy().to_string()
            ]
        );
        assert_eq!(urls, Vec::<String>::new());
    }

    #[test]
    fn test_non_existent_file_skipped() {
        let fixture = TempTestDir::new("nonexistent");
        let text = "/nonexistent/path/never_existed.png and ~/ghost.webp";
        let (paths, urls) = extract_image_refs(text, &fixture.path);
        assert!(paths.is_empty());
        assert!(urls.is_empty());
    }

    #[test]
    fn test_deduplication_and_ordering() {
        let fixture = TempTestDir::new("dedup");
        let img1 = fixture.path.join("first.png");
        let img2 = fixture.path.join("second.jpg");
        fs::write(&img1, b"1").unwrap();
        fs::write(&img2, b"2").unwrap();

        let text = format!(
            "See ~/first.png, then {}, then ~/first.png again, and finally {}.",
            img2.display(),
            img1.display()
        );
        let (paths, urls) = extract_image_refs(&text, &fixture.path);
        assert_eq!(
            paths,
            vec![
                img1.to_string_lossy().to_string(),
                img2.to_string_lossy().to_string()
            ]
        );
        assert!(urls.is_empty());
    }

    #[test]
    fn test_code_blocks_ignored() {
        let fixture = TempTestDir::new("code_blocks");
        let real = fixture.path.join("real.png");
        let ignored_fenced = fixture.path.join("fenced.png");
        let ignored_inline = fixture.path.join("inline.png");
        fs::write(&real, b"real").unwrap();
        fs::write(&ignored_fenced, b"fenced").unwrap();
        fs::write(&ignored_inline, b"inline").unwrap();

        let text = format!(
            "Real image: {}\n\
            ```bash\n\
            echo {}\n\
            curl https://example.com/fenced.png\n\
            ```\n\
            Inline example: `{}` or `https://example.com/inline.jpg`.\n\
            Real remote URL: https://example.com/real.png",
            real.display(),
            ignored_fenced.display(),
            ignored_inline.display()
        );

        let (paths, urls) = extract_image_refs(&text, &fixture.path);
        assert_eq!(paths, vec![real.to_string_lossy().to_string()]);
        assert_eq!(urls, vec!["https://example.com/real.png"]);
    }

    #[test]
    fn test_url_punctuation_strip_and_quirks() {
        let fixture = TempTestDir::new("urls");
        let text = "Look at https://example.com/a.png. and (https://example.com/b.jpg) \
                    or <https://example.com/c.gif?size=large!> plus https://example.com/d.webp?foo=bar). \
                    Query ending in question mark https://example.com/e.bmp? and dup https://example.com/a.png; \
                    Extension prefix quirk: https://example.com/f.pngabc";

        let (paths, urls) = extract_image_refs(text, &fixture.path);
        assert!(paths.is_empty());
        assert_eq!(
            urls,
            vec![
                "https://example.com/a.png",
                "https://example.com/b.jpg",
                "https://example.com/c.gif?size=large",
                "https://example.com/d.webp?foo=bar",
                "https://example.com/e.bmp",
                "https://example.com/f.png",
            ]
        );
    }

    #[test]
    fn test_posix_home_expansion_edge_cases() {
        let root = Path::new("/");
        assert_eq!(expand_tilde("~/file.png", root), "/file.png");

        let trailing_slash = Path::new("/home/user/");
        assert_eq!(
            expand_tilde("~/file.png", trailing_slash),
            "/home/user/file.png"
        );

        let normal = Path::new("/home/user");
        assert_eq!(expand_tilde("~/file.png", normal), "/home/user/file.png");

        let rel_home = Path::new("custom/dir");
        assert_eq!(expand_tilde("~/file.png", rel_home), "custom/dir/file.png");

        assert_eq!(expand_tilde("/var/img.png", normal), "/var/img.png");
    }

    #[test]
    fn test_unicode_word_chars_and_combining_marks() {
        let fixture = TempTestDir::new("unicode");
        let cjk_img = fixture.path.join("猫.png");
        fs::write(&cjk_img, b"cat").unwrap();
        let german_img = fixture.path.join("straße.jpeg");
        fs::write(&german_img, b"street").unwrap();

        let text = format!(
            "CJK path: {} and German path: {}\n\
             Combining mark before: \u{0345}{}\n\
             Combining mark after: {}\u{0301}\n\
             Word char after (rejected): {}x",
            cjk_img.display(),
            german_img.display(),
            cjk_img.display(),
            german_img.display(),
            cjk_img.display()
        );

        let (paths, _) = extract_image_refs(&text, &fixture.path);
        assert_eq!(
            paths,
            vec![
                cjk_img.to_string_lossy().to_string(),
                german_img.to_string_lossy().to_string()
            ]
        );
    }

    #[test]
    fn test_ignorecase_oddities_on_extensions() {
        let fixture = TempTestDir::new("case_oddities");
        let uppercase = fixture.path.join("img.PNG");
        fs::write(&uppercase, b"PNG").unwrap();
        let dotted_i = fixture.path.join("photo.g\u{0130}f");
        fs::write(&dotted_i, b"GIF").unwrap();
        let dotless_i = fixture.path.join("chart.t\u{0131}ff");
        fs::write(&dotless_i, b"TIFF").unwrap();

        let text = format!(
            "Images: {} and {} and {}",
            uppercase.display(),
            dotted_i.display(),
            dotless_i.display()
        );

        let (paths, _) = extract_image_refs(&text, &fixture.path);
        assert_eq!(
            paths,
            vec![
                uppercase.to_string_lossy().to_string(),
                dotted_i.to_string_lossy().to_string(),
                dotless_i.to_string_lossy().to_string(),
            ]
        );
    }

    #[test]
    fn test_non_image_extensions_and_url_lookbehind() {
        let fixture = TempTestDir::new("lookbehind");
        let txt = fixture.path.join("doc.txt");
        let pdf = fixture.path.join("report.pdf");
        fs::write(&txt, b"txt").unwrap();
        fs::write(&pdf, b"pdf").unwrap();

        let real_img = fixture.path.join("real.png");
        fs::write(&real_img, b"real").unwrap();

        let text = format!(
            "Not images: {} and {}.\n\
             URL lookbehind: https://example.com/{}\n\
             Real path: {}",
            txt.display(),
            pdf.display(),
            real_img.display(),
            real_img.display()
        );

        let (paths, urls) = extract_image_refs(&text, &fixture.path);
        assert_eq!(paths, vec![real_img.to_string_lossy().to_string()]);
        assert_eq!(
            urls,
            vec![format!("https://example.com/{}", real_img.display())]
        );
    }
}

#[cfg(test)]
mod golden_corpus {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;

    struct Home(PathBuf);
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn references_match_python_with_real_files() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../tools/image-reference-goldens.json"))
                .unwrap();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = Home(
            std::env::temp_dir().join(format!("hermes-image-refs-{}-{stamp}", std::process::id())),
        );
        std::fs::create_dir_all(&home.0).unwrap();
        for entry in fixture["files"].as_array().unwrap() {
            let target = home.0.join(entry.as_str().unwrap());
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, b"fixture").unwrap();
        }
        for entry in fixture["directories"].as_array().unwrap() {
            std::fs::create_dir_all(home.0.join(entry.as_str().unwrap())).unwrap();
        }
        for case in fixture["cases"].as_array().unwrap() {
            let text = case["text"]
                .as_str()
                .unwrap()
                .replace("__HOME__", home.0.to_str().unwrap());
            let (paths, urls) = extract_image_refs(&text, &home.0);
            let paths: Vec<_> = paths
                .iter()
                .map(|p| p.replace(home.0.to_str().unwrap(), "__HOME__"))
                .collect();
            assert_eq!(serde_json::json!(paths), case["paths"], "{case}");
            assert_eq!(serde_json::json!(urls), case["urls"], "{case}");
        }
    }
}
