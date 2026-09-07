//! Local audio conversion for the STT rejected-container retry.
//! Match the shared Python m4a encode profile and executable search order.
use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

const ENCODE_ARGS: &[&str] = &[
    "-vn",
    "-ac",
    "1",
    "-ar",
    "16000",
    "-c:a",
    "aac",
    "-b:a",
    "32k",
    "-movflags",
    "+faststart",
];

/// Shared inbound container rules from tools/audio_container.py. Audio-context
/// MP4 brands use m4a regardless of whether the brand normally denotes video.
pub fn sniff_audio_ext(data: &[u8], fallback: &str) -> String {
    let ext = if data.len() >= 8 && &data[4..8] == b"ftyp" {
        ".m4a"
    } else if data.starts_with(b"OggS") {
        ".ogg"
    } else if data.starts_with(b"fLaC") {
        ".flac"
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WAVE" {
        ".wav"
    } else if data.starts_with(b"ID3") {
        ".mp3"
    } else if data.len() >= 2 && data[0] == 255 && data[1] & 0xe0 == 0xe0 {
        if data[1] & 0xf6 == 0xf0 {
            ".aac"
        } else {
            ".mp3"
        }
    } else if data.starts_with(b"\x1a\x45\xdf\xa3") {
        ".webm"
    } else {
        return if fallback.starts_with('.') {
            fallback.into()
        } else {
            format!(".{fallback}")
        };
    };
    ext.into()
}

/// Resolve once per download. Preserve the Python implementation's negative
/// limit behavior (reject all sizes), despite its docstring saying disabled.
pub fn inbound_limit(config: &serde_json::Value) -> i64 {
    crate::python_value::integer(&config["gateway"]["max_inbound_media_bytes"])
        .and_then(|v| v.as_i64())
        .unwrap_or(128 * 1024 * 1024)
}

pub fn validate_inbound_size(size: usize, limit: i64) -> Result<()> {
    anyhow::ensure!(
        limit == 0 || (limit > 0 && size as u128 <= limit as u128),
        "Inbound audio payload is too large ({size} bytes > {limit} bytes)"
    );
    Ok(())
}

/// Read attachment responses incrementally. Authentication and allowed remote
/// destinations remain adapter-owned; the shared cache never adds credentials.
pub async fn cache_audio_response(
    home: &Path,
    response: reqwest::Response,
    fallback: &str,
    limit: i64,
) -> Result<PathBuf> {
    let bytes = read_inbound_response(response, limit).await?;
    cache_audio(home, &bytes, fallback, limit).await
}

/// Bound media bodies while streaming, including responses without a length.
/// The adapter must validate the destination before attaching credentials.
pub async fn read_inbound_response(response: reqwest::Response, limit: i64) -> Result<Vec<u8>> {
    let mut response = response.error_for_status()?;
    anyhow::ensure!(
        response.status().is_success(),
        "media download did not return a successful response"
    );
    if let Some(size) = response.content_length() {
        validate_inbound_size(usize::try_from(size)?, limit)?;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        validate_inbound_size(
            bytes
                .len()
                .checked_add(chunk.len())
                .context("media size overflow")?,
            limit,
        )?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Store in the active profile's current or legacy cache layout. Callers pass
/// trusted fallback extensions, never filenames supplied by remote senders.
pub async fn cache_audio(home: &Path, data: &[u8], fallback: &str, limit: i64) -> Result<PathBuf> {
    validate_inbound_size(data.len(), limit)?;
    let ext = sniff_audio_ext(data, fallback);
    anyhow::ensure!(!ext.contains(['/', '\\']), "invalid audio cache extension");
    let directory = crate::config_file::get_hermes_dir("cache/audio", "audio_cache", Some(home));
    tokio::fs::create_dir_all(&directory).await?;
    let id = crate::install_identity::mint_id().context("audio cache identity unavailable")?;
    let path = directory.join(format!("audio_{}{ext}", &id[..12]));
    tokio::fs::write(&path, data).await?;
    Ok(path)
}

/// The converted file owns its private work directory for the entire upload.
/// Cancellation or an HTTP error drops the same guard as successful completion.
pub struct ConvertedAudio {
    directory: PathBuf,
    pub path: PathBuf,
}

impl Drop for ConvertedAudio {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

pub fn find_binary(name: &str) -> Option<PathBuf> {
    let dirs = [
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    dirs.into_iter()
        .chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ))
        .map(|dir| dir.join(name))
        .find(|path| {
            let Ok(metadata) = path.metadata() else {
                return false;
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                metadata.is_file()
            }
        })
}

/// Validate the source before a decoder or remote upload. The source flag
/// separates the remote size limit from local engines that accept larger clips.
pub async fn validate_audio_file(path: &str, enforce_size_limit: bool) -> Result<()> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("Audio file not found: {path}")
        }
        Err(error) => anyhow::bail!("Failed to access file: {error}"),
    };
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "Path is a symbolic link: {path}"
    );
    anyhow::ensure!(metadata.is_file(), "Path is not a file: {path}");
    if enforce_size_limit {
        validate_upload_size(metadata.len())?;
    }
    const FORMATS: &[&str] = &[
        ".aac", ".caf", ".flac", ".m4a", ".mp3", ".mp4", ".mpeg", ".mpga", ".oga", ".ogg", ".opus",
        ".wav", ".webm",
    ];
    let suffix = Path::new(path)
        .extension()
        .filter(|extension| !extension.is_empty())
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    anyhow::ensure!(
        FORMATS.contains(&suffix.to_lowercase().as_str()),
        "Unsupported format: {suffix}. Supported: {}",
        FORMATS.join(", ")
    );
    Ok(())
}

