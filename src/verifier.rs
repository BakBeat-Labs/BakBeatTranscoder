// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Artifact verifier and TranscodeManifest.
//!
//! Every execution produces a manifest. The manifest is the ground truth
//! record of what was produced: paths, hashes, sizes, timestamps.
//! `bbt verify <manifest.json>` re-checks all artifacts at any future point.
//! `bbt resume <manifest.json>` re-encodes anything that failed or drifted.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::BbtError;
use crate::graph::ExecutionGraph;

pub const MANIFEST_SCHEMA_VERSION: &str = "1.0";

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub node_id: Uuid,
    pub output_path: PathBuf,
    /// SHA-256 hex of the output file at encode time.
    pub sha256: String,
    pub size_bytes: u64,
    pub duration_ms: Option<u64>,
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
    CarriedForward {
        from_manifest_id: Uuid,
    },
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
    HashMismatch { expected: String, actual: String },
    SizeMismatch { expected: u64, actual: u64 },
    OriginallyFailed { error: String },
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

    /// Re-verify all artifacts against their recorded hashes.
    pub fn verify(&self) -> Vec<VerificationResult> {
        self.artifacts
            .iter()
            .map(|record| {
                let status = match &record.status {
                    ArtifactStatus::Failed { error } => {
                        VerificationStatus::OriginallyFailed { error: error.clone() }
                    }
                    ArtifactStatus::Skipped { .. } => {
                        VerificationStatus::OriginallyFailed {
                            error: "skipped".to_string(),
                        }
                    }
                    ArtifactStatus::CarriedForward { .. } => {
                        // Re-verify the file is still intact
                        match check_artifact_integrity(record) {
                            true => VerificationStatus::CarriedForward,
                            false => VerificationStatus::Missing,
                        }
                    }
                    ArtifactStatus::Success => check_artifact(record),
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

/// Check whether a previously successful artifact is still valid on disk.
/// Returns true only if the file exists AND the SHA-256 still matches.
/// Any doubt → false → re-encode.
pub fn artifact_still_valid(record: &ArtifactRecord) -> bool {
    check_artifact_integrity(record)
}

fn check_artifact_integrity(record: &ArtifactRecord) -> bool {
    if !record.output_path.exists() {
        return false;
    }
    match compute_sha256(&record.output_path) {
        Ok(hash) => hash == record.sha256,
        Err(_) => false,
    }
}

fn check_artifact(record: &ArtifactRecord) -> VerificationStatus {
    if !record.output_path.exists() {
        return VerificationStatus::Missing;
    }

    let meta = match std::fs::metadata(&record.output_path) {
        Ok(m) => m,
        Err(_) => return VerificationStatus::Missing,
    };

    if meta.len() != record.size_bytes {
        return VerificationStatus::SizeMismatch {
            expected: record.size_bytes,
            actual: meta.len(),
        };
    }

    match compute_sha256(&record.output_path) {
        Ok(hash) if hash == record.sha256 => VerificationStatus::Ok,
        Ok(hash) => VerificationStatus::HashMismatch {
            expected: record.sha256.clone(),
            actual: hash,
        },
        Err(_) => VerificationStatus::Missing,
    }
}

fn compute_sha256(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}
