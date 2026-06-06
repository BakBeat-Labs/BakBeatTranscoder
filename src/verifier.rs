// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Artifact verifier and TranscodeManifest.
//!
//! Every execution produces a manifest. The manifest is the ground truth
//! record of what was produced: paths, hashes, sizes, timestamps.
//! `bbt verify <manifest.json>` re-checks all artifacts at any future point.

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
    Success,
    Failed { error: String },
    Skipped { reason: String },
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
        let mut results = Vec::new();

        for record in &self.artifacts {
            let status = match &record.status {
                ArtifactStatus::Failed { error } => VerificationStatus::OriginallyFailed {
                    error: error.clone(),
                },
                ArtifactStatus::Skipped { reason } => VerificationStatus::OriginallyFailed {
                    error: format!("skipped: {reason}"),
                },
                ArtifactStatus::Success => verify_artifact(record),
            };

            results.push(VerificationResult {
                node_id: record.node_id,
                output_path: record.output_path.clone(),
                status,
            });
        }

        results
    }
}

fn verify_artifact(record: &ArtifactRecord) -> VerificationStatus {
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

    let actual_hash = match compute_sha256(&record.output_path) {
        Ok(h) => h,
        Err(_) => return VerificationStatus::Missing,
    };

    if actual_hash != record.sha256 {
        return VerificationStatus::HashMismatch {
            expected: record.sha256.clone(),
            actual: actual_hash,
        };
    }

    VerificationStatus::Ok
}

fn compute_sha256(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}
