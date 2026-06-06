// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The deterministic execution graph — frozen, serialized, hashable machine truth.
//!
//! A graph is fully resolved before any I/O begins: every parameter is explicit,
//! every input is hashed, every output path is determined. The same graph run
//! against the same input hashes must produce identical outputs.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Schema 1.1 — renamed codec→audio_codec, bitrate_kbps→audio_bitrate_kbps;
/// added MediaType, video stream fields to EncodeParams.
pub const GRAPH_SCHEMA_VERSION: &str = "1.1";

/// Whether a node encodes audio-only or a video file (with embedded audio track).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    #[default]
    Audio,
    Video,
}

/// A fully resolved, serializable transcoding plan ready for execution.
/// Saved as JSON — canonical ordering matters for stable hashing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionGraph {
    pub schema_version: String,
    pub graph_id: Uuid,
    pub created_at: DateTime<Utc>,
    /// SHA-256 of the canonically serialized nodes array.
    /// Changing any parameter changes this hash — use it to detect tampering or drift.
    pub graph_hash: String,
    pub nodes: Vec<ExecutionNode>,
}

/// A single encode operation with all parameters fully resolved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionNode {
    pub id: Uuid,
    /// Stable ordering index within the graph (for reproducible sequential execution)
    pub sequence: u32,
    pub input_path: PathBuf,
    /// SHA-256 hex of the input file at plan time.
    /// If this changes before execution, the run should be considered tainted.
    pub input_sha256: String,
    pub input_size_bytes: u64,
    pub output_path: PathBuf,
    /// Which adapter handles this node (e.g. "ffmpeg", "atrac")
    pub adapter: String,
    pub params: EncodeParams,
}

/// Fully resolved encoding parameters. All audio fields are always present.
/// Video fields are None for audio-only nodes.
/// BTreeMap fields use stable ordering for deterministic hashing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodeParams {
    #[serde(default)]
    pub media_type: MediaType,

    pub container: String,
    pub extension: String,
    pub cbr: bool,

    // ── Audio track ───────────────────────────────────────────────────────────
    // For audio-only: the primary codec.
    // For video: the codec used for the embedded audio track.
    pub audio_codec: String,
    pub audio_bitrate_kbps: Option<u32>,
    pub sample_rate_hz: u32,
    pub channels: u8,

    // ── Video stream ──────────────────────────────────────────────────────────
    // All None for audio-only nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_bitrate_kbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Frames per second. None = preserve source frame rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<f32>,
    /// FFmpeg pixel format string (e.g. "yuv420p"). None = let FFmpeg decide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixel_format: Option<String>,

    // ── Adapter-specific overrides ────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, String>,
}

impl ExecutionGraph {
    pub fn new(nodes: Vec<ExecutionNode>) -> Self {
        let graph_id = Uuid::new_v4();
        let graph_hash = compute_graph_hash(&nodes);
        Self {
            schema_version: GRAPH_SCHEMA_VERSION.to_string(),
            graph_id,
            created_at: Utc::now(),
            graph_hash,
            nodes,
        }
    }

    pub fn save_to_file(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load_from_file(path: &std::path::Path) -> anyhow::Result<Self> {
        use crate::error::BbtError;
        let content = std::fs::read_to_string(path)
            .map_err(|_| BbtError::GraphNotFound(path.to_owned()))?;
        serde_json::from_str(&content).map_err(|e| {
            BbtError::InvalidGraph {
                path: path.to_owned(),
                reason: e.to_string(),
            }
            .into()
        })
    }

    /// Recompute and verify the graph hash. Returns true if intact.
    pub fn verify_hash(&self) -> bool {
        compute_graph_hash(&self.nodes) == self.graph_hash
    }
}

fn compute_graph_hash(nodes: &[ExecutionNode]) -> String {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::to_string(nodes).expect("nodes always serialize");
    hex::encode(Sha256::digest(canonical.as_bytes()))
}
