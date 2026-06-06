// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Media probing.
//!
//! Audio files: probed natively via Symphonia (MPL-2.0). Symphonia is our
//! metadata authority for audio — no subprocess, no FFmpeg.
//!
//! Video files: probed via ffprobe (ships with FFmpeg). ffprobe is called as
//! an external subprocess; its output is parsed from JSON. This keeps the
//! licensing boundary clean — we call a binary, we don't link a library.
//!
//! Entry point: `probe_media(path)` routes to the right prober automatically.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use symphonia::core::codecs::CodecType;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey, Value};
use symphonia::core::probe::Hint;

use crate::binaries;
use crate::profiles::DeviceProfile;

// ── Public types ──────────────────────────────────────────────────────────────

/// Result of probing any media file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "media_type", rename_all = "snake_case")]
pub enum MediaInfo {
    Audio(AudioInfo),
    Video(VideoInfo),
}

/// Probe result for an audio file (Symphonia-based).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioInfo {
    pub path: PathBuf,
    /// Container format from file extension (e.g. "flac", "mp3", "m4a")
    pub container: String,
    /// Codec string (e.g. "mp3", "aac", "flac", "vorbis", "alac", "pcm_s16le")
    pub codec: String,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u8>,
    pub bits_per_sample: Option<u32>,
    pub duration_secs: Option<f64>,
    /// Approximate bitrate in kbps estimated from file size and duration
    pub bitrate_kbps: Option<u32>,
    pub tags: BTreeMap<String, String>,
}

/// Probe result for a video file (ffprobe-based).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoInfo {
    pub path: PathBuf,
    /// Primary container format (e.g. "mp4", "mov", "mkv")
    pub container: String,
    pub duration_secs: Option<f64>,
    pub video_streams: Vec<VideoStream>,
    pub audio_streams: Vec<AudioStream>,
    pub tags: BTreeMap<String, String>,
}

/// A single video stream within a container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoStream {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub frame_rate: Option<f32>,
    pub bitrate_kbps: Option<u32>,
    pub pixel_format: Option<String>,
}

/// A single audio stream within a container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioStream {
    pub codec: String,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u8>,
    pub bitrate_kbps: Option<u32>,
}

// ── MediaInfo helpers ─────────────────────────────────────────────────────────

impl MediaInfo {
    pub fn path(&self) -> &Path {
        match self {
            Self::Audio(a) => &a.path,
            Self::Video(v) => &v.path,
        }
    }

    pub fn container(&self) -> &str {
        match self {
            Self::Audio(a) => &a.container,
            Self::Video(v) => &v.container,
        }
    }

    pub fn duration_secs(&self) -> Option<f64> {
        match self {
            Self::Audio(a) => a.duration_secs,
            Self::Video(v) => v.duration_secs,
        }
    }

    pub fn tags(&self) -> &BTreeMap<String, String> {
        match self {
            Self::Audio(a) => &a.tags,
            Self::Video(v) => &v.tags,
        }
    }

