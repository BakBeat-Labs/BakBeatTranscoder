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
use crate::mp4_aac::{self, AudiobookFacts};
use crate::spec::TranscodeSpec;

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
    /// Total frame count from the container (includes encoder priming + trailing padding for AAC).
    /// Used for gapless trim calculations when iTunSMPB total_pcm_samples is absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n_frames: Option<u64>,
    /// Approximate bitrate in kbps estimated from file size and duration
    pub bitrate_kbps: Option<u32>,
    /// Whether the source carries an embedded picture (cover art, FLAC PICTURE
    /// block, MP4 covr / attached_pic, ID3 APIC). Used to decide whether the
    /// transcode graph needs to preserve a video/image stream.
    pub has_artwork: bool,
    /// AAC profile when known (`LC`, `HE-AAC`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Encoder delay in PCM samples from the MP4 edit list (or equivalent).
    /// `null` when this file is not ISO-BMFF or priming could not be read —
    /// BakBeat must not treat `null` as Shuffle-ready.
    pub priming_samples: Option<u64>,
    #[serde(default)]
    pub has_chapters: bool,
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
    /// Whether this file already matches the requested target format.
    /// Used by the planner to skip no-op transcodes.
    pub fn already_matches_spec(&self, spec: &TranscodeSpec) -> bool {
        use crate::graph::MediaType;
        use crate::mp4_aac::{containers_match_audiobook, is_zero_priming_aac_m4b_target};
        match (self, &spec.media_type) {
            (MediaInfo::Audio(a), MediaType::Audio) => {
                if !a.codec.eq_ignore_ascii_case(&spec.audio_codec)
                    || spec
                        .sample_rate_hz
                        .is_some_and(|rate| a.sample_rate_hz != Some(rate))
                    || spec
                        .channels
                        .is_some_and(|channels| a.channels != Some(channels))
                    || spec
                        .audio_bitrate_kbps
                        .is_some_and(|rate| a.bitrate_kbps != Some(rate))
                    || (spec.audio_bitrate_kbps.is_some() && requires_unverified_cbr(spec))
                    || spec.audio_block_size.is_some()
                    || spec.movflags.is_some()
                    || (a.has_artwork && !spec.preserve_artwork)
                {
                    return false;
                }
                if is_zero_priming_aac_m4b_target(
                    true,
                    &spec.audio_codec,
                    &spec.container,
                    &spec.extension,
                    spec.aac_priming,
                ) {
                    let profile_ok = a
                        .profile
                        .as_deref()
                        .is_some_and(|p| p.eq_ignore_ascii_case("LC"));
                    let priming_ok = a.priming_samples == Some(0);
                    return a.container.eq_ignore_ascii_case(&spec.extension)
                        && containers_match_audiobook(
                            &a.container,
                            &spec.container,
                            &spec.extension,
                        )
                        && profile_ok
                        && priming_ok;
                }
                a.container.eq_ignore_ascii_case(&spec.container)
                    && a.container.eq_ignore_ascii_case(&spec.extension)
                    && spec.aac_priming.map_or(true, |samples| {
                        a.priming_samples == Some(u64::from(samples))
                    })
            }
            (MediaInfo::Video(v), MediaType::Video) => {
                let Some(audio) = v.audio_streams.first() else {
                    return false;
                };
                let Some(video) = v.video_streams.first() else {
                    return false;
                };
                v.container.eq_ignore_ascii_case(&spec.container)
                    && v.path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case(&spec.extension))
                    && audio.codec.eq_ignore_ascii_case(&spec.audio_codec)
                    && spec
                        .sample_rate_hz
                        .map_or(true, |rate| audio.sample_rate_hz == Some(rate))
                    && spec
                        .channels
                        .map_or(true, |channels| audio.channels == Some(channels))
                    && spec
                        .audio_bitrate_kbps
                        .map_or(true, |rate| audio.bitrate_kbps == Some(rate))
                    && !(spec.audio_bitrate_kbps.is_some() && requires_unverified_cbr(spec))
                    && spec
                        .video_codec
                        .as_deref()
                        .map_or(true, |codec| video.codec.eq_ignore_ascii_case(codec))
                    && spec
                        .video_bitrate_kbps
                        .map_or(true, |rate| video.bitrate_kbps == Some(rate))
                    && spec.width.map_or(true, |width| video.width == width)
                    && spec.height.map_or(true, |height| video.height == height)
                    && spec
                        .frame_rate
                        .map_or(true, |rate| video.frame_rate == Some(rate))
                    && spec
                        .pixel_format
                        .as_deref()
                        .map_or(true, |format| video.pixel_format.as_deref() == Some(format))
                    && spec.video_filter.is_none()
                    && spec.video_profile.is_none()
                    && spec.video_level.is_none()
                    && spec.poster_artwork_path.is_none()
                    && spec.movflags.is_none()
                    && spec.audio_block_size.is_none()
                    && spec.aac_priming.is_none()
            }
            // Media type mismatch — never a pass-through
            _ => false,
        }
    }

    pub fn duration_secs(&self) -> Option<f64> {
        match self {
            MediaInfo::Audio(a) => a.duration_secs,
            MediaInfo::Video(v) => v.duration_secs,
        }
    }

    pub fn container(&self) -> &str {
        match self {
            MediaInfo::Audio(a) => &a.container,
            MediaInfo::Video(v) => &v.container,
        }
    }

    /// Primary video dimensions, if this is a video file with a video stream.
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        match self {
            MediaInfo::Audio(_) => None,
            MediaInfo::Video(v) => v.video_streams.first().map(|s| (s.width, s.height)),
        }
    }

    /// Short, cheap codec fingerprint — e.g. `"aac"` or `"hevc+aac"`.
    /// Used for shape/staleness comparisons, not content identity.
    pub fn codec_summary(&self) -> String {
        match self {
            MediaInfo::Audio(a) => a.codec.clone(),
            MediaInfo::Video(v) => {
                let vcodec = v
                    .video_streams
                    .first()
                    .map(|s| s.codec.as_str())
                    .unwrap_or("none");
                let acodec = v
                    .audio_streams
                    .first()
                    .map(|s| s.codec.as_str())
                    .unwrap_or("none");
                format!("{vcodec}+{acodec}")
            }
        }
    }
}

