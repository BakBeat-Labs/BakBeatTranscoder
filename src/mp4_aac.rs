// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ISO-BMFF / MP4 helpers for AAC-LC audiobook artifacts.
//!
//! Used to read encoder priming (`elst` media_time), AAC profile (`esds`),
//! chapter boxes, and to strip native FFmpeg AAC encoder delay from ADTS.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{anyhow, Result};

/// Facts BakBeat needs to decide copyExisting vs create for Shuffle audiobooks.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AudiobookFacts {
    pub codec: String,
    /// `LC`, `HE-AAC`, `HE-AACv2`, or unknown (`null`).
    pub profile: Option<String>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u8>,
    pub bitrate_kbps: Option<u32>,
    /// Encoder delay in PCM samples. `null` if the container cannot be parsed
    /// for an edit list (do not treat as Shuffle-ready).
    pub priming_samples: Option<u64>,
    pub has_chapters: Option<bool>,
    pub has_artwork: Option<bool>,
    pub container: String,
}

pub fn is_protected_audiobook_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("m4p" | "aax" | "aaxc")
    )
}

pub fn is_iso_bmff_audio_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "m4a" | "m4b" | "mp4" | "m4v" | "mov" | "ipod"
    )
}

/// Drop the first complete ADTS AAC frame (native encoder delay = 1024 samples).
pub fn drop_first_adts_frame(input: &[u8]) -> Result<Vec<u8>> {
    let start = find_adts_sync(input).ok_or_else(|| anyhow!("ADTS stream has no sync word"))?;
    let Some((frame_len, _)) = adts_frame_at(input, start) else {
        return Err(anyhow!("ADTS stream has no complete first frame to drop"));
    };
    let next = start + frame_len;
    if next >= input.len() {
        return Err(anyhow!(
            "ADTS stream has only one frame; cannot drop encoder delay"
        ));
    }
    let rest = &input[next..];
    if find_adts_sync(rest)
        .and_then(|off| adts_frame_at(rest, off))
        .is_none()
    {
        return Err(anyhow!(
            "ADTS stream has no frames after dropping encoder delay"
        ));
    }
    Ok(rest.to_vec())
}

fn find_adts_sync(data: &[u8]) -> Option<usize> {
    data.windows(2)
        .position(|w| w[0] == 0xFF && w[1] & 0xF0 == 0xF0)
}

pub fn drop_first_adts_frame_file(src: &Path, dst: &Path) -> Result<()> {
    let input = fs::read(src)?;
    let stripped = drop_first_adts_frame(&input)?;
    fs::write(dst, stripped)?;
    Ok(())
}

fn adts_frame_at(data: &[u8], offset: usize) -> Option<(usize, bool)> {
    if offset + 7 > data.len() {
        return None;
    }
    let h = &data[offset..];
    if h[0] != 0xFF || h[1] & 0xF0 != 0xF0 {
        return None;
    }
    let protection_absent = h[1] & 0x01 != 0;
    let len = ((h[3] as usize & 0x03) << 11) | ((h[4] as usize) << 3) | ((h[5] as usize) >> 5);
    if len < 7 || offset + len > data.len() {
        return None;
    }
    Some((len, protection_absent))
}

#[derive(Debug, Clone, Default)]
pub struct Mp4AacInfo {
    pub priming_samples: Option<u64>,
    pub profile: Option<String>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u8>,
    pub has_chapters: bool,
    pub has_artwork: bool,
    pub encrypted: bool,
    pub parsed: bool,
    pub alac: Option<AlacInfo>,
}

#[derive(Debug, Clone, Copy)]
pub struct AlacInfo {
    pub sample_rate_hz: Option<u32>,
    pub bits_per_sample: Option<u32>,
    pub channels: Option<u8>,
}

/// Read `moov` (and `moof` if present) without loading `mdat`.
pub fn inspect_mp4(path: &Path, track_index: u32) -> Result<Mp4AacInfo> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut info = Mp4AacInfo::default();
    let mut pos = 0u64;
    while pos + 8 <= file_len {
        file.seek(SeekFrom::Start(pos))?;
        let mut hdr = [0u8; 8];
        file.read_exact(&mut hdr)?;
        let size32 = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
        let mut typ = [0u8; 4];
        typ.copy_from_slice(&hdr[4..8]);
        let (header_len, total) = if size32 == 1 {
            let mut wide = [0u8; 8];
            file.read_exact(&mut wide)?;
            (16u64, u64::from_be_bytes(wide))
        } else if size32 == 0 {
            (8u64, file_len.saturating_sub(pos))
        } else {
            (8u64, u64::from(size32))
        };
        if total < header_len || pos.saturating_add(total) > file_len {
            break;
        }
        if &typ == b"moov" || &typ == b"moof" {
            let payload_len = (total - header_len) as usize;
            let mut payload = vec![0u8; payload_len];
            file.read_exact(&mut payload)?;
            walk_boxes(&payload, &mut info, false, track_index);
            info.parsed = true;
            if &typ == b"moov" {
                break;
            }
        }
        pos = pos.saturating_add(total);
    }
    Ok(info)
}

