//! Native image attachment handling ported from `agent/image_routing.py`.
//!
//! Provides MIME sniffing from magic bytes, format negotiation against accepted provider
//! capabilities, PNG transcoding for unsupported formats (BMP, TIFF, etc.) to prevent
//! provider HTTP 400 rejections, and construction of OpenAI-style native image content parts.
#![allow(dead_code)]

use std::fs;
use std::io::Cursor;
use std::path::Path;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use image::{DynamicImage, ImageFormat};

use crate::file_read_safety::FileReadPolicy;

/// Formats accepted natively by all major cloud vision providers (Anthropic, OpenAI,
/// Gemini, Bedrock).
///
/// Images in formats outside this set (such as BMP, TIFF, or ICO) must be transcoded
/// to PNG before attaching to prevent provider HTTP 400 errors ("Could not process image").
pub const UNIVERSALLY_SUPPORTED_MIMES: &[&str] =
    &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Options governing native image attachment, including read safety policy
/// and the provider's accepted MIME types.
#[derive(Debug, Clone, Copy)]
pub struct NativeImageOptions<'a> {
    pub read_policy: &'a FileReadPolicy,
    pub accepted_mimes: &'a [&'a str],
}

/// Detect image MIME type from magic bytes.
///
/// Preserves exact signature order from `agent/image_routing.py`:
/// 1. PNG: `89 50 4E 47 0D 0A 1A 0A`
/// 2. JPEG: `FF D8 FF`
/// 3. GIF: `GIF87a` / `GIF89a`
/// 4. WEBP: `RIFF` .... `WEBP`
/// 5. BMP: `BM`
/// 6. ISO-BMFF family: `ftyp` at offset 4..8, checking brands `avif`, `avis`, and `heic`/`heix`/etc.
/// 7. TIFF: `II*\0` (little-endian) or `MM\0*` (big-endian)
/// 8. ICO: `00 00 01 00`
/// 9. SVG: text-based `<svg` check within first 512 bytes (ASCII lstrip and lowercase).
///
/// Note on HEIC/AVIF: While magic bytes for `image/heic` and `image/avif` are sniffed
/// accurately here, the default `image` crate (0.25) configuration does not bundle HEIF/AVIF
/// decoders. They will fail transcoding to PNG and be skipped gracefully by callers.
pub fn sniff_mime_from_bytes(raw: &[u8]) -> Option<&'static str> {
    if raw.is_empty() {
        return None;
    }

    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if raw.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }

    // JPEG: FF D8 FF
    if raw.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }

    // GIF87a / GIF89a
    if raw.len() >= 6 && (&raw[..6] == b"GIF87a" || &raw[..6] == b"GIF89a") {
        return Some("image/gif");
    }

    // WEBP: "RIFF" .... "WEBP"
    if raw.len() >= 12 && &raw[..4] == b"RIFF" && &raw[8..12] == b"WEBP" {
        return Some("image/webp");
    }

    // BMP: "BM"
    if raw.starts_with(b"BM") {
        return Some("image/bmp");
    }

    // ISO-BMFF family (HEIC/HEIF/AVIF): bytes 4..8 == 'ftyp', major brand at 8..12
    if raw.len() >= 12 && &raw[4..8] == b"ftyp" {
        let brand = &raw[8..12];
        if brand == b"avif" || brand == b"avis" {
            return Some("image/avif");
        }
        if matches!(
            brand,
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" | b"heim" | b"heis"
        ) {
            return Some("image/heic");
        }
    }

    // TIFF: II*\0 (little-endian) or MM\0* (big-endian)
    if raw.len() >= 4 && (&raw[..4] == b"II*\x00" || &raw[..4] == b"MM\x00*") {
        return Some("image/tiff");
    }

    // ICO: 00 00 01 00 (reserved=0, type=1=icon)
    if raw.len() >= 4 && &raw[..4] == b"\x00\x00\x01\x00" {
        return Some("image/x-icon");
    }

    // SVG: text-based, look for an <svg tag near the start. Like Python's
    // bytes.lstrip, this removes ASCII whitespace but does not remove a BOM.
    // In Python bytes.lstrip() strips ASCII whitespace: b' \t\n\r\x0b\x0c'.
    let limit = raw.len().min(512);
    let slice = &raw[..limit];
    let start = slice
        .iter()
        .position(|&b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C))
        .unwrap_or(slice.len());
    let stripped = &slice[start..];
    let head: Vec<u8> = stripped.iter().map(|b| b.to_ascii_lowercase()).collect();
    if (head.starts_with(b"<?xml") || head.starts_with(b"<svg"))
        && head.windows(4).any(|w| w == b"<svg")
    {
        return Some("image/svg+xml");
    }

    None
}

