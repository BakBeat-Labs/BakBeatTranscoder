// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BbtError {
    #[error("file not found: {0}")]
    FileNotFound(PathBuf),

    #[error("probe failed for {path}: {reason}")]
    ProbeFailed { path: PathBuf, reason: String },

    #[error("no audio track found in {0}")]
    NoAudioTrack(PathBuf),

    #[error("profile not found: '{0}'")]
    ProfileNotFound(String),

    #[error("invalid profile file {path}: {reason}")]
    InvalidProfile { path: PathBuf, reason: String },

    #[error("capability resolver: {0}")]
    CapabilityError(String),

    #[error("no adapter can encode to codec '{codec}' (available: {available})")]
    NoAdapterForCodec { codec: String, available: String },

    #[error("graph file not found: {0}")]
    GraphNotFound(PathBuf),

    #[error("invalid graph file {path}: {reason}")]
    InvalidGraph { path: PathBuf, reason: String },

    #[error("manifest file not found: {0}")]
    ManifestNotFound(PathBuf),

    #[error("invalid manifest {path}: {reason}")]
    InvalidManifest { path: PathBuf, reason: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("required binary '{binary}' not found in PATH")]
    BinaryNotFound { binary: String },

    #[error("encode failed for {path}: {stderr}")]
    EncodeFailed { path: PathBuf, stderr: String },

    #[error("intermediate decode failed for {path}: {stderr}")]
    DecodeFailed { path: PathBuf, stderr: String },

    #[error("unsupported codec: '{0}'")]
    UnsupportedCodec(String),

    #[error("IO error during encode: {0}")]
    Io(#[from] std::io::Error),
}