    /// Whether this file already matches a device profile's target format.
    /// Used by the planner to skip no-op transcodes.
    pub fn already_matches_profile(&self, profile: &DeviceProfile) -> bool {
        use crate::graph::MediaType;
        match (self, &profile.media_type) {
            (MediaInfo::Audio(a), MediaType::Audio) => {
                a.codec.eq_ignore_ascii_case(&profile.audio_codec)
                    && a.container.eq_ignore_ascii_case(&profile.container)
            }
            (MediaInfo::Video(v), MediaType::Video) => {
                let video_ok = profile
                    .video_codec
                    .as_deref()
                    .map(|vc| {
                        v.video_streams
                            .first()
                            .map(|s| s.codec.eq_ignore_ascii_case(vc))
                            .unwrap_or(false)
                    })
                    .unwrap_or(true);
                let audio_ok = v
                    .audio_streams
                    .first()
                    .map(|s| s.codec.eq_ignore_ascii_case(&profile.audio_codec))
                    .unwrap_or(false);
                let container_ok = v.container.eq_ignore_ascii_case(&profile.container);
                video_ok && audio_ok && container_ok
            }
            // Media type mismatch — never a pass-through
            _ => false,
        }
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Probe any media file, routing to Symphonia (audio) or ffprobe (video).
pub fn probe_media(path: &Path) -> Result<MediaInfo> {
    if is_video_extension(path) {
        probe_video_file(path).map(MediaInfo::Video)
    } else {
        probe_audio_file(path).map(MediaInfo::Audio)
    }
}

// ── Audio probing (Symphonia) ─────────────────────────────────────────────────

pub fn probe_audio_file(path: &Path) -> Result<AudioInfo> {
    if !path.exists() {
        return Err(anyhow!("file not found: {}", path.display()));
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

    let fmt_opts = FormatOptions { enable_gapless: false, ..Default::default() };
    let meta_opts: MetadataOptions = Default::default();

    let mut probed = symphonia::default::get_probe()
        .format(&hint, mss, &fmt_opts, &meta_opts)
        .with_context(|| format!("probing {}", path.display()))?;

    // Clone codec params before taking mutable borrow for metadata
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

// ── Video probing (ffprobe) ───────────────────────────────────────────────────

/// Probe a video file using ffprobe.
///
/// ffprobe ships with FFmpeg. It is called as an external subprocess and its
/// JSON output is parsed. This preserves the licensing boundary: we call a
/// binary, we do not link against FFmpeg's libraries.
pub fn probe_video_file(path: &Path) -> Result<VideoInfo> {
    if !path.exists() {
        return Err(anyhow!("file not found: {}", path.display()));
    }

    let ffprobe = binaries::find_ffprobe().ok_or_else(|| {
        anyhow!(
            "ffprobe not found — install FFmpeg (https://ffmpeg.org) or set \
             BBT_FFPROBE_PATH to the binary location"
        )
    })?;

    let output = Command::new(&ffprobe)
        .args([
            "-v", "quiet",
            "-print_format", "json",
            "-show_streams",
            "-show_format",
            &path.to_string_lossy(),
        ])
        .output()
        .context("running ffprobe")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("ffprobe failed for {}: {}", path.display(), stderr.trim()));
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing ffprobe JSON output")?;

    parse_ffprobe_output(path, &json)
}

fn parse_ffprobe_output(path: &Path, json: &serde_json::Value) -> Result<VideoInfo> {
    let streams = json["streams"]
        .as_array()
        .ok_or_else(|| anyhow!("ffprobe output has no streams array"))?;

    let fmt = &json["format"];

    // Use the first token of format_name (e.g. "mov,mp4,m4a,3gp" → "mov")
    let container = fmt["format_name"]
        .as_str()
        .unwrap_or("unknown")
        .split(',')
        .next()
        .unwrap_or("unknown")
        .to_string();

    let duration_secs = fmt["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok());

    let mut video_streams: Vec<VideoStream> = Vec::new();
    let mut audio_streams: Vec<AudioStream> = Vec::new();

    for s in streams {
        match s["codec_type"].as_str() {
            Some("video") => {
                video_streams.push(VideoStream {
                    codec: s["codec_name"].as_str().unwrap_or("unknown").to_string(),
                    width: s["width"].as_u64().unwrap_or(0) as u32,
                    height: s["height"].as_u64().unwrap_or(0) as u32,
                    frame_rate: parse_frame_rate(s["r_frame_rate"].as_str()),
                    bitrate_kbps: s["bit_rate"]
                        .as_str()
                        .and_then(|b| b.parse::<u64>().ok())
                        .map(|bps| (bps / 1000) as u32),
                    pixel_format: s["pix_fmt"].as_str().map(str::to_owned),
                });
            }
            Some("audio") => {
                audio_streams.push(AudioStream {
                    codec: s["codec_name"].as_str().unwrap_or("unknown").to_string(),
                    sample_rate_hz: s["sample_rate"]
                        .as_str()
                        .and_then(|r| r.parse::<u32>().ok()),
                    channels: s["channels"].as_u64().map(|c| c as u8),
                    bitrate_kbps: s["bit_rate"]
                        .as_str()
                        .and_then(|b| b.parse::<u64>().ok())
                        .map(|bps| (bps / 1000) as u32),
                });
            }
            _ => {}
        }
    }

    // Collect format-level tags (title, artist, etc.)
    let tags: BTreeMap<String, String> = fmt["tags"]
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.to_lowercase(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();

    Ok(VideoInfo {
        path: path.to_owned(),
        container,
        duration_secs,
        video_streams,
        audio_streams,
        tags,
    })
}

/// Parse an ffprobe frame-rate fraction string (e.g. "30000/1001", "25/1").
fn parse_frame_rate(s: Option<&str>) -> Option<f32> {
    let s = s?;
    let mut parts = s.splitn(2, '/');
    let num: f32 = parts.next()?.parse().ok()?;
    let den: f32 = parts.next().unwrap_or("1").parse().ok()?;
    if den == 0.0 { None } else { Some(num / den) }
}

// ── Routing ───────────────────────────────────────────────────────────────────

fn is_video_extension(path: &Path) -> bool {
    const VIDEO_EXTS: &[&str] = &[
        "mp4", "m4v", "mov", "avi", "mkv", "wmv", "flv", "webm",
        "mpeg", "mpg", "m2v", "ts", "mts", "m2ts", "3gp", "3g2",
        "ogv", "vob", "f4v",
    ];
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| VIDEO_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

// ── Symphonia helpers ─────────────────────────────────────────────────────────

fn collect_tags(tags: &[symphonia::core::meta::Tag]) -> BTreeMap<String, String> {
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
            Value::Binary(_) => continue,
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