/// Determine MIME type for `path`.
///
/// When `raw` bytes are provided, magic-byte sniffing takes precedence.
/// Otherwise, falls back to Python's MIME defaults plus host mime.types files,
/// then the source's image suffix defaults.
pub fn guess_mime(path: &Path, raw: Option<&[u8]>) -> String {
    if let Some(bytes) = raw {
        if let Some(sniffed) = sniff_mime_from_bytes(bytes) {
            return sniffed.to_string();
        }
    }
    if let Some(guess) = crate::mime_types::guess_path_type(path) {
        if guess.starts_with("image/") {
            return guess.to_string();
        }
    }
    let suffix = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|s| s.to_ascii_lowercase());
    match suffix.as_deref() {
        Some("jpg") | Some("jpeg") => "image/jpeg".to_string(),
        Some("png") => "image/png".to_string(),
        Some("gif") => "image/gif".to_string(),
        Some("webp") => "image/webp".to_string(),
        Some("bmp") => "image/bmp".to_string(),
        _ => "image/jpeg".to_string(),
    }
}

/// Decode arbitrary image bytes and re-encode as lossless PNG.
///
/// Returns `None` if the format cannot be decoded (e.g. vector SVG, missing optional
/// HEIC/AVIF decoders, or corrupted bytes).
/// Original image dimensions and color/alpha channels are preserved.
pub fn transcode_to_png(raw: &[u8]) -> Option<Vec<u8>> {
    let decoded = decode_numeric_tiff(raw)
        .map(Ok)
        .unwrap_or_else(|| image::load_from_memory(raw));
    let img = match decoded {
        Ok(img) => img,
        Err(err) => {
            tracing::info!("image_routing: image could not decode input: {}", err);
            return None;
        }
    };

    let mut cursor = Cursor::new(Vec::new());
    if let Err(err) = img.write_to(&mut cursor, ImageFormat::Png) {
        // Fall back to converting to RGBA8 to preserve transparency if writing the
        // current DynamicImage mode directly failed.
        cursor.get_mut().clear();
        cursor.set_position(0);
        let rgba = DynamicImage::ImageRgba8(img.to_rgba8());
        if let Err(retry_err) = rgba.write_to(&mut cursor, ImageFormat::Png) {
            tracing::info!(
                "image_routing: image could not transcode image to PNG: {} (retry failed: {})",
                err,
                retry_err
            );
            return None;
        }
    }
    Some(cursor.into_inner())
}

