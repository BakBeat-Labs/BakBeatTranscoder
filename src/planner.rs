// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Projection planner — takes (inputs, profile) and produces a TranscodePlan.
//!
//! The planner is a pure function over the filesystem state at plan time.
//! It probes each input file and resolves all profile "inherit from source"
//! fields (None sample_rate, None channels, None width/height) into explicit values.
//! No encoding happens here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::gapless;
use crate::graph::{EncodeParams, ExecutionGraph, ExecutionNode, GaplessTrim};
use crate::probe::{probe_media, AudioInfo, MediaInfo};
use crate::profiles::DeviceProfile;

/// A planned job before adapter assignment. Holds resolved params but no adapter yet.
#[derive(Debug, Clone)]
pub struct PlannedJob {
    pub source_path: PathBuf,
    pub output_path: PathBuf,
    pub params: EncodeParams,
    pub assigned_adapter: Option<String>,
}

pub struct TranscodePlan {
    pub jobs: Vec<PlannedJob>,
    /// Files probed but skipped because they already match the target format.
    pub skipped_count: usize,
}

/// Plan a transcoding batch.
///
/// `on_probed` is called after each file: `(current_1_based, total, path, elapsed_ms)`.
pub fn build_plan(
    inputs: &[PathBuf],
    profile: &DeviceProfile,
    output_dir: &Path,
    source_root: Option<&Path>,
    mut on_probed: impl FnMut(usize, usize, &Path, u64),
) -> Result<TranscodePlan> {
    let total = inputs.len();
    let mut jobs = Vec::new();
    let mut skipped_count = 0usize;

    for (idx, input_path) in inputs.iter().enumerate() {
        let probe_start = std::time::Instant::now();
        let source_info = probe_media(input_path)?;
        let elapsed_ms = probe_start.elapsed().as_millis() as u64;

        on_probed(idx + 1, total, input_path, elapsed_ms);

        if source_info.already_matches_profile(profile) {
            tracing::debug!(path = ?input_path, "skipping: already in target format");
            skipped_count += 1;
            continue;
        }

        let output_path =
            resolve_output_path(input_path, source_root, output_dir, &profile.extension);

        let params = resolve_params(&source_info, profile);

        jobs.push(PlannedJob {
            source_path: input_path.clone(),
            output_path,
            params,
            assigned_adapter: None,
        });
    }

    Ok(TranscodePlan { jobs, skipped_count })
}

/// Convert a `TranscodePlan` (with assigned adapters) into a serializable `ExecutionGraph`.
pub fn plan_to_graph(plan: &TranscodePlan) -> Result<ExecutionGraph> {
    let mut nodes = Vec::new();

    for (seq, job) in plan.jobs.iter().enumerate() {
        let adapter = job
            .assigned_adapter
            .clone()
            .expect("adapters must be assigned before building graph");

        let input_sha256 = hash_file(&job.source_path)?;
        let input_size_bytes = std::fs::metadata(&job.source_path)?.len();

        nodes.push(ExecutionNode {
            id: Uuid::new_v4(),
            sequence: seq as u32,
            input_path: job.source_path.clone(),
            input_sha256,
            input_size_bytes,
            output_path: job.output_path.clone(),
            adapter,
            params: job.params.clone(),
        });
    }

    Ok(ExecutionGraph::new(nodes))
}

fn resolve_output_path(
    input: &Path,
    source_root: Option<&Path>,
    output_dir: &Path,
    extension: &str,
) -> PathBuf {
    let relative = if let Some(root) = source_root {
        input.strip_prefix(root).unwrap_or(input)
    } else {
        Path::new(input.file_name().unwrap_or(input.as_os_str()))
    };
    let mut output = output_dir.join(relative);
    output.set_extension(extension);
    output
}

/// Resolve profile `Option` fields against probed source info.
/// Every field in `EncodeParams` is explicit after this — no Nones where a
/// concrete value is needed for deterministic encoding.
fn resolve_params(source: &MediaInfo, profile: &DeviceProfile) -> EncodeParams {
    // Pull source audio params
    let (src_sample_rate, src_channels) = match source {
        MediaInfo::Audio(a) => (a.sample_rate_hz, a.channels),
        MediaInfo::Video(v) => {
            let a = v.audio_streams.first();
            (a.and_then(|s| s.sample_rate_hz), a.and_then(|s| s.channels))
        }
    };

    // Pull source video params (for "preserve source" semantics when profile leaves them None)
    let (src_width, src_height, src_frame_rate) = match source {
        MediaInfo::Audio(_) => (None, None, None),
        MediaInfo::Video(v) => {
            let vs = v.video_streams.first();
            (
                vs.map(|s| s.width),
                vs.map(|s| s.height),
                vs.and_then(|s| s.frame_rate),
            )
        }
    };

    let gapless_trim = compute_gapless_trim(source);
    if let Some(ref trim) = gapless_trim {
        tracing::debug!(
            encoder_delay = trim.encoder_delay,
            trailing_padding = trim.trailing_padding,
            output_frames = trim.output_frames,
            "iTunSMPB gapless trim applied"
        );
    }

    EncodeParams {
        media_type: profile.media_type.clone(),
        container: profile.container.clone(),
        extension: profile.extension.clone(),
        cbr: profile.cbr,
        audio_codec: profile.audio_codec.clone(),
        audio_bitrate_kbps: profile.audio_bitrate_kbps,
        sample_rate_hz: profile.sample_rate_hz.or(src_sample_rate).unwrap_or(44100),
        channels: profile.channels.or(src_channels).unwrap_or(2),
        video_codec: profile.video_codec.clone(),
        video_bitrate_kbps: profile.video_bitrate_kbps,
        width: profile.width.or(src_width),
        height: profile.height.or(src_height),
        frame_rate: profile.frame_rate.or(src_frame_rate),
        pixel_format: profile.pixel_format.clone(),
        gapless_trim,
        extra: BTreeMap::new(),
    }
}

