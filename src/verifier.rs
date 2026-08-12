// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Artifact verifier and TranscodeManifest.
//!
//! Every execution produces a manifest. The manifest is the ground truth
//! record of what was produced: paths, sizes, shapes, timestamps.
//! `bbt verify <manifest.json>` re-checks all artifacts at any future point.
//! `bbt resume <manifest.json>` re-encodes anything that failed or drifted.
//!
//! bbt produces device-friendly derivatives, not archival copies. Artifact
//! validity is judged the way a build system judges an artifact: does the
//! output exist, is it playable-shaped (right container/codec/dimensions),
//! and is its duration sane relative to the source — not "does its content
//! hash match byte-for-byte". See `crate::graph::SourceFingerprint`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::BbtError;
use crate::graph::{ExecutionGraph, ExecutionNode};
use crate::probe::probe_media;

pub const MANIFEST_SCHEMA_VERSION: &str = "2.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscodeManifest {
    pub schema_version: String,
    pub manifest_id: Uuid,
    pub completed_at: DateTime<Utc>,
    pub total_elapsed_ms: u64,
    pub success_count: usize,
    pub failure_count: usize,
    pub carried_forward_count: usize,
    /// manifest_id of the manifest this run resumed from, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<Uuid>,
    pub graph: ExecutionGraph,
    pub artifacts: Vec<ArtifactRecord>,
}

/// The facts recorded about a produced artifact — build-artifact facts, not
/// archival provenance. Enough to tell a device file exists and is
/// playable-shaped; not a promise the bytes never changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub node_id: Uuid,
    pub output_path: PathBuf,
    pub size_bytes: u64,
    pub duration_ms: Option<u64>,
    pub container: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub encode_elapsed_ms: u64,
    pub verified_at: Option<DateTime<Utc>>,
    pub status: ArtifactStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ArtifactStatus {
    /// Freshly encoded this run.
    Success,
    /// Verified intact from a previous run; not re-encoded.
    CarriedForward { from_manifest_id: Uuid },
    /// Encode failed.
    Failed { error: String },
    /// Intentionally skipped (e.g. already in target format).
    Skipped { reason: String },
}

impl ArtifactStatus {
    /// Whether this artifact is available and usable.
    pub fn is_good(&self) -> bool {
        matches!(self, Self::Success | Self::CarriedForward { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationResult {
    pub node_id: Uuid,
    pub output_path: PathBuf,
    pub status: VerificationStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VerificationStatus {
    Ok,
    Missing,
    /// File exists but is zero bytes.
    Empty,
    /// File exists and is nonzero, but bbt's probers couldn't parse it as
    /// media at all (distinct from an intentionally unprobeable format like
    /// ATRAC — this means it failed to open, e.g. truncated/corrupt).
    Unreadable {
        error: String,
    },
    /// Container/codec/dimensions drifted from what was recorded at encode time.
    ShapeMismatch {
        detail: String,
    },
    /// Output duration is not close to the source duration recorded at plan time.
    DurationMismatch {
        expected_secs: f64,
        actual_secs: f64,
    },
    OriginallyFailed {
        error: String,
    },
    CarriedForward,
}

impl TranscodeManifest {
    pub fn new(
        graph: ExecutionGraph,
        artifacts: Vec<ArtifactRecord>,
        total_elapsed_ms: u64,
        success_count: usize,
        failure_count: usize,
    ) -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION.to_string(),
            manifest_id: Uuid::new_v4(),
            completed_at: Utc::now(),
            total_elapsed_ms,
            success_count,
            failure_count,
            carried_forward_count: 0,
            resumed_from: None,
            graph,
            artifacts,
        }
    }

    pub fn new_resumed(
        graph: ExecutionGraph,
        artifacts: Vec<ArtifactRecord>,
        total_elapsed_ms: u64,
        success_count: usize,
        failure_count: usize,
        carried_forward_count: usize,
        resumed_from: Uuid,
    ) -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION.to_string(),
            manifest_id: Uuid::new_v4(),
            completed_at: Utc::now(),
            total_elapsed_ms,
            success_count,
            failure_count,
            carried_forward_count,
            resumed_from: Some(resumed_from),
            graph,
            artifacts,
        }
    }

    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load_from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|_| BbtError::ManifestNotFound(path.to_owned()))?;
        serde_json::from_str(&content).map_err(|e| {
            BbtError::InvalidManifest {
                path: path.to_owned(),
                reason: e.to_string(),
            }
            .into()
        })
    }

    /// Re-verify all artifacts: existence, shape, and duration sanity against
    /// the source facts recorded in the graph at plan time.
    pub fn verify(&self) -> Vec<VerificationResult> {
        let nodes: HashMap<Uuid, &ExecutionNode> =
            self.graph.nodes.iter().map(|n| (n.id, n)).collect();

        self.artifacts
            .iter()
            .map(|record| {
                let status = match &record.status {
                    ArtifactStatus::Failed { error } => VerificationStatus::OriginallyFailed {
                        error: error.clone(),
                    },
                    ArtifactStatus::Skipped { .. } => VerificationStatus::OriginallyFailed {
                        error: "skipped".to_string(),
                    },
                    ArtifactStatus::CarriedForward { .. } => {
                        if artifact_still_valid(record) {
                            VerificationStatus::CarriedForward
                        } else {
                            VerificationStatus::Missing
                        }
                    }
                    ArtifactStatus::Success => {
                        check_artifact(record, nodes.get(&record.node_id).copied())
                    }
                };
                VerificationResult {
                    node_id: record.node_id,
                    output_path: record.output_path.clone(),
                    status,
                }
            })
            .collect()
    }
}