fn requires_unverified_cbr(spec: &TranscodeSpec) -> bool {
    // The probe only reports average bitrate. These encoders also apply
    // CBR-specific options when a bitrate is requested.
    spec.cbr && matches!(spec.audio_codec.as_str(), "mp3" | "vorbis" | "opus")
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

    let fmt_opts = FormatOptions {
        enable_gapless: false,
        ..Default::default()
    };
    let meta_opts: MetadataOptions = Default::default();

    let mut probed = symphonia::default::get_probe()
        .format(&hint, mss, &fmt_opts, &meta_opts)
        .with_context(|| format!("probing {}", path.display()))?;

    // Clone codec params before taking mutable borrow for metadata
    let track = probed
        .format
        .default_track()
        .ok_or_else(|| anyhow!("no audio track found in {}", path.display()))?;
    let track_index = track.id;
    let codec_params = track.codec_params.clone();

    let mut codec = codec_type_to_str(codec_params.codec);
    let mut sample_rate_hz = codec_params.sample_rate;
    let mut channels = codec_params.channels.map(|c| c.count() as u8);
    let mut bits_per_sample = codec_params
        .bits_per_sample
        .or(codec_params.bits_per_coded_sample);
    let mut n_frames = codec_params.n_frames;
    let mut duration_secs = n_frames
        .zip(codec_params.sample_rate)
        .filter(|&(_, rate)| rate > 0)
        .map(|(frames, rate)| frames as f64 / rate as f64);

    let mut has_artwork = false;
    let tags = {
        let from_format: Option<BTreeMap<String, String>> = {
            let meta = probed.format.metadata();
            if let Some(rev) = meta.current() {
                has_artwork |= !rev.visuals().is_empty();
                Some(collect_tags(rev.tags()))
            } else {
                None
            }
        };
        if let Some(t) = from_format.filter(|t| !t.is_empty()) {
            t
        } else if let Some(meta) = probed.metadata.get() {
            if let Some(rev) = meta.current() {
                has_artwork |= !rev.visuals().is_empty();
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

    let mut profile = None;
    let mut priming_samples = None;
    let mut has_chapters = false;
    if mp4_aac::is_iso_bmff_audio_ext(&container) {
        match mp4_aac::inspect_mp4(path, track_index) {
            Ok(mp4) if mp4.parsed => {
                profile = mp4.profile;
                // Missing elst is unknown, not zero. BakBeat must not copy on null.
                priming_samples = mp4.priming_samples;
                has_chapters = mp4.has_chapters;
                has_artwork |= mp4.has_artwork;
                if let Some(alac) = mp4.alac {
                    // Symphonia may not identify ALAC when the cookie is in a wave box.
                    codec = "alac";
                    sample_rate_hz = alac.sample_rate_hz;
                    bits_per_sample = alac.bits_per_sample;
                    channels = alac.channels;
                } else {
                    channels = channels.or(mp4.channels);
                    sample_rate_hz = sample_rate_hz.or(mp4.sample_rate_hz);
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(path = ?path, error = %e, "mp4 inspect failed; priming unknown");
            }
        }
    }

    if codec == "alac" {
        // For MP4, Symphonia's n_frames is mdhd duration in media ticks.
        let timing = n_frames.zip(codec_params.time_base);
        duration_secs = timing
            .map(|(ticks, base)| ticks as f64 * f64::from(base.numer) / f64::from(base.denom));
        n_frames = timing
            .zip(sample_rate_hz)
            .and_then(|((ticks, base), rate)| {
                let samples = u128::from(ticks) * u128::from(base.numer) * u128::from(rate);
                let scale = u128::from(base.denom);
                u64::try_from((samples + scale / 2) / scale).ok()
            });
    }
    let bitrate_kbps = duration_secs
        .filter(|&d| d > 0.0)
        .map(|d| ((file_size as f64 * 8.0) / d / 1000.0) as u32);

    Ok(AudioInfo {
        path: path.to_owned(),
        container,
        codec: codec.to_owned(),
        sample_rate_hz,
        channels,
        bits_per_sample,
        duration_secs,
        n_frames,
        bitrate_kbps,
        has_artwork,
        profile,
        priming_samples,
        has_chapters,
        tags,
    })
}

/// Structured audiobook probe for BakBeat copy-vs-create. Not a raw ffprobe dump.
pub fn probe_audiobook_facts(path: &Path) -> Result<AudiobookFacts> {
    if !path.exists() {
        return Err(anyhow!("file not found: {}", path.display()));
    }
    if mp4_aac::is_protected_audiobook_path(path) {
        return Err(anyhow!(
            "refusing FairPlay/Audible DRM source: {}",
            path.display()
        ));
    }

    let audio = probe_audio_file(path)?;
    Ok(AudiobookFacts {
        codec: audio.codec,
        profile: audio.profile,
        sample_rate_hz: audio.sample_rate_hz,
        channels: audio.channels,
        bitrate_kbps: audio.bitrate_kbps,
        priming_samples: audio.priming_samples,
        has_chapters: Some(audio.has_chapters),
        has_artwork: Some(audio.has_artwork),
        container: audio.container,
    })
}

/// Fail unless an AAC-LC ipod/m4b artifact matches the requested rate/channels
/// with zero encoder priming.
pub fn verify_zero_priming_aac_m4b(path: &Path, sample_rate_hz: u32, channels: u8) -> Result<()> {
    let facts = probe_audiobook_facts(path)?;
    if !facts.codec.eq_ignore_ascii_case("aac") {
        return Err(anyhow!(
            "shuffle m4b verify: expected AAC, found {}",
            facts.codec
        ));
    }
    match facts.profile.as_deref() {
        Some("LC") => {}
        other => {
            return Err(anyhow!(
                "shuffle m4b verify: expected AAC-LC, found profile {other:?}"
            ));
        }
    }
    if facts.sample_rate_hz != Some(sample_rate_hz) {
        return Err(anyhow!(
            "shuffle m4b verify: expected {sample_rate_hz} Hz, found {:?}",
            facts.sample_rate_hz
        ));
    }
    if facts.channels != Some(channels) {
        return Err(anyhow!(
            "shuffle m4b verify: expected {channels} channels, found {:?}",
            facts.channels
        ));
    }
    match facts.priming_samples {
        Some(0) => {}
        Some(n) => {
            return Err(anyhow!(
                "shuffle m4b verify: expected 0 priming samples, found {n}"
            ));
        }
        None => {
            return Err(anyhow!(
                "shuffle m4b verify: priming_samples unknown; refusing success"
            ));
        }
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext != "m4b" && facts.container != "ipod" && facts.container != "m4b" {
        return Err(anyhow!(
            "shuffle m4b verify: expected .m4b / ipod, found ext={ext} container={}",
            facts.container
        ));
    }
    Ok(())
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
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
            &path.to_string_lossy(),
        ])
        .output()
        .context("running ffprobe")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "ffprobe failed for {}: {}",
            path.display(),
            stderr.trim()
        ));
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

    let duration_secs = fmt["duration"].as_str().and_then(|s| s.parse::<f64>().ok());

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
    if den == 0.0 {
        None
    } else {
        Some(num / den)
    }
}

// ── Routing ───────────────────────────────────────────────────────────────────

fn is_video_extension(path: &Path) -> bool {
    const VIDEO_EXTS: &[&str] = &[
        "mp4", "m4v", "mov", "avi", "mkv", "wmv", "flv", "webm", "mpeg", "mpg", "m2v", "ts", "mts",
        "m2ts", "3gp", "3g2", "ogv", "vob", "f4v",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::MediaType;
    use crate::spec::TranscodeSpec;

    fn shuffle_spec() -> TranscodeSpec {
        TranscodeSpec {
            media_type: MediaType::Audio,
            container: "ipod".into(),
            extension: "m4b".into(),
            cbr: true,
            audio_codec: "aac".into(),
            audio_bitrate_kbps: Some(128),
            sample_rate_hz: Some(44100),
            channels: Some(2),
            video_codec: None,
            video_bitrate_kbps: None,
            width: None,
            height: None,
            frame_rate: None,
            pixel_format: None,
            video_filter: None,
            video_profile: None,
            video_level: None,
            poster_artwork_path: None,
            hwaccel: None,
            movflags: None,
            audio_block_size: None,
            preserve_artwork: false,
            aac_priming: None,
        }
    }

    fn audio(
        container: &str,
        codec: &str,
        rate: u32,
        channels: u8,
        profile: Option<&str>,
        priming: Option<u64>,
    ) -> MediaInfo {
        MediaInfo::Audio(AudioInfo {
            path: format!("/tmp/x.{container}").into(),
            container: container.into(),
            codec: codec.into(),
            sample_rate_hz: Some(rate),
            channels: Some(channels),
            bits_per_sample: None,
            duration_secs: Some(1.0),
            n_frames: None,
            bitrate_kbps: Some(128),
            has_artwork: false,
            profile: profile.map(str::to_string),
            priming_samples: priming,
            has_chapters: false,
            tags: BTreeMap::new(),
        })
    }

    #[test]
    fn shuffle_skip_requires_lc_44100_stereo_zero_priming() {
        let spec = shuffle_spec();
        assert!(audio("m4b", "aac", 44100, 2, Some("LC"), Some(0)).already_matches_spec(&spec));
        assert!(!audio("m4b", "aac", 22050, 2, Some("LC"), Some(0)).already_matches_spec(&spec));
        assert!(!audio("m4b", "aac", 44100, 2, Some("LC"), Some(1024)).already_matches_spec(&spec));
        assert!(!audio("m4b", "aac", 44100, 2, Some("HE-AAC"), Some(0)).already_matches_spec(&spec));
        assert!(!audio("m4b", "aac", 44100, 2, None, Some(0)).already_matches_spec(&spec));
        assert!(!audio("m4a", "aac", 44100, 2, Some("LC"), Some(0)).already_matches_spec(&spec));
    }

    #[test]
    fn generic_audio_skip_requires_requested_rate_channels_and_bitrate() {
        let spec = TranscodeSpec {
            container: "mp3".into(),
            extension: "mp3".into(),
            audio_codec: "mp3".into(),
            sample_rate_hz: Some(44100),
            channels: Some(2),
            cbr: false,
            preserve_artwork: true,
            ..shuffle_spec()
        };
        assert!(audio("mp3", "mp3", 44100, 2, None, None).already_matches_spec(&spec));
        assert!(!audio("mp3", "mp3", 22050, 2, None, None).already_matches_spec(&spec));
        assert!(!audio("mp3", "mp3", 44100, 1, None, None).already_matches_spec(&spec));
        assert!(!audio("mp3", "aac", 44100, 2, None, None).already_matches_spec(&spec));
        let different_bitrate = TranscodeSpec {
            audio_bitrate_kbps: Some(96),
            ..spec
        };
        assert!(!audio("mp3", "mp3", 44100, 2, None, None).already_matches_spec(&different_bitrate));
    }

    fn have_ffmpeg() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn verify_rejects_native_ffmpeg_aac_priming() {
        if !have_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let primed = dir.path().join("primed.m4b");
        assert!(Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=44100:duration=1",
                "-ac",
                "2",
                "-c:a",
                "aac",
                "-b:a",
                "128k",
                "-f",
                "ipod",
            ])
            .arg(&primed)
            .status()
            .unwrap()
            .success());
        let err = verify_zero_priming_aac_m4b(&primed, 44100, 2).unwrap_err();
        assert!(
            err.to_string().contains("priming"),
            "expected priming failure, got {err}"
        );
    }
}