/// Pillow converts its integer/float grayscale modes to RGBA by clipping to
/// 0..255. DynamicImage lacks signed 32-bit grayscale and scales 16-bit values,
/// so use the TIFF decoder directly for these modes before PNG encoding.
fn decode_numeric_tiff(raw: &[u8]) -> Option<DynamicImage> {
    use tiff::decoder::{Decoder, DecodingResult};
    if sniff_mime_from_bytes(raw) != Some("image/tiff") {
        return None;
    }
    let mut decoder = Decoder::new(Cursor::new(raw)).ok()?;
    if !matches!(decoder.colortype().ok()?, tiff::ColorType::Gray(bits) if bits > 8) {
        return None;
    }
    let (width, height) = decoder.dimensions().ok()?;
    let mut samples = DecodingResult::U8(Vec::new());
    decoder.read_image_to_buffer(&mut samples).ok()?;
    let values: Vec<u8> = match samples {
        DecodingResult::I16(v) => v.into_iter().map(|v| v.clamp(0, 255) as u8).collect(),
        DecodingResult::I32(v) => v.into_iter().map(|v| v.clamp(0, 255) as u8).collect(),
        DecodingResult::U16(v) => v.into_iter().map(|v| v.min(255) as u8).collect(),
        DecodingResult::U32(v) => v
            .into_iter()
            .map(|v| (v as i32).clamp(0, 255) as u8)
            .collect(),
        DecodingResult::F32(v) => v.into_iter().map(|v| v.clamp(0.0, 255.0) as u8).collect(),
        _ => return None,
    };
    let gray = image::GrayImage::from_raw(width, height, values)?;
    Some(DynamicImage::ImageLuma8(gray).to_rgba8().into())
}

/// Encode a local image file as a base64 `data:` URL at its native size.
///
/// Enforces read safety policy before performing disk reads.
/// Formats accepted in `options.accepted_mimes` pass through unchanged (even if corrupt).
/// Unsupported formats (e.g. BMP, TIFF) are transcoded to PNG if a decoder is available;
/// otherwise, returns `None` so callers can report the attachment as skipped.
/// No image resizing or artificial size limits are applied here.
pub fn file_to_data_url(path: &Path, options: &NativeImageOptions<'_>) -> Option<String> {
    let path_str = path.to_string_lossy();
    if let Err(err) = options.read_policy.check_read(&path_str) {
        tracing::warn!(
            "image_routing: blocked local image attachment {} -- {}",
            path.display(),
            err
        );
        return None;
    }

    let mut raw = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(
                "image_routing: failed to read {} \u{2014} {}",
                path.display(),
                err
            );
            return None;
        }
    };

    let mut mime = guess_mime(path, Some(&raw));
    if !options.accepted_mimes.contains(&mime.as_str()) {
        match transcode_to_png(&raw) {
            Some(transcoded) => {
                let file_name = path
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                tracing::info!(
                    "image_routing: transcoded {} ({}) -> image/png for provider compatibility",
                    file_name,
                    mime
                );
                raw = transcoded;
                mime = "image/png".to_string();
            }
            None => {
                tracing::warn!(
                    "image_routing: {} is {} which is not accepted by the active provider and could not be transcoded to PNG; skipping this attachment.",
                    path.display(),
                    mime
                );
                return None;
            }
        }
    }

    let b64 = BASE64_STANDARD.encode(&raw);
    Some(format!("data:{};base64,{}", mime, b64))
}