#[allow(dead_code)]
pub fn inspect_mp4_bytes(data: &[u8]) -> Mp4AacInfo {
    let mut info = Mp4AacInfo::default();
    if data.len() < 8 {
        return info;
    }
    walk_boxes(data, &mut info, false, 0);
    info.parsed = true;
    info
}

fn walk_boxes(data: &[u8], info: &mut Mp4AacInfo, in_sound_track: bool, track_index: u32) {
    let mut offset = 0usize;
    let mut track = 0;
    while let Some((typ, payload, next)) = next_box(data, offset) {
        offset = next;
        match &typ {
            b"moov" | b"edts" | b"mdia" | b"minf" | b"stbl" | b"udta" | b"moof" | b"meta"
            | b"ilst" => {
                walk_boxes(payload, info, in_sound_track, track_index);
            }
            b"trak" => {
                let handler = find_handler(payload);
                let sound = handler.as_ref() == Some(b"soun");
                if matches!(handler.as_ref(), Some(b"text" | b"sbtl")) {
                    info.has_chapters = true;
                }
                walk_boxes(payload, info, sound && track == track_index, track_index);
                track += 1;
            }
            b"elst" if in_sound_track => {
                if let Some(priming) = parse_elst_priming(payload) {
                    info.priming_samples = Some(priming);
                }
            }
            b"stsd" => walk_stsd(payload, info, in_sound_track),
            b"esds" if in_sound_track => {
                if let Some(profile) = profile_from_esds(payload) {
                    info.profile = Some(profile);
                }
            }
            b"chpl" | b"chap" => info.has_chapters = true,
            b"covr" => info.has_artwork = true,
            b"enca" | b"encv" | b"pssh" | b"sinf" => info.encrypted = true,
            _ => {}
        }
    }
}

fn find_handler(data: &[u8]) -> Option<[u8; 4]> {
    let mut offset = 0usize;
    while let Some((typ, payload, next)) = next_box(data, offset) {
        offset = next;
        if &typ == b"hdlr" && payload.len() >= 12 {
            let mut h = [0u8; 4];
            h.copy_from_slice(&payload[8..12]);
            return Some(h);
        }
        if matches!(&typ, b"mdia" | b"minf" | b"edts") {
            if let Some(h) = find_handler(payload) {
                return Some(h);
            }
        }
    }
    None
}

fn walk_stsd(payload: &[u8], info: &mut Mp4AacInfo, in_sound_track: bool) {
    // FullBox version/flags (4) + entry_count (4)
    if payload.len() < 8 {
        return;
    }
    let entries = &payload[8..];
    let mut offset = 0usize;
    while let Some((typ, sample_payload, next)) = next_box(entries, offset) {
        offset = next;
        if &typ == b"enca" || &typ == b"encv" {
            info.encrypted = true;
        }
        if in_sound_track && (&typ == b"mp4a" || &typ == b"enca") {
            if sample_payload.len() >= 28 {
                info.channels = Some(u16::from_be_bytes(
                    sample_payload[16..18].try_into().unwrap_or([0, 2]),
                ) as u8);
                let sr =
                    u32::from_be_bytes(sample_payload[24..28].try_into().unwrap_or([0, 0, 0, 0]))
                        >> 16;
                if sr > 0 {
                    info.sample_rate_hz = Some(sr);
                }
                walk_boxes(&sample_payload[28..], info, true, 0);
            }
        }
        if in_sound_track && &typ == b"alac" && info.alac.is_none() {
            info.alac = alac_sample_entry(sample_payload);
        }
        if &typ == b"mp4v" || &typ == b"avc1" || &typ == b"encv" {
            info.has_artwork = true;
        }
    }
}