/// Whether a previously successful artifact still looks usable on disk:
/// exists, nonzero, and (best-effort) still probes to the recorded shape.
/// Any doubt → false → the caller re-encodes rather than trusting a maybe.
pub fn artifact_still_valid(record: &ArtifactRecord) -> bool {
    let Ok(meta) = std::fs::metadata(&record.output_path) else {
        return false;
    };
    if meta.len() == 0 {
        return false;
    }
    match probe_media(&record.output_path) {
        Ok(probed) => probed.container().eq_ignore_ascii_case(&record.container),
        // Unparseable by bbt's own probers (e.g. ATRAC) isn't itself a failure —
        // fall back to "exists and nonzero", which is all we recorded for it originally.
        Err(_) => record.video_codec.is_none() && record.audio_codec.is_none(),
    }
}

/// Duration tolerance: 5% of the expected length, or 2s, whichever is larger.
/// Loose on purpose — this is a sanity check against truncated/wrong-track
/// outputs, not a frame-accurate comparison.
fn duration_tolerance_secs(expected_secs: f64) -> f64 {
    (expected_secs * 0.05).max(2.0)
}

fn check_artifact(record: &ArtifactRecord, node: Option<&ExecutionNode>) -> VerificationStatus {
    let meta = match std::fs::metadata(&record.output_path) {
        Ok(m) => m,
        Err(_) => return VerificationStatus::Missing,
    };
    if meta.len() == 0 {
        return VerificationStatus::Empty;
    }

    let probed = match probe_media(&record.output_path) {
        Ok(p) => p,
        Err(e) => {
            // Formats bbt can't parse at all (ATRAC's .aea/.oma) were never
            // shape-checked to begin with — exists + nonzero is the whole bar.
            if record.video_codec.is_none() && record.audio_codec.is_none() {
                return VerificationStatus::Ok;
            }
            return VerificationStatus::Unreadable {
                error: e.to_string(),
            };
        }
    };

    if !probed.container().eq_ignore_ascii_case(&record.container) {
        return VerificationStatus::ShapeMismatch {
            detail: format!(
                "container: expected {}, found {}",
                record.container,
                probed.container()
            ),
        };
    }

    if let (Some((rec_w, rec_h)), Some((w, h))) =
        (record.width.zip(record.height), probed.dimensions())
    {
        if (rec_w, rec_h) != (w, h) {
            return VerificationStatus::ShapeMismatch {
                detail: format!("dimensions: expected {rec_w}x{rec_h}, found {w}x{h}"),
            };
        }
    }

    if let (Some(node), Some(actual_secs)) = (node, probed.duration_secs()) {
        if let Some(expected_secs) = node.input.duration_secs {
            let tolerance = duration_tolerance_secs(expected_secs);
            if (actual_secs - expected_secs).abs() > tolerance {
                return VerificationStatus::DurationMismatch {
                    expected_secs,
                    actual_secs,
                };
            }
        }
    }

    VerificationStatus::Ok
}
