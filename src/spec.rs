// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Concrete transcode request supplied by BakBeat.
//!
//! BakBeat owns device policy. `bbt` receives the exact artifact shape the
//! caller wants, resolves any source-inherited technical fields, picks an
//! adapter, and executes.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::graph::MediaType;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscodeSpec {
    #[serde(default)]
    pub media_type: MediaType,

    pub container: String,
    pub extension: String,
    pub cbr: bool,

    pub audio_codec: String,
    pub audio_bitrate_kbps: Option<u32>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u8>,

    pub video_codec: Option<String>,
    pub video_bitrate_kbps: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f32>,
    pub pixel_format: Option<String>,
    pub video_filter: Option<String>,
    pub video_profile: Option<String>,
    pub video_level: Option<String>,
    pub poster_artwork_path: Option<PathBuf>,
    pub hwaccel: Option<String>,
    pub movflags: Option<String>,
    pub audio_block_size: Option<u32>,

    /// Preserve embedded cover art/attached pictures for audio outputs.
    /// BakBeat can set this false for device paths that require audio-only
    /// files, avoiding any direct post-process call to ffmpeg.
    pub preserve_artwork: bool,

    /// Requested AAC encoder priming in PCM samples. `Some(0)` forces the
    /// zero-priming AAC-LC ipod/m4b path. Inferred for `--container ipod
    /// --extension m4b` even when this is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aac_priming: Option<u32>,
}