fn alac_sample_entry(payload: &[u8]) -> Option<AlacInfo> {
    let version = u16::from_be_bytes(payload.get(8..10)?.try_into().ok()?);
    let (child_offset, sample_rate, bits, channels) = match version {
        0 | 1 => (
            if version == 0 { 28 } else { 44 },
            u32::from_be_bytes(payload.get(24..28)?.try_into().ok()?) >> 16,
            u32::from(u16::from_be_bytes(payload.get(18..20)?.try_into().ok()?)),
            u32::from(u16::from_be_bytes(payload.get(16..18)?.try_into().ok()?)),
        ),
        2 => (
            64,
            f64::from_be_bytes(payload.get(32..40)?.try_into().ok()?) as u32,
            u32::from_be_bytes(payload.get(48..52)?.try_into().ok()?),
            u32::from_be_bytes(payload.get(40..44)?.try_into().ok()?),
        ),
        _ => return None,
    };
    Some(
        payload
            .get(child_offset..)
            .and_then(|children| find_alac_config(children, true))
            .unwrap_or(AlacInfo {
                sample_rate_hz: (sample_rate > 0).then_some(sample_rate),
                bits_per_sample: (bits > 0).then_some(bits),
                channels: u8::try_from(channels).ok().filter(|&n| n > 0),
            }),
    )
}

fn find_alac_config(children: &[u8], allow_wave: bool) -> Option<AlacInfo> {
    let mut offset = 0;
    while let Some((typ, payload, next)) = next_box(children, offset) {
        offset = next;
        let config = match &typ {
            b"alac" => parse_alac_config(payload),
            b"wave" if allow_wave => find_alac_config(payload, false),
            _ => None,
        };
        if config.is_some() {
            return config;
        }
    }
    None
}

fn parse_alac_config(payload: &[u8]) -> Option<AlacInfo> {
    // ALAC FullBox header followed by the 24-byte ALACSpecificConfig.
    let config = payload.get(4..28)?;
    let frame_length = u32::from_be_bytes(config[0..4].try_into().ok()?);
    let bits = config[5];
    let channels = config[9];
    let rate = u32::from_be_bytes(config[20..24].try_into().ok()?);
    if frame_length == 0
        || config[4] != 0
        || !matches!(bits, 16 | 20 | 24 | 32)
        || !(1..=8).contains(&channels)
        || !(1..=768_000).contains(&rate)
    {
        return None;
    }
    Some(AlacInfo {
        sample_rate_hz: Some(rate),
        bits_per_sample: Some(u32::from(bits)),
        channels: Some(channels),
    })
}

/// First non-empty edit's `media_time` is encoder priming in media timescale
/// (audio sample rate). Empty edits (`media_time == -1`) are skipped.
fn parse_elst_priming(payload: &[u8]) -> Option<u64> {
    if payload.len() < 8 {
        return None;
    }
    let version = payload[0];
    let entry_count = u32::from_be_bytes(payload[4..8].try_into().ok()?);
    let mut pos = 8usize;
    for _ in 0..entry_count {
        if version == 1 {
            if pos + 20 > payload.len() {
                return None;
            }
            let media_time = i64::from_be_bytes(payload[pos + 8..pos + 16].try_into().ok()?);
            pos += 20;
            if media_time >= 0 {
                return Some(media_time as u64);
            }
        } else {
            if pos + 12 > payload.len() {
                return None;
            }
            let media_time = i32::from_be_bytes(payload[pos + 4..pos + 8].try_into().ok()?);
            pos += 12;
            if media_time >= 0 {
                return Some(media_time as u64);
            }
        }
    }
    Some(0)
}

fn profile_from_esds(payload: &[u8]) -> Option<String> {
    // Skip FullBox header if present.
    let body = if payload.len() > 4 {
        &payload[4..]
    } else {
        payload
    };
    let asc = find_decoder_specific_info(body)?;
    if asc.is_empty() {
        return None;
    }
    let mut object_type = asc[0] >> 3;
    if object_type == 31 && asc.len() >= 2 {
        object_type = 32 + (asc[1] >> 3);
    }
    Some(match object_type {
        2 => "LC".to_string(),
        5 => "HE-AAC".to_string(),
        29 => "HE-AACv2".to_string(),
        1 => "MAIN".to_string(),
        4 => "LTP".to_string(),
        other => format!("AOT-{other}"),
    })
}

