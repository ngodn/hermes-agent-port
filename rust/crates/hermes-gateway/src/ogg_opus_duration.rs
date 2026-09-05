//! Ogg Opus header-only duration probing, matching the gateway's Mutagen path.
//! Audio payloads are not decoded. Page positions count samples at 48 kHz,
//! independently of the input sample-rate field in OpusHead (RFC 7845).
use std::{fs::File, io::Read, path::Path};

struct Page {
    flags: u8,
    position: i64,
    serial: u32,
    sequence: u32,
    packets: Vec<Vec<u8>>,
    complete: bool,
}

fn page(input: &mut impl Read) -> Option<Page> {
    let mut header = [0_u8; 27];
    input.read_exact(&mut header).ok()?;
    if &header[..4] != b"OggS" || header[4] != 0 {
        return None;
    }
    let mut lacing = vec![0; header[26] as usize];
    input.read_exact(&mut lacing).ok()?;
    let mut packets = Vec::new();
    let mut packet = Vec::new();
    for length in &lacing {
        let start = packet.len();
        packet.resize(start + *length as usize, 0);
        input.read_exact(&mut packet[start..]).ok()?;
        if *length < 255 {
            packets.push(std::mem::take(&mut packet));
        }
    }
    let complete = packet.is_empty();
    if !packet.is_empty() {
        packets.push(packet);
    }
    Some(Page {
        flags: header[5],
        position: i64::from_le_bytes(header[6..14].try_into().ok()?),
        serial: u32::from_le_bytes(header[14..18].try_into().ok()?),
        sequence: u32::from_le_bytes(header[18..22].try_into().ok()?),
        packets,
        complete,
    })
}

fn valid_comments(data: &[u8]) -> bool {
    fn take_length(data: &mut &[u8]) -> Option<usize> {
        let length = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
        *data = &data[4..];
        Some(length)
    }
    let mut data = data;
    let Some(vendor) = take_length(&mut data) else {
        return false;
    };
    let Some(rest) = data.get(vendor..) else {
        return false;
    };
    data = rest;
    let Some(count) = take_length(&mut data) else {
        return false;
    };
    // Each entry requires at least its four-byte length prefix.
    if count > data.len() / 4 {
        return false;
    }
    for _ in 0..count {
        let Some(length) = take_length(&mut data) else {
            return false;
        };
        let Some(rest) = data.get(length..) else {
            return false;
        };
        data = rest;
    }
    true
}

pub fn seconds(path: &Path) -> Option<f64> {
    let mut input = File::open(path).ok()?;
    let head = loop {
        let next = page(&mut input)?;
        if next.packets.first()?.starts_with(b"OpusHead") {
            break next;
        }
    };
    let packet = head.packets.first()?;
    if head.flags & 2 == 0 || packet.len() < 19 || packet[8] >> 4 != 0 {
        return None;
    }
    let skip = u16::from_le_bytes(packet[10..12].try_into().ok()?);
    let serial = head.serial;
    let tags = loop {
        let next = page(&mut input)?;
        if next.serial == serial && next.packets.first()?.starts_with(b"OpusTags") {
            break next;
        }
    };
    let mut comment = tags.packets.first()?.get(8..)?.to_vec();
    let mut complete = tags.complete || tags.packets.len() > 1;
    let mut sequence = tags.sequence;
    let mut last = (tags.serial, tags.position, tags.flags);
    let mut best = if tags.position != -1 {
        Some(tags.position)
    } else if head.position != -1 {
        Some(head.position)
    } else {
        None
    };
    let mut first_end = if tags.flags & 4 != 0 { best } else { None };
    while let Some(next) = page(&mut input) {
        last = (next.serial, next.position, next.flags);
        if next.serial != serial {
            continue;
        }
        if !complete {
            if next.sequence != sequence.wrapping_add(1) || next.flags & 1 == 0 {
                return None;
            }
            comment.extend_from_slice(next.packets.first()?);
            complete = next.complete || next.packets.len() > 1;
            sequence = next.sequence;
        }
        if next.position != -1 {
            best = Some(next.position);
            if first_end.is_none() && next.flags & 4 != 0 {
                first_end = best;
            }
        }
    }
    if !complete || !valid_comments(&comment) {
        return None;
    }
    let position = if last.0 == serial && last.1 != -1 && last.2 & 4 != 0 {
        last.1
    } else {
        first_end.or(best)?
    };
    Some(((position as i128 - skip as i128) as f64) / 48000.0)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn gateway_probe_uses_opus_duration_for_all_voice_extensions() {
        use base64::Engine;
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/ogg-duration-goldens.json")).unwrap();
        let case = cases
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == "position-312-2928312")
            .unwrap();
        let data = base64::engine::general_purpose::STANDARD
            .decode(case["ogg"].as_str().unwrap())
            .unwrap();
        for extension in ["ogg", "opus", "OGA"] {
            let path = std::env::temp_dir().join(format!(
                "hermes-voice-{}-{}.{extension}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::write(&path, &data).unwrap();
            assert_eq!(
                crate::audio_process::probe_duration(path.to_str().unwrap())
                    .await
                    .as_deref(),
                Some("1:01")
            );
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn duration_matches_mutagen_files() {
        use base64::Engine;
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/ogg-duration-goldens.json")).unwrap();
        let path = std::env::temp_dir().join(format!(
            "hermes-opus-{}-{}.ogg",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for case in cases.as_array().unwrap() {
            std::fs::write(
                &path,
                base64::engine::general_purpose::STANDARD
                    .decode(case["ogg"].as_str().unwrap())
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                super::seconds(&path).map(f64::to_bits),
                case["bits"].as_u64(),
                "{}",
                case["name"]
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}
