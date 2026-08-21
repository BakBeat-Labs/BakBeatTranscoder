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

/// Result of a successful encode operation — the facts needed to decide the
/// output exists, is playable-shaped, and matches what was asked for. Not a
/// content hash: see `crate::graph::SourceFingerprint` for why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactInfo {
    pub output_path: PathBuf,
    pub size_bytes: u64,
    /// Duration probed from the output file, if available.
    pub duration_ms: Option<u64>,
    pub container: String,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priming_samples: Option<u64>,
}

/// Trait implemented by all encoder backends.
/// Adapters must be Send + Sync — the graph executor will run them
/// across threads when concurrency is enabled in a future release.
pub trait EncoderAdapter: Send + Sync {
    /// Codec strings this adapter can produce (e.g. ["mp3", "aac", "flac"])
    fn supported_output_codecs(&self) -> &[&str];

    /// Whether the underlying binary is present and executable.
    fn is_available(&self) -> bool;

    /// Whether this adapter's active binary can actually produce the given
    /// codec right now. Defaults to a static membership check against
    /// `supported_output_codecs`; adapters whose binary may be built without
    /// some optional encoder (e.g. FFmpeg without `--enable-libxvid`) should
    /// override this with a runtime check.
    fn is_codec_available(&self, codec: &str) -> bool {
        self.supported_output_codecs().contains(&codec)
    }

    /// Encode one node. Receives fully resolved parameters.
    /// Must not modify the input file. Output directory will be created if absent.
    fn encode(&self, node: &ExecutionNode) -> Result<ArtifactInfo, AdapterError>;
}

/// Build an `ArtifactInfo` by re-probing a freshly encoded output — cheap
/// header-level facts (container, codec, dimensions, duration), not a
/// content hash. The hard bar is "exists and nonzero"; shape facts are
/// best-effort on top of that, since some adapters (ATRAC's .aea/.oma) emit
/// formats neither Symphonia nor ffprobe parse. A format bbt itself can't
/// probe is not a failed encode.
pub(crate) fn probe_output(path: &std::path::Path) -> Result<ArtifactInfo, AdapterError> {
    let size_bytes = std::fs::metadata(path)?.len();
    if size_bytes == 0 {
        return Err(AdapterError::EncodeFailed {
            path: path.to_path_buf(),
            stderr: "output file is empty".to_string(),
        });
    }

    match crate::probe::probe_media(path) {
        Ok(info) => {
            let duration_ms = info.duration_secs().map(|d| (d * 1000.0).round() as u64);
            let (video_codec, audio_codec, width, height, priming_samples) = match &info {
                crate::probe::MediaInfo::Audio(a) => {
                    (None, Some(a.codec.clone()), None, None, a.priming_samples)
                }
                crate::probe::MediaInfo::Video(v) => (
                    v.video_streams.first().map(|s| s.codec.clone()),
                    v.audio_streams.first().map(|s| s.codec.clone()),
                    v.video_streams.first().map(|s| s.width),
                    v.video_streams.first().map(|s| s.height),
                    None,
                ),
            };

            Ok(ArtifactInfo {
                output_path: path.to_path_buf(),
                size_bytes,
                duration_ms,
                container: info.container().to_string(),
                video_codec,
                audio_codec,
                width,
                height,
                priming_samples,
            })
        }
        Err(e) => {
            tracing::debug!(path = ?path, error = %e, "post-encode probe skipped: format not parseable by bbt's probers");
            let container = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_string();
            Ok(ArtifactInfo {
                output_path: path.to_path_buf(),
                size_bytes,
                duration_ms: None,
                container,
                video_codec: None,
                audio_codec: None,
                width: None,
                height: None,
                priming_samples: None,
            })
        }
    }
}

/// Create parent directories for a path if they don't exist.
pub(crate) fn ensure_parent(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}
