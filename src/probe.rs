// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Symphonia-based audio probing. Symphonia is our metadata authority —
//! we do NOT shell out to FFmpeg for probing or format understanding.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use symphonia::core::codecs::CodecType;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey, Value};
use symphonia::core::probe::Hint;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioInfo {
    pub path: PathBuf,
    /// Container format derived from file extension (e.g. "flac", "mp3", "m4a")
    pub container: String,
    /// Codec string (e.g. "mp3", "aac", "flac", "vorbis", "alac", "pcm_s16le")
    pub codec: String,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u8>,
    pub bits_per_sample: Option<u32>,
    /// Approximate duration in seconds, derived from n_frames / sample_rate
    pub duration_secs: Option<f64>,
    /// Approximate bitrate in kbps, estimated from file size and duration
    pub bitrate_kbps: Option<u32>,
    /// Normalized metadata tags (title, artist, album, etc.)
    pub tags: BTreeMap<String, String>,
}

impl AudioInfo {
    /// Whether this file is already in the target format (codec + container match).
    /// Used by the planner to skip no-op transcodes.
    pub fn matches_codec(&self, codec: &str, container: &str) -> bool {
        self.codec.eq_ignore_ascii_case(codec)
            && self.container.eq_ignore_ascii_case(container)
    }
}

pub fn probe_file(path: &Path) -> Result<AudioInfo> {
    if !path.exists() {
        return Err(anyhow!("File not found: {}", path.display()));
    }

    let file_size = std::fs::metadata(path)
        .context("reading file metadata")?
        .len();

    let src = File::open(path).context("opening audio file")?;
    let mss = MediaSourceStream::new(Box::new(src), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let fmt_opts = FormatOptions {
        enable_gapless: false,
        ..Default::default()
    };
    let meta_opts: MetadataOptions = Default::default();

    let mut probed = symphonia::default::get_probe()
        .format(&hint, mss, &fmt_opts, &meta_opts)
        .with_context(|| format!("probing {}", path.display()))?;

    // Clone codec params before taking a mutable borrow for metadata
    let codec_params = probed
        .format
        .default_track()
        .ok_or_else(|| anyhow!("no audio track found in {}", path.display()))?
        .codec_params
        .clone();

    let codec = codec_type_to_str(codec_params.codec).to_string();
    let sample_rate_hz = codec_params.sample_rate;
    let channels = codec_params.channels.map(|c| c.count() as u8);
    let bits_per_sample = codec_params
        .bits_per_sample
        .or(codec_params.bits_per_coded_sample);

    let duration_secs = codec_params
        .n_frames
        .zip(codec_params.sample_rate)
        .map(|(frames, rate)| frames as f64 / rate as f64);

    let bitrate_kbps = duration_secs
        .filter(|&d| d > 0.0)
        .map(|d| ((file_size as f64 * 8.0) / d / 1000.0) as u32);

    // Collect tags — try format reader's embedded metadata first, then probe log.
    // Each scope is explicit to satisfy the borrow checker: format metadata requires
    // &mut probed.format, and ProbedMetadata::get() returns an owned Metadata<'_>
    // wrapper that must stay alive while we read from it.
    let tags = {
        let from_format: Option<BTreeMap<String, String>> = {
            let meta = probed.format.metadata();
            meta.current().map(|rev| collect_tags(rev.tags()))
        };

        if let Some(t) = from_format.filter(|t| !t.is_empty()) {
            t
        } else if let Some(meta) = probed.metadata.get() {
            if let Some(rev) = meta.current() {
                collect_tags(rev.tags())
            } else {
                BTreeMap::new()
            }
        } else {
            BTreeMap::new()
        }
    };

    let container = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("unknown")
        .to_lowercase();

    Ok(AudioInfo {
        path: path.to_owned(),
        container,
        codec,
        sample_rate_hz,
        channels,
        bits_per_sample,
        duration_secs,
        bitrate_kbps,
        tags,
    })
}

fn collect_tags(
    tags: &[symphonia::core::meta::Tag],
) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for tag in tags {
        let key = tag
            .std_key
            .map(std_key_name)
            .unwrap_or_else(|| tag.key.to_lowercase());
        let value = match &tag.value {
            Value::String(s) => s.clone(),
            Value::Boolean(b) => b.to_string(),
            Value::UnsignedInt(n) => n.to_string(),
            Value::SignedInt(n) => n.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Binary(_) => continue, // skip binary blobs
            Value::Flag => "true".to_string(),
        };
        map.entry(key).or_insert(value);
    }
    map
}

fn std_key_name(key: StandardTagKey) -> String {
    match key {
        StandardTagKey::TrackTitle => "title",
        StandardTagKey::Artist => "artist",
        StandardTagKey::Album => "album",
        StandardTagKey::AlbumArtist => "album_artist",
        StandardTagKey::TrackNumber => "track",
        StandardTagKey::TrackTotal => "track_total",
        StandardTagKey::DiscNumber => "disc",
        StandardTagKey::DiscTotal => "disc_total",
        StandardTagKey::Date => "date",
        StandardTagKey::OriginalDate => "original_date",
        StandardTagKey::Genre => "genre",
        StandardTagKey::Comment => "comment",
        StandardTagKey::Composer => "composer",
        StandardTagKey::Lyrics => "lyrics",
        StandardTagKey::Label => "label",
        StandardTagKey::Bpm => "bpm",
        StandardTagKey::EncodedBy => "encoded_by",
        StandardTagKey::Encoder => "encoder",
        StandardTagKey::Copyright => "copyright",
        _ => return format!("{key:?}").to_lowercase(),
    }
    .to_string()
}

fn codec_type_to_str(codec: CodecType) -> &'static str {
    use symphonia::core::codecs::*;
    match codec {
        CODEC_TYPE_MP1 => "mp1",
        CODEC_TYPE_MP2 => "mp2",
        CODEC_TYPE_MP3 => "mp3",
        CODEC_TYPE_AAC => "aac",
        CODEC_TYPE_FLAC => "flac",
        CODEC_TYPE_VORBIS => "vorbis",
        CODEC_TYPE_OPUS => "opus",
        CODEC_TYPE_ALAC => "alac",
        CODEC_TYPE_PCM_S8 => "pcm_s8",
        CODEC_TYPE_PCM_U8 => "pcm_u8",
        CODEC_TYPE_PCM_S16LE => "pcm_s16le",
        CODEC_TYPE_PCM_S16BE => "pcm_s16be",
        CODEC_TYPE_PCM_U16LE => "pcm_u16le",
        CODEC_TYPE_PCM_U16BE => "pcm_u16be",
        CODEC_TYPE_PCM_S24LE => "pcm_s24le",
        CODEC_TYPE_PCM_S24BE => "pcm_s24be",
        CODEC_TYPE_PCM_S32LE => "pcm_s32le",
        CODEC_TYPE_PCM_S32BE => "pcm_s32be",
        CODEC_TYPE_PCM_F32LE => "pcm_f32le",
        CODEC_TYPE_PCM_F32BE => "pcm_f32be",
        CODEC_TYPE_PCM_F64LE => "pcm_f64le",
        CODEC_TYPE_PCM_F64BE => "pcm_f64be",
        CODEC_TYPE_NULL => "null",
        _ => "unknown",
    }
}
