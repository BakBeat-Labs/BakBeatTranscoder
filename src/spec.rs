// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Concrete transcode request supplied by BakBeat.
//!
//! BakBeat owns device policy. `bbt` receives the exact artifact shape the
//! caller wants, resolves any source-inherited technical fields, picks an
//! adapter, and executes.

use serde::{Deserialize, Serialize};

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

    /// Preserve embedded cover art/attached pictures for audio outputs.
    /// BakBeat can set this false for device paths that require audio-only
    /// files, avoiding any direct post-process call to ffmpeg.
    pub preserve_artwork: bool,
}
