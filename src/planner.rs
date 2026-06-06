// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Projection planner — takes (inputs, profile) and produces a TranscodePlan.
//!
//! The planner is a pure function over the filesystem state at plan time.
//! It probes each input file and resolves all profile "inherit from source"
//! fields (None sample_rate, None channels) into explicit values.
//! No encoding happens here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::graph::{EncodeParams, ExecutionGraph, ExecutionNode};
use crate::probe::{probe_file, AudioInfo};
use crate::profiles::DeviceProfile;

/// A planned job before adapter assignment. Holds resolved params but no adapter yet.
#[derive(Debug, Clone)]
pub struct PlannedJob {
    pub source_path: PathBuf,
    pub source_info: AudioInfo,
    pub output_path: PathBuf,
    pub params: EncodeParams,
    pub assigned_adapter: Option<String>,
}

pub struct TranscodePlan {
    pub jobs: Vec<PlannedJob>,
    /// Files that were probed but skipped because they're already in the target format.
    pub skipped_count: usize,
}

/// Plan a transcoding batch.
///
/// `inputs` is a flat list of audio files.
/// `output_dir` is the root of the output tree (relative path structure is preserved).
/// `source_root` is used to compute relative paths; defaults to the common prefix of inputs.
/// `on_probed` is called after each file is probed: `(current_1_based, total, path, elapsed_ms)`.
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
        let source_info = probe_file(input_path)?;
        let elapsed_ms = probe_start.elapsed().as_millis() as u64;

        on_probed(idx + 1, total, input_path, elapsed_ms);

        // If already in the target format, skip (deterministic no-op detection)
        if source_info.matches_codec(&profile.audio_codec, &profile.container) {
            tracing::debug!(path = ?input_path, "skipping: already in target format");
            skipped_count += 1;
            continue;
        }

        let output_path = resolve_output_path(
            input_path,
            source_root,
            output_dir,
            &profile.extension,
        );

        let params = resolve_params(&source_info, profile);

        jobs.push(PlannedJob {
            source_path: input_path.clone(),
            source_info,
            output_path,
            params,
            assigned_adapter: None,
        });
    }

    Ok(TranscodePlan { jobs, skipped_count })
}

/// Convert a `TranscodePlan` (with assigned adapters) into a serializable `ExecutionGraph`.
/// This hashes all input files and fixes all parameters into the frozen graph representation.
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

/// Resolve the output path for an input file, preserving relative directory structure.
fn resolve_output_path(
    input: &Path,
    source_root: Option<&Path>,
    output_dir: &Path,
    extension: &str,
) -> PathBuf {
    let relative = if let Some(root) = source_root {
        input.strip_prefix(root).unwrap_or(input)
    } else {
        // If no source root, just use the filename
        Path::new(input.file_name().unwrap_or(input.as_os_str()))
    };

    let mut output = output_dir.join(relative);
    output.set_extension(extension);
    output
}

/// Resolve a profile's `Option` fields against the probed source info.
/// All resulting `EncodeParams` fields are explicit — no Nones.
fn resolve_params(source: &AudioInfo, profile: &DeviceProfile) -> EncodeParams {
    let sample_rate_hz = profile
        .sample_rate_hz
        .or(source.sample_rate_hz)
        .unwrap_or(44100); // safe fallback

    let channels = profile
        .channels
        .or(source.channels)
        .unwrap_or(2); // safe fallback to stereo

    EncodeParams {
        media_type: profile.media_type.clone(),
        container: profile.container.clone(),
        extension: profile.extension.clone(),
        cbr: profile.cbr,
        audio_codec: profile.audio_codec.clone(),
        audio_bitrate_kbps: profile.audio_bitrate_kbps,
        sample_rate_hz,
        channels,
        video_codec: profile.video_codec.clone(),
        video_bitrate_kbps: profile.video_bitrate_kbps,
        width: profile.width,
        height: profile.height,
        frame_rate: profile.frame_rate,
        pixel_format: profile.pixel_format.clone(),
        extra: BTreeMap::new(),
    }
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}