fn find_decoder_specific_info(data: &[u8]) -> Option<&[u8]> {
    let mut i = 0usize;
    while i + 2 < data.len() {
        let tag = data[i];
        let (len, hdr) = mpeg4_descriptor_length(&data[i + 1..])?;
        let start = i + 1 + hdr;
        if start > data.len() {
            return None;
        }
        let end = (start + len).min(data.len());
        let body = &data[start..end];
        if tag == 0x05 {
            return Some(body);
        }
        if tag == 0x03 {
            let nested = skip_es_descriptor_header(body)?;
            if let Some(found) = find_decoder_specific_info(nested) {
                return Some(found);
            }
        } else if tag == 0x04 {
            let nested = body.get(13..)?;
            if let Some(found) = find_decoder_specific_info(nested) {
                return Some(found);
            }
        }
        i = end;
    }
    None
}

fn skip_es_descriptor_header(body: &[u8]) -> Option<&[u8]> {
    if body.len() < 3 {
        return None;
    }
    let flags = body[2];
    let mut skip = 3usize;
    if flags & 0x80 != 0 {
        skip += 2;
    }
    if flags & 0x40 != 0 {
        let url_len = *body.get(skip)? as usize;
        skip += 1 + url_len;
    }
    if flags & 0x20 != 0 {
        skip += 2;
    }
    body.get(skip..)
}

fn mpeg4_descriptor_length(data: &[u8]) -> Option<(usize, usize)> {
    if data.is_empty() {
        return None;
    }
    let mut len = 0usize;
    let mut n = 0usize;
    loop {
        if n >= data.len() || n >= 4 {
            return None;
        }
        let b = data[n];
        n += 1;
        len = (len << 7) | (b & 0x7F) as usize;
        if b & 0x80 == 0 {
            return Some((len, n));
        }
    }
}

fn next_box(data: &[u8], offset: usize) -> Option<([u8; 4], &[u8], usize)> {
    if offset + 8 > data.len() {
        return None;
    }
    let size32 = u32::from_be_bytes(data[offset..offset + 4].try_into().ok()?);
    let mut typ = [0u8; 4];
    typ.copy_from_slice(&data[offset + 4..offset + 8]);
    let (header, total) = if size32 == 1 {
        if offset + 16 > data.len() {
            return None;
        }
        let size64 = u64::from_be_bytes(data[offset + 8..offset + 16].try_into().ok()?);
        (16usize, usize::try_from(size64).ok()?)
    } else if size32 == 0 {
        (8usize, data.len() - offset)
    } else {
        (8usize, size32 as usize)
    };
    if total < header || total > data.len() - offset {
        return None;
    }
    let payload = &data[offset + header..offset + total];
    Some((typ, payload, offset + total))
}

/// Shuffle audiobook target: AAC in an ipod/m4b mux with 0 encoder priming.
pub fn is_zero_priming_aac_m4b_target(
    media_is_audio: bool,
    audio_codec: &str,
    container: &str,
    extension: &str,
    aac_priming: Option<u32>,
) -> bool {
    if !media_is_audio || !audio_codec.eq_ignore_ascii_case("aac") {
        return false;
    }
    if audio_codec.eq_ignore_ascii_case("aac_at") {
        return false;
    }
    if aac_priming == Some(0) {
        return true;
    }
    let ext = extension.to_ascii_lowercase();
    let cont = container.to_ascii_lowercase();
    // ipod muxer is also used for video; only `.m4b` / explicit priming 0
    // selects the Shuffle audiobook path.
    ext == "m4b" || cont == "m4b"
}