pub fn validate_upload_size(size: u64) -> Result<()> {
    anyhow::ensure!(
        size <= 25 * 1024 * 1024,
        "File too large: {:.1}MB (max 25MB)",
        size as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

/// Gateway duration display rounds ties to even, then clamps negative values.
fn format_duration(seconds: f64) -> Option<String> {
    if !seconds.is_finite() || seconds >= u64::MAX as f64 {
        return None;
    }
    let total = seconds.round_ties_even().max(0.0) as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    Some(if hours == 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    })
}

/// Probe PCM WAV headers without a subprocess, then use the gateway's bounded
/// ffprobe fallback. Ogg/Opus headers use the native Mutagen-compatible path.
pub async fn probe_duration(path: &str) -> Option<String> {
    if Path::new(path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
    {
        let owned = path.to_owned();
        if let Ok(Some(seconds)) =
            tokio::task::spawn_blocking(move || wav_seconds(Path::new(&owned))).await
        {
            return format_duration(seconds);
        }
    }
    if Path::new(path).extension().is_some_and(|ext| {
        ["ogg", "opus", "oga"]
            .iter()
            .any(|suffix| ext.eq_ignore_ascii_case(suffix))
    }) {
        let owned = path.to_owned();
        if let Ok(Some(seconds)) = tokio::task::spawn_blocking(move || {
            crate::ogg_opus_duration::seconds(Path::new(&owned))
        })
        .await
        {
            return format_duration(seconds);
        }
    }
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
                path,
            ])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !result.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&result.stdout).ok()?;
    format_duration(
        text.trim_matches(crate::python_value::python_whitespace)
            .parse()
            .ok()?,
    )
}

fn wav_seconds(path: &Path) -> Option<f64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0; 12];
    file.read_exact(&mut header).ok()?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
        return None;
    }
    let end = u32::from_le_bytes(header[4..8].try_into().ok()?) as u64 + 8;
    let mut format = None;
    while file.stream_position().ok()? + 8 <= end {
        let mut chunk = [0; 8];
        file.read_exact(&mut chunk).ok()?;
        let size = u32::from_le_bytes(chunk[4..].try_into().ok()?) as u64;
        let start = file.stream_position().ok()?;
        if &chunk[..4] == b"fmt " {
            if size < 16 || start + 16 > end {
                return None;
            }
            let mut fmt = [0; 16];
            file.read_exact(&mut fmt).ok()?;
            let code = u16::from_le_bytes(fmt[..2].try_into().ok()?);
            if code == 0xfffe {
                if size < 40 || start + 40 > end {
                    return None;
                }
                let mut extra = [0; 24];
                file.read_exact(&mut extra).ok()?;
                if extra[8..] != [1, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113] {
                    return None;
                }
            } else if code != 1 {
                return None;
            }
            let channels = u16::from_le_bytes(fmt[2..4].try_into().ok()?) as u32;
            let rate = u32::from_le_bytes(fmt[4..8].try_into().ok()?);
            let width = (u16::from_le_bytes(fmt[14..16].try_into().ok()?) as u32).div_ceil(8);
            if channels == 0 || width == 0 {
                return None;
            }
            format = Some((channels * width, rate.max(1)));
        } else if &chunk[..4] == b"data" {
            let (frame_size, rate) = format?;
            return Some((size / frame_size as u64) as f64 / rate as f64);
        }
        file.seek(SeekFrom::Start(start + size + size % 2)).ok()?;
    }
    None
}

