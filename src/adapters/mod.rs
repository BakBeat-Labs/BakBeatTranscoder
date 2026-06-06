// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Encoder adapter trait and artifact types.
//! Adapters are stateless — all encoding parameters come in via ExecutionNode,
//! all results come out via ArtifactInfo. Adapters are replaceable by design.

pub mod atrac;
pub mod ffmpeg;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::AdapterError;
use crate::graph::ExecutionNode;

/// Result of a successful encode operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactInfo {
    pub output_path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
    /// Duration probed from the output file, if available.
    pub duration_ms: Option<u64>,
}

/// Trait implemented by all encoder backends.
/// Adapters must be Send + Sync — the graph executor will run them
/// across threads when concurrency is enabled in a future release.
pub trait EncoderAdapter: Send + Sync {
    /// Codec strings this adapter can produce (e.g. ["mp3", "aac", "flac"])
    fn supported_output_codecs(&self) -> &[&str];

    /// Whether the underlying binary is present and executable.
    fn is_available(&self) -> bool;

    /// Encode one node. Receives fully resolved parameters.
    /// Must not modify the input file. Output directory will be created if absent.
    fn encode(&self, node: &ExecutionNode) -> Result<ArtifactInfo, AdapterError>;
}

/// Compute SHA-256 of a file and return hex string.
pub(crate) fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

/// Create parent directories for a path if they don't exist.
pub(crate) fn ensure_parent(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}