/// Derive iTunSMPB gapless trim for AAC/M4A sources.
///
/// Only applied when the source is an AAC codec in an M4A container with a
/// parseable iTunSMPB tag that carries non-zero trailing padding.
///
/// Output frame count authority (in order):
/// 1. iTunSMPB word 3 (total_pcm_samples) — matches afconvert when present.
/// 2. n_frames − encoder_delay − trailing_padding — fallback when word 3 is absent.
fn compute_gapless_trim(source: &MediaInfo) -> Option<GaplessTrim> {
    let audio = match source {
        MediaInfo::Audio(a) => a,
        MediaInfo::Video(_) => return None,
    };

    if !is_aac_m4a(audio) {
        return None;
    }

    let raw = audio.tags.get("itunsmpb")?;
    let smpb = gapless::parse_itunsmpb(raw)?;

    if smpb.trailing_padding == 0 {
        return None;
    }

    let output_frames = if let Some(total) = smpb.total_pcm_samples {
        total
    } else {
        let n = audio.n_frames?;
        // n_frames includes priming + valid + trailing; subtract both ends
        n.saturating_sub(smpb.encoder_delay)
            .saturating_sub(smpb.trailing_padding)
    };

    if output_frames == 0 {
        return None;
    }

    Some(GaplessTrim {
        encoder_delay: smpb.encoder_delay,
        trailing_padding: smpb.trailing_padding,
        output_frames,
    })
}

fn is_aac_m4a(audio: &AudioInfo) -> bool {
    audio.container == "m4a" && audio.codec == "aac"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use crate::probe::AudioInfo;

    fn make_aac_m4a(tags: BTreeMap<String, String>, n_frames: Option<u64>) -> MediaInfo {
        MediaInfo::Audio(AudioInfo {
            path: "/tmp/test.m4a".into(),
            container: "m4a".to_string(),
            codec: "aac".to_string(),
            sample_rate_hz: Some(44100),
            channels: Some(2),
            bits_per_sample: None,
            duration_secs: None,
            n_frames,
            bitrate_kbps: None,
            tags,
        })
    }

    #[test]
    fn trim_from_total_pcm_samples() {
        // dbpoweramp fixture: total_pcm_samples word is authoritative
        let mut tags = BTreeMap::new();
        tags.insert(
            "itunsmpb".to_string(),
            "00000000 00000840 000003C8 0000000000AE13F8".to_string(),
        );
        let source = make_aac_m4a(tags, Some(11411456));
        let trim = compute_gapless_trim(&source).expect("should compute trim");
        assert_eq!(trim.encoder_delay, 2112);
        assert_eq!(trim.trailing_padding, 968);
        // word 3 (total_pcm_samples) takes precedence → matches afconvert
        assert_eq!(trim.output_frames, 11408376);
    }

    #[test]
    fn trim_fallback_without_total_pcm() {
        // Without word 3, derive from n_frames − delay − trailing
        // n_frames = 2112 + 11408376 + 968 = 11411456
        let mut tags = BTreeMap::new();
        tags.insert(
            "itunsmpb".to_string(),
            "00000000 00000840 000003C8".to_string(),
        );
        let source = make_aac_m4a(tags, Some(11411456));
        let trim = compute_gapless_trim(&source).expect("should compute trim");
        assert_eq!(trim.output_frames, 11408376);
    }

    #[test]
    fn no_trim_when_itunsmpb_absent() {
        let source = make_aac_m4a(BTreeMap::new(), Some(11411456));
        assert!(compute_gapless_trim(&source).is_none());
    }

    #[test]
    fn no_trim_when_trailing_padding_zero() {
        let mut tags = BTreeMap::new();
        tags.insert(
            "itunsmpb".to_string(),
            "00000000 00000840 00000000 0000000000AE13F8".to_string(),
        );
        let source = make_aac_m4a(tags, Some(11411456));
        assert!(compute_gapless_trim(&source).is_none());
    }

    #[test]
    fn no_trim_for_non_m4a_source() {
        let mut tags = BTreeMap::new();
        tags.insert(
            "itunsmpb".to_string(),
            "00000000 00000840 000003C8 0000000000AE13F8".to_string(),
        );
        let source = MediaInfo::Audio(AudioInfo {
            path: "/tmp/test.flac".into(),
            container: "flac".to_string(),
            codec: "flac".to_string(),
            sample_rate_hz: Some(44100),
            channels: Some(2),
            bits_per_sample: Some(16),
            duration_secs: None,
            n_frames: Some(11408376),
            bitrate_kbps: None,
            tags,
        });
        assert!(compute_gapless_trim(&source).is_none());
    }

    #[test]
    fn dbpoweramp_fixture_frame_count_matches_afconvert() {
        // End-to-end: simulate the failing fixture.
        // ffmpeg decoded 11409344 frames (priming stripped, trailing kept).
        // After trim with output_frames=11408376 → matches afconvert.
        let mut tags = BTreeMap::new();
        tags.insert(
            "itunsmpb".to_string(),
            "00000000 00000840 000003C8 0000000000AE13F8".to_string(),
        );
        let source = make_aac_m4a(tags, Some(11411456));
        let trim = compute_gapless_trim(&source).unwrap();

        // Simulate: ffmpeg decoded (n_frames - encoder_delay) = 11409344 frames
        let ffmpeg_decoded: u64 = 11411456 - 2112; // 11409344
        let after_trim = ffmpeg_decoded.min(trim.output_frames);
        assert_eq!(after_trim, 11408376, "trimmed count must match afconvert");
    }
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}