/// Build an OpenAI-style `content` list for a user turn.
///
/// Returns `(content_parts, skipped)`.
///
/// If one or more images are successfully attached, a single leading `text` part is
/// created combining the user caption (defaulting to "What do you see in this image?")
/// with tracking hints (`[Image attached at: <path>]` and `[Image attached: <url>]`).
/// Each local image is embedded as a `data:` URL, while remote URLs pass through verbatim.
/// Skipped entries reflect local paths that do not exist, fail read safety checks, or
/// could not be read/transcoded.
pub fn build_native_content_parts(
    user_text: &str,
    image_paths: &[String],
    image_urls: &[String],
    options: &NativeImageOptions<'_>,
) -> (Vec<serde_json::Value>, Vec<String>) {
    let mut skipped: Vec<String> = Vec::new();
    let mut image_parts: Vec<serde_json::Value> = Vec::new();
    let mut attached_paths: Vec<String> = Vec::new();
    let mut attached_urls: Vec<String> = Vec::new();

    for raw_path in image_paths {
        let p = Path::new(raw_path);
        if !p.is_file() {
            skipped.push(raw_path.clone());
            continue;
        }
        match file_to_data_url(p, options) {
            Some(data_url) => {
                image_parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": {
                        "url": data_url
                    }
                }));
                attached_paths.push(raw_path.clone());
            }
            None => {
                skipped.push(raw_path.clone());
            }
        }
    }

    for raw_url in image_urls {
        let url =
            raw_url.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c));
        if url.is_empty() {
            continue;
        }
        image_parts.push(serde_json::json!({
            "type": "image_url",
            "image_url": {
                "url": url
            }
        }));
        attached_urls.push(url.to_string());
    }

    let text =
        user_text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c));

    if !attached_paths.is_empty() || !attached_urls.is_empty() {
        let base_text = if text.is_empty() {
            "What do you see in this image?"
        } else {
            text
        };
        let mut hint_lines: Vec<String> = Vec::new();
        for p in &attached_paths {
            hint_lines.push(format!("[Image attached at: {p}]"));
        }
        for u in &attached_urls {
            hint_lines.push(format!("[Image attached: {u}]"));
        }
        let combined_text = format!("{base_text}\n\n{}", hint_lines.join("\n"));
        let mut parts = vec![serde_json::json!({
            "type": "text",
            "text": combined_text
        })];
        parts.extend(image_parts);
        (parts, skipped)
    } else {
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(serde_json::json!({
                "type": "text",
                "text": text
            }));
        }
        (parts, skipped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_test_dir(sub: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let unique = format!(
            "hermes_test_native_img_{}_{}_{}",
            sub,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        p.push(unique);
        fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    fn test_policy(dir: &Path) -> FileReadPolicy {
        FileReadPolicy {
            home: dir.to_path_buf(),
            cwd: dir.to_path_buf(),
            hermes_home: dir.join(".hermes"),
            hermes_root: dir.to_path_buf(),
        }
    }

    // Helper to generate a minimal valid 1x1 red PNG.
    fn sample_png_bytes() -> Vec<u8> {
        let mut img = image::RgbaImage::new(1, 1);
        img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        let dyn_img = DynamicImage::ImageRgba8(img);
        let mut cursor = Cursor::new(Vec::new());
        dyn_img.write_to(&mut cursor, ImageFormat::Png).unwrap();
        cursor.into_inner()
    }

    #[test]
    fn test_sniff_mime_signatures_and_order() {
        assert_eq!(sniff_mime_from_bytes(b""), None);

        // PNG
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        assert_eq!(sniff_mime_from_bytes(png), Some("image/png"));

        // JPEG
        let jpeg = b"\xff\xd8\xff\xe0\x00\x10JFIF";
        assert_eq!(sniff_mime_from_bytes(jpeg), Some("image/jpeg"));

        // GIF87a and GIF89a
        assert_eq!(sniff_mime_from_bytes(b"GIF87a\x01\x00"), Some("image/gif"));
        assert_eq!(sniff_mime_from_bytes(b"GIF89a\x01\x00"), Some("image/gif"));

        // WebP
        let webp = b"RIFF\x20\x00\x00\x00WEBPVP8 ";
        assert_eq!(sniff_mime_from_bytes(webp), Some("image/webp"));

        // BMP
        assert_eq!(sniff_mime_from_bytes(b"BM\x36\x00"), Some("image/bmp"));

        // ISO-BMFF AVIF
        let avif = b"\x00\x00\x00\x1cftypavif\x00\x00\x00\x00";
        assert_eq!(sniff_mime_from_bytes(avif), Some("image/avif"));
        let avis = b"\x00\x00\x00\x1cftypavis\x00\x00\x00\x00";
        assert_eq!(sniff_mime_from_bytes(avis), Some("image/avif"));

        // ISO-BMFF HEIC brands
        for brand in &[
            b"heic", b"heix", b"hevc", b"hevx", b"mif1", b"msf1", b"heim", b"heis",
        ] {
            let mut heic = b"\x00\x00\x00\x1cftyp".to_vec();
            heic.extend_from_slice(*brand);
            assert_eq!(sniff_mime_from_bytes(&heic), Some("image/heic"));
        }

        // TIFF (little-endian and big-endian)
        assert_eq!(
            sniff_mime_from_bytes(b"II*\x00\x08\x00"),
            Some("image/tiff")
        );
        assert_eq!(
            sniff_mime_from_bytes(b"MM\x00*\x00\x08"),
            Some("image/tiff")
        );

        // ICO
        assert_eq!(
            sniff_mime_from_bytes(b"\x00\x00\x01\x00\x01\x00"),
            Some("image/x-icon")
        );

        // SVG variations: XML prolog, bare <svg, leading ASCII whitespace with \t\r\n\v\f, case insensitivity
        assert_eq!(
            sniff_mime_from_bytes(b"<?xml version=\"1.0\"?><svg width=\"10\"></svg>"),
            Some("image/svg+xml")
        );
        assert_eq!(
            sniff_mime_from_bytes(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
            Some("image/svg+xml")
        );
        assert_eq!(
            sniff_mime_from_bytes(b" \t\r\n\x0b\x0c<SVG height=\"10\"></SVG>"),
            Some("image/svg+xml")
        );

        // Non-image bytes
        assert_eq!(sniff_mime_from_bytes(b"Plain text note"), None);
        assert_eq!(
            sniff_mime_from_bytes(b"<html><body>test</body></html>"),
            None
        );
    }

    #[test]
    fn test_heic_avif_decoder_gap_documented() {
        // HEIC/AVIF are sniffed correctly, but transcoding fails gracefully
        // because image 0.25 does not bundle HEIF/AVIF decoders by default.
        let avif = b"\x00\x00\x00\x1cftypavif\x00\x00\x00\x00some_bytes";
        assert_eq!(sniff_mime_from_bytes(avif), Some("image/avif"));
        assert_eq!(transcode_to_png(avif), None);

        let heic = b"\x00\x00\x00\x1cftypheic\x00\x00\x00\x00some_bytes";
        assert_eq!(sniff_mime_from_bytes(heic), Some("image/heic"));
        assert_eq!(transcode_to_png(heic), None);
    }

    #[test]
    fn test_guess_mime_fallback_and_magic_precedence() {
        let path_jpg = Path::new("test.jpg");
        let png_bytes = sample_png_bytes();
        // Magic bytes win over misleading file extension
        assert_eq!(guess_mime(path_jpg, Some(&png_bytes)), "image/png");

        // Filename based fallbacks when raw is None
        assert_eq!(guess_mime(Path::new("pic.png"), None), "image/png");
        assert_eq!(guess_mime(Path::new("pic.jpg"), None), "image/jpeg");
        assert_eq!(guess_mime(Path::new("pic.jpeg"), None), "image/jpeg");
        assert_eq!(guess_mime(Path::new("pic.gif"), None), "image/gif");
        assert_eq!(guess_mime(Path::new("pic.webp"), None), "image/webp");
        assert_eq!(guess_mime(Path::new("pic.bmp"), None), "image/bmp");
        assert_eq!(guess_mime(Path::new("pic.unknown"), None), "image/jpeg");
    }

    #[test]
    fn test_bmp_transcode_pixel_preservation() {
        // Create 2x2 BMP with distinct RGB pixels
        let mut img = image::RgbImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgb([255, 0, 0])); // Red
        img.put_pixel(1, 0, image::Rgb([0, 255, 0])); // Green
        img.put_pixel(0, 1, image::Rgb([0, 0, 255])); // Blue
        img.put_pixel(1, 1, image::Rgb([255, 255, 0])); // Yellow

        let dyn_img = DynamicImage::ImageRgb8(img);
        let mut bmp_buf = Cursor::new(Vec::new());
        dyn_img
            .write_to(&mut bmp_buf, ImageFormat::Bmp)
            .expect("encode bmp");
        let bmp_bytes = bmp_buf.into_inner();

        // Transcode BMP -> PNG
        let png_bytes = transcode_to_png(&bmp_bytes).expect("transcode bmp to png");

        // Verify dimensions and exact pixel colors preserved
        let reloaded = image::load_from_memory(&png_bytes).expect("load transcoded png");
        assert_eq!(reloaded.width(), 2);
        assert_eq!(reloaded.height(), 2);

        let rgb = reloaded.to_rgb8();
        assert_eq!(rgb.get_pixel(0, 0), &image::Rgb([255, 0, 0]));
        assert_eq!(rgb.get_pixel(1, 0), &image::Rgb([0, 255, 0]));
        assert_eq!(rgb.get_pixel(0, 1), &image::Rgb([0, 0, 255]));
        assert_eq!(rgb.get_pixel(1, 1), &image::Rgb([255, 255, 0]));
    }

    #[test]
    fn test_tiff_transcode_pixel_and_alpha_preservation() {
        // Create 2x2 TIFF with distinct RGBA pixels including non-trivial alpha
        let mut img = image::RgbaImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgba([200, 10, 20, 128])); // semi-transparent
        img.put_pixel(1, 0, image::Rgba([10, 200, 30, 255])); // opaque
        img.put_pixel(0, 1, image::Rgba([30, 40, 200, 64])); // translucent
        img.put_pixel(1, 1, image::Rgba([0, 0, 0, 0])); // fully transparent

        let dyn_img = DynamicImage::ImageRgba8(img);
        let mut tiff_buf = Cursor::new(Vec::new());
        dyn_img
            .write_to(&mut tiff_buf, ImageFormat::Tiff)
            .expect("encode tiff");
        let tiff_bytes = tiff_buf.into_inner();

        // Transcode TIFF -> PNG
        let png_bytes = transcode_to_png(&tiff_bytes).expect("transcode tiff to png");

        // Verify dimensions, colors, and alpha channel preservation
        let reloaded = image::load_from_memory(&png_bytes).expect("load transcoded png");
        assert_eq!(reloaded.width(), 2);
        assert_eq!(reloaded.height(), 2);

        let rgba = reloaded.to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0), &image::Rgba([200, 10, 20, 128]));
        assert_eq!(rgba.get_pixel(1, 0), &image::Rgba([10, 200, 30, 255]));
        assert_eq!(rgba.get_pixel(0, 1), &image::Rgba([30, 40, 200, 64]));
        assert_eq!(rgba.get_pixel(1, 1), &image::Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn test_png_passes_through_unchanged_even_corrupt() {
        let temp_dir = temp_test_dir("pass_through");
        let policy = test_policy(&temp_dir);
        let options = NativeImageOptions {
            read_policy: &policy,
            accepted_mimes: UNIVERSALLY_SUPPORTED_MIMES,
        };

        // Valid PNG passes through byte-identical
        let png_path = temp_dir.join("valid.png");
        let valid_bytes = sample_png_bytes();
        fs::write(&png_path, &valid_bytes).unwrap();
        let url = file_to_data_url(&png_path, &options).expect("data url");
        assert!(url.starts_with("data:image/png;base64,"));
        let b64 = url.strip_prefix("data:image/png;base64,").unwrap();
        assert_eq!(BASE64_STANDARD.decode(b64).unwrap(), valid_bytes);

        // Corrupt bytes with PNG header also pass through unchanged because PNG is accepted
        let corrupt_path = temp_dir.join("corrupt.png");
        let corrupt_bytes = b"\x89PNG\r\n\x1a\ncorrupt_truncated_bytes";
        fs::write(&corrupt_path, corrupt_bytes).unwrap();
        let corrupt_url = file_to_data_url(&corrupt_path, &options).expect("corrupt png url");
        let b64_corrupt = corrupt_url.strip_prefix("data:image/png;base64,").unwrap();
        assert_eq!(BASE64_STANDARD.decode(b64_corrupt).unwrap(), corrupt_bytes);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_unsupported_corrupt_skipped() {
        let temp_dir = temp_test_dir("corrupt_skip");
        let policy = test_policy(&temp_dir);
        let options = NativeImageOptions {
            read_policy: &policy,
            accepted_mimes: UNIVERSALLY_SUPPORTED_MIMES,
        };

        // Corrupted BMP cannot be transcoded to PNG and is skipped (returns None)
        let corrupt_bmp = temp_dir.join("corrupt.bmp");
        fs::write(&corrupt_bmp, b"BMnot_a_valid_bmp_content").unwrap();
        assert_eq!(file_to_data_url(&corrupt_bmp, &options), None);

        // SVG cannot be rasterized to PNG and is skipped
        let svg_path = temp_dir.join("diagram.svg");
        fs::write(&svg_path, b"<svg><rect width=\"100\"/></svg>").unwrap();
        assert_eq!(file_to_data_url(&svg_path, &options), None);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_managed_runtime_narrower_accepted_mimes() {
        let temp_dir = temp_test_dir("managed_narrow");
        let policy = test_policy(&temp_dir);

        // Managed runtime accepted set without webp
        let narrower_mimes: &[&str] = &["image/png", "image/jpeg"];
        let options = NativeImageOptions {
            read_policy: &policy,
            accepted_mimes: narrower_mimes,
        };

        // Create a 1x1 WebP
        let mut img = image::RgbImage::new(1, 1);
        img.put_pixel(0, 0, image::Rgb([12, 34, 56]));
        let dyn_img = DynamicImage::ImageRgb8(img);
        let mut webp_buf = Cursor::new(Vec::new());
        dyn_img
            .write_to(&mut webp_buf, ImageFormat::WebP)
            .expect("encode webp");

        let webp_path = temp_dir.join("managed.webp");
        fs::write(&webp_path, webp_buf.into_inner()).unwrap();

        // WebP is outside narrower accepted_mimes, so it transcodes to PNG
        let url = file_to_data_url(&webp_path, &options).expect("transcoded webp url");
        assert!(url.starts_with("data:image/png;base64,"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_build_native_content_parts_urls_and_paths() {
        let temp_dir = temp_test_dir("content_parts");
        let policy = test_policy(&temp_dir);
        let options = NativeImageOptions {
            read_policy: &policy,
            accepted_mimes: UNIVERSALLY_SUPPORTED_MIMES,
        };

        let local_png = temp_dir.join("test.png");
        fs::write(&local_png, sample_png_bytes()).unwrap();

        let remote_url = "https://example.com/remote.jpg".to_string();
        let (parts, skipped) = build_native_content_parts(
            "Check both images",
            &[local_png.to_string_lossy().to_string()],
            std::slice::from_ref(&remote_url),
            &options,
        );

        assert_eq!(skipped, Vec::<String>::new());
        assert_eq!(parts.len(), 3);

        // Text part with combined caption and hints
        assert_eq!(parts[0]["type"], "text");
        let text = parts[0]["text"].as_str().unwrap();
        assert!(text.starts_with("Check both images"));
        assert!(text.contains(&format!("[Image attached at: {}]", local_png.display())));
        assert!(text.contains(&format!("[Image attached: {remote_url}]")));

        // Local image_url part
        assert_eq!(parts[1]["type"], "image_url");
        assert!(parts[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));

        // Remote image_url part
        assert_eq!(parts[2]["type"], "image_url");
        assert_eq!(parts[2]["image_url"]["url"], remote_url);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_build_native_content_parts_defaults_and_skips() {
        let temp_dir = temp_test_dir("defaults_skips");
        let policy = test_policy(&temp_dir);
        let options = NativeImageOptions {
            read_policy: &policy,
            accepted_mimes: UNIVERSALLY_SUPPORTED_MIMES,
        };

        let local_png = temp_dir.join("photo.png");
        fs::write(&local_png, sample_png_bytes()).unwrap();
        let missing = temp_dir.join("missing.png");

        // Empty user_text should fall back to default question
        let (parts, skipped) = build_native_content_parts(
            "   ",
            &[
                local_png.to_string_lossy().to_string(),
                missing.to_string_lossy().to_string(),
            ],
            &[],
            &options,
        );

        assert_eq!(skipped, vec![missing.to_string_lossy().to_string()]);
        assert_eq!(parts.len(), 2);
        let text = parts[0]["text"].as_str().unwrap();
        assert!(text.starts_with("What do you see in this image?"));
        assert!(text.contains(&format!("[Image attached at: {}]", local_png.display())));

        // Pure text without attachments
        let (parts_empty, skipped_empty) =
            build_native_content_parts("Hello only", &[], &[], &options);
        assert_eq!(skipped_empty, Vec::<String>::new());
        assert_eq!(parts_empty.len(), 1);
        assert_eq!(parts_empty[0]["type"], "text");
        assert_eq!(parts_empty[0]["text"], "Hello only");

        let _ = fs::remove_dir_all(&temp_dir);
    }
}

#[cfg(test)]
mod golden_corpus {
    use super::*;
    use serde_json::{json, Value};
    use std::path::PathBuf;

    #[test]
    fn native_loading_matches_python_files_and_pixels() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../tools/native-image-goldens.json")).unwrap();
        for case in fixture["sniff"].as_array().unwrap() {
            let bytes: Vec<u8> = serde_json::from_value(case["bytes"].clone()).unwrap();
            assert_eq!(
                json!(sniff_mime_from_bytes(&bytes)),
                case["expected"],
                "{case}"
            );
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "hermes-native-corpus-{}-{stamp}",
            std::process::id()
        ));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        fs::create_dir_all(root.join("directory.png")).unwrap();
        for (name, content) in fixture["files"].as_object().unwrap() {
            fs::write(
                root.join(name),
                BASE64_STANDARD.decode(content.as_str().unwrap()).unwrap(),
            )
            .unwrap();
        }
        let policy = FileReadPolicy {
            home: root.clone(),
            cwd: root.clone(),
            hermes_home: root.join(".hermes/profile"),
            hermes_root: root.join(".hermes"),
        };
        for (index, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
            let paths: Vec<String> = case["paths"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| root.join(p.as_str().unwrap()).to_str().unwrap().to_owned())
                .collect();
            let urls: Vec<String> = serde_json::from_value(case["urls"].clone()).unwrap();
            let accepted = if case["managed"] == true {
                &["image/png", "image/jpeg", "image/gif"][..]
            } else {
                UNIVERSALLY_SUPPORTED_MIMES
            };
            let options = NativeImageOptions {
                read_policy: &policy,
                accepted_mimes: accepted,
            };
            let (mut parts, skipped) = build_native_content_parts(
                case["caption"].as_str().unwrap(),
                &paths,
                &urls,
                &options,
            );
            if case["pixels"] == true {
                for part in &mut parts {
                    if let Some(url) = part["image_url"]["url"]
                        .as_str()
                        .filter(|url| url.starts_with("data:"))
                    {
                        let (header, payload) = url.split_once(',').unwrap();
                        let bytes = BASE64_STANDARD.decode(payload).unwrap();
                        let picture = image::load_from_memory(&bytes).unwrap();
                        part["image_url"] = json!({"mime": header[5..].split(';').next().unwrap(), "size": [picture.width(), picture.height()], "rgba": picture.to_rgba8().as_raw()});
                    }
                }
            }
            let actual = json!({"parts": parts, "skipped": skipped})
                .to_string()
                .replace(&format!("{}/", root.display()), "");
            assert_eq!(
                serde_json::from_str::<Value>(&actual).unwrap(),
                case["expected"],
                "case {index}"
            );
        }
    }
}
