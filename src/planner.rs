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
}

/// Plan a transcoding batch.
///
/// `inputs` is a flat list of audio files.
/// `output_dir` is the root of the output tree (relative path structure is preserved).
/// `source_root` is used to compute relative paths; defaults to the common prefix of inputs.
pub fn build_plan(
    inputs: &[PathBuf],
    profile: &DeviceProfile,
    output_dir: &Path,
    source_root: Option<&Path>,
) -> Result<TranscodePlan> {
    let mut jobs = Vec::new();

    for (_seq, input_path) in inputs.iter().enumerate() {
        let source_info = probe_file(input_path)?;

        // If already in the target format, skip (deterministic no-op detection)
        if source_info.matches_codec(&profile.codec, &profile.container) {
            tracing::debug!(
                path = ?input_path,
                "skipping: already in target format"
            );
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

    Ok(TranscodePlan { jobs })
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
        codec: profile.codec.clone(),
        container: profile.container.clone(),
        extension: profile.extension.clone(),
        bitrate_kbps: profile.bitrate_kbps,
        sample_rate_hz,
        channels,
        cbr: profile.cbr,
        extra: BTreeMap::new(),
    }
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}