pub async fn transcode(path: &str) -> Result<ConvertedAudio> {
    let ffmpeg = find_binary("ffmpeg").ok_or_else(|| {
        anyhow::anyhow!("audio needs transcoding for the STT API, but ffmpeg was not found")
    })?;
    transcode_with(&ffmpeg, path).await
}

async fn transcode_with(ffmpeg: &Path, path: &str) -> Result<ConvertedAudio> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = loop {
        let candidate = std::env::temp_dir().join(format!(
            "hermes-stt-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let stem = Path::new(path)
        .file_stem()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("audio"));
    let output = directory.join(format!("{}-stt.m4a", stem.to_string_lossy()));
    let converted = ConvertedAudio {
        directory,
        path: output,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(ffmpeg)
            .args(["-y", "-i", path])
            .args(ENCODE_ARGS)
            .arg(&converted.path)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("failed to transcode audio for the STT API: command timed out after 120s")?
    .context("failed to transcode audio for the STT API")?;
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        let stdout = String::from_utf8_lossy(&result.stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        anyhow::bail!("failed to transcode audio for the STT API: {detail}");
    }
    Ok(converted)
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn cache_extensions_match_python() {
        let rows: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/audio-cache-goldens.json")).unwrap();
        for row in rows.as_array().unwrap() {
            let bytes: Vec<u8> = row["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect();
            assert_eq!(
                super::sniff_audio_ext(&bytes, row["fallback"].as_str().unwrap()),
                row["result"],
                "{row}"
            );
        }
        assert!(super::validate_inbound_size(0, -1).is_err());
        assert!(super::validate_inbound_size(5, 4).is_err());
        assert!(super::validate_inbound_size(4, 4).is_ok());
        assert!(super::validate_inbound_size(5, 0).is_ok());
    }
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn validation_matches_python_files_and_size_limits() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/audio-validation-goldens.json"))
                .unwrap();
        for case in cases.as_array().unwrap() {
            let root = std::env::temp_dir().join(format!(
                "hermes-audio-validation-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&root).unwrap();
            let path = root.join(case["name"].as_str().unwrap());
            match case["kind"].as_str().unwrap() {
                "file" => {
                    std::fs::File::create(&path)
                        .unwrap()
                        .set_len(case["size"].as_u64().unwrap())
                        .unwrap();
                }
                "directory" => std::fs::create_dir(&path).unwrap(),
                "symlink" => std::os::unix::fs::symlink(root.join("missing"), &path).unwrap(),
                "missing" => {}
                _ => panic!("unknown fixture kind"),
            }
            let actual =
                validate_audio_file(path.to_str().unwrap(), case["cap"].as_bool().unwrap())
                    .await
                    .err()
                    .map(|error| error.to_string().replace(root.to_str().unwrap(), "${ROOT}"));
            assert_eq!(actual.as_deref(), case["error"].as_str(), "{case}");
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn duration_and_wav_headers_match_python() {
        use base64::Engine;
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/audio-duration-goldens.json"))
                .unwrap();
        for case in cases["format"].as_array().unwrap() {
            assert_eq!(
                format_duration(case["seconds"].as_f64().unwrap()).as_deref(),
                case["expected"].as_str()
            );
        }
        let path = std::env::temp_dir().join(format!(
            "hermes-duration-test-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for case in cases["wav"].as_array().unwrap() {
            let data = base64::engine::general_purpose::STANDARD
                .decode(case["wav"].as_str().unwrap())
                .unwrap();
            std::fs::write(&path, data).unwrap();
            assert_eq!(
                wav_seconds(&path).map(f64::to_bits),
                case["bits"].as_u64(),
                "{case}"
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn conversion_workspace_is_private_and_removed_on_success_or_error() {
        for success in [true, false] {
            let root = std::env::temp_dir().join(format!(
                "hermes-conversion-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&root).unwrap();
            let script = root.join("encoder");
            // Record the output location from the actual subprocess so cleanup
            // is checked against the directory used by this invocation.
            let source = format!("#!/bin/sh\nfor output; do :; done\nprintf '%s' \"$output\" > \"$(dirname \"$0\")/captured\"\nprintf 'converted' > \"$output\"\nexit {}\n", if success { 0 } else { 1 });
            std::fs::write(&script, source).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
            let result = transcode_with(&script, "voice with spaces.wav").await;
            let path = PathBuf::from(std::fs::read_to_string(root.join("captured")).unwrap());
            if success {
                let converted = result.unwrap();
                assert_eq!(path, converted.path);
                assert_eq!(std::fs::read(&path).unwrap(), b"converted");
                assert_eq!(
                    std::fs::metadata(path.parent().unwrap())
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700
                );
                drop(converted);
            } else {
                assert!(result.is_err());
            }
            assert!(!path.parent().unwrap().exists());
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