pub fn containers_match_audiobook(
    source_container: &str,
    spec_container: &str,
    spec_ext: &str,
) -> bool {
    if source_container.eq_ignore_ascii_case(spec_container) {
        return true;
    }
    if source_container.eq_ignore_ascii_case(spec_ext) {
        return true;
    }
    let src = source_container.to_ascii_lowercase();
    let cont = spec_container.to_ascii_lowercase();
    let ext = spec_ext.to_ascii_lowercase();
    matches!(
        (src.as_str(), cont.as_str(), ext.as_str()),
        ("m4b", "ipod", "m4b") | ("m4b", "m4b", _) | ("ipod", "ipod", _) | ("ipod", "m4b", _)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alac_cookie_overrides_sample_entry_directly_and_in_wave() {
        fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut atom = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
            atom.extend(kind);
            atom.extend(payload);
            atom
        }

        let mut entry = [0; 28];
        entry[16..18].copy_from_slice(&2u16.to_be_bytes());
        entry[18..20].copy_from_slice(&16u16.to_be_bytes());
        entry[24..28].copy_from_slice(&(44_100u32 << 16).to_be_bytes());
        let mut cookie = [0; 28];
        cookie[4..8].copy_from_slice(&4096u32.to_be_bytes());
        cookie[9] = 24;
        cookie[13] = 1;
        cookie[24..28].copy_from_slice(&192_000u32.to_be_bytes());

        let direct = atom(b"alac", &cookie);
        for child in [direct.clone(), atom(b"wave", &direct)] {
            let info = alac_sample_entry(&[entry.as_slice(), &child].concat()).unwrap();
            assert_eq!(info.sample_rate_hz, Some(192_000));
            assert_eq!(info.bits_per_sample, Some(24));
            assert_eq!(info.channels, Some(1));
        }

        cookie[13] = 0; // An invalid channel count makes the cookie unusable.
        for child in [Vec::new(), atom(b"alac", &cookie)] {
            let info = alac_sample_entry(&[entry.as_slice(), &child].concat()).unwrap();
            assert_eq!(info.sample_rate_hz, Some(44_100));
            assert_eq!(info.bits_per_sample, Some(16));
            assert_eq!(info.channels, Some(2));
        }
    }

    fn adts_silence_two_frames() -> Vec<u8> {
        // Minimal syntactically valid ADTS frames (payload need not decode).
        fn frame(payload_len: usize) -> Vec<u8> {
            let total = 7 + payload_len;
            let mut f = vec![0u8; total];
            f[0] = 0xFF;
            f[1] = 0xF1; // MPEG-4, Layer 0, protection absent
            f[2] = 0x50; // LC, 44.1kHz
            f[3] = 0x80; // stereo + length high bits
            let len = total as u16;
            f[3] |= ((len >> 11) as u8) & 0x03;
            f[4] = ((len >> 3) & 0xFF) as u8;
            f[5] = ((len & 0x07) << 5) as u8;
            f
        }
        let mut out = frame(10);
        out.extend(frame(12));
        out
    }

    #[test]
    fn drops_only_the_first_adts_frame() {
        let src = adts_silence_two_frames();
        let (first_len, _) = adts_frame_at(&src, 0).unwrap();
        let stripped = drop_first_adts_frame(&src).unwrap();
        assert_eq!(stripped.len(), src.len() - first_len);
        assert!(adts_frame_at(&stripped, 0).is_some());
    }

    #[test]
    fn elst_media_time_1024_is_priming() {
        // version 0, flags 0, entry_count 1, duration 1000, media_time 1024, rate 1.0
        let mut payload = vec![0, 0, 0, 0, 0, 0, 0, 1];
        payload.extend(1000u32.to_be_bytes());
        payload.extend(1024u32.to_be_bytes());
        payload.extend(0x00010000u32.to_be_bytes());
        assert_eq!(parse_elst_priming(&payload), Some(1024));
    }

    #[test]
    fn elst_media_time_zero_is_zero_priming() {
        let mut payload = vec![0, 0, 0, 0, 0, 0, 0, 1];
        payload.extend(1000u32.to_be_bytes());
        payload.extend(0u32.to_be_bytes());
        payload.extend(0x00010000u32.to_be_bytes());
        assert_eq!(parse_elst_priming(&payload), Some(0));
    }

    #[test]
    fn zero_priming_target_matches_ipod_m4b() {
        assert!(is_zero_priming_aac_m4b_target(
            true, "aac", "ipod", "m4b", None
        ));
        assert!(is_zero_priming_aac_m4b_target(
            true,
            "aac",
            "m4a",
            "m4a",
            Some(0)
        ));
        assert!(!is_zero_priming_aac_m4b_target(
            true, "aac", "m4a", "m4a", None
        ));
        assert!(!is_zero_priming_aac_m4b_target(
            false, "aac", "ipod", "m4v", None
        ));
        assert!(!is_zero_priming_aac_m4b_target(
            true, "mp3", "ipod", "m4b", None
        ));
        assert!(!is_zero_priming_aac_m4b_target(
            true, "aac", "ipod", "m4a", None
        ));
    }

    #[test]
    fn protected_extensions_are_refused() {
        assert!(is_protected_audiobook_path(Path::new("book.m4p")));
        assert!(is_protected_audiobook_path(Path::new("book.aax")));
        assert!(!is_protected_audiobook_path(Path::new("book.m4b")));
    }

    #[test]
    fn esds_expandable_length_reports_aac_lc() {
        let payload = hex_bytes(
            "000000000380808025000100048080801740150000000001f4000001f3770580808005121056e500",
        );
        assert_eq!(profile_from_esds(&payload).as_deref(), Some("LC"));
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
