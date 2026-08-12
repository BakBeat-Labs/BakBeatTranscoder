// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! FFmpeg encoder adapter.
//!
//! FFmpeg is an execution adapter only — it is called as an external subprocess.
//! It has no role in probing, metadata authority, or policy decisions.
//! Users provide their own FFmpeg installation; we do not bundle it.
//!
//! Licensing note: calling FFmpeg as a subprocess does not create a derivative
//! work under LGPL/GPL. Our MPL-2.0 code and FFmpeg remain separate programs.

use std::path::PathBuf;
use std::process::Command;

use tracing::{debug, trace};

use crate::adapters::{ensure_parent, probe_output, ArtifactInfo, EncoderAdapter};
use crate::binaries;
use crate::error::AdapterError;
use crate::graph::{ExecutionNode, MediaType};

pub struct FfmpegAdapter {
    binary: PathBuf,
    /// Whether the active binary's `-encoders` output lists `libxvid`.
    /// Not every FFmpeg build enables optional GPL encoders like Xvid, so
    /// this is checked at runtime rather than assumed from the codec map.
    has_libxvid: bool,
}

impl FfmpegAdapter {
    pub fn detect() -> Option<Self> {
        binaries::find_ffmpeg().map(|p| {
            let has_libxvid = detect_libxvid(&p);
            Self {
                binary: p,
                has_libxvid,
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn for_testing(has_libxvid: bool) -> Self {
        Self {
            binary: "/usr/bin/ffmpeg".into(),
            has_libxvid,
        }
    }

    fn build_args(&self, node: &ExecutionNode) -> Result<Vec<String>, AdapterError> {
        let p = &node.params;
        let mut args: Vec<String> = Vec::new();

        args.push("-y".into());
        if let Some(hwaccel) = &p.hwaccel {
            args.extend(["-hwaccel".into(), hwaccel.clone()]);
        }
        args.extend(["-i".into(), node.input_path.to_string_lossy().into_owned()]);

        if p.media_type == MediaType::Video {
            args.extend([
                "-map".into(),
                "0:v:0".into(),
                "-map".into(),
                "0:a:0?".into(),
            ]);
            if let Some(poster_path) = &p.poster_artwork_path {
                args.extend(["-i".into(), poster_path.to_string_lossy().into_owned()]);
                args.extend([
                    "-map".into(),
                    "1:v:0".into(),
                ]);
            }
            args.extend(["-sn".into(), "-dn".into(), "-map_chapters".into(), "-1".into()]);
        }

        // Audio-only output: map the audio track plus any embedded cover art
        // (FLAC PICTURE block, MP4 covr/attached_pic, ID3 APIC all surface to
        // ffmpeg as a video stream). `0:v?` is optional so files without art
        // are unaffected. `-c:v copy` carries the image bytes through as-is;
        // the disposition flag marks it as the front-cover attached picture
        // in the output container (ID3 APIC for MP3, covr atom for MP4/M4A).
        if p.media_type == MediaType::Audio {
            args.extend(["-map".into(), "0:a".into()]);

            // ASF/WMA support for embedded artwork is inconsistent across old
            // players and can make ffmpeg reject otherwise valid transcodes.
            if p.preserve_artwork && p.audio_codec != "wma" {
                args.extend([
                    "-map".into(),
                    "0:v?".into(),
                    "-c:v".into(),
                    "copy".into(),
                    "-disposition:v:0".into(),
                    "attached_pic".into(),
                ]);
            } else {
                args.extend(["-vn".into(), "-sn".into(), "-dn".into()]);
            }
        }

        args.extend(["-map_metadata".into(), "0".into()]);

        // ID3v2.3 is the most broadly compatible tag version for legacy MSC
        // device players; ffmpeg defaults to 2.4 which some older firmwares
        // can't read. `bbt probe` (Symphonia) reads either version fine.
        if p.audio_codec == "mp3" {
            args.extend(["-id3v2_version".into(), "3".into()]);
        }

        // ── Video stream args (video encodes only) ────────────────────────────
        if p.media_type == MediaType::Video {
            if let Some(vcodec) = &p.video_codec {
                args.extend(["-c:v:0".into(), codec_to_ffmpeg(vcodec)?.into()]);
            }
            if let Some(vbr) = p.video_bitrate_kbps {
                args.extend(["-b:v:0".into(), format!("{vbr}k")]);
            }
            if let Some(filter) = &p.video_filter {
                args.extend(["-filter:v:0".into(), filter.clone()]);
            } else if let (Some(w), Some(h)) = (p.width, p.height) {
                args.extend(["-filter:v:0".into(), format!("scale={w}:{h}")]);
            }
            if let Some(fps) = p.frame_rate {
                args.extend(["-r:v:0".into(), fps.to_string()]);
            }
            if let Some(pf) = &p.pixel_format {
                args.extend(["-pix_fmt:v:0".into(), pf.clone()]);
            }
            if let Some(profile) = &p.video_profile {
                args.extend(["-profile:v:0".into(), profile.clone()]);
            }
            if let Some(level) = &p.video_level {
                args.extend(["-level:v:0".into(), level.clone()]);
            }
        }

        // ── Audio track args ──────────────────────────────────────────────────
        let ffmpeg_acodec = codec_to_ffmpeg(&p.audio_codec)?;
        args.extend(["-codec:a".into(), ffmpeg_acodec.into()]);

        if let Some(kbps) = p.audio_bitrate_kbps {
            args.extend(["-b:a".into(), format!("{kbps}k")]);

            if p.cbr {
                match p.audio_codec.as_str() {
                    "mp3" => {
                        args.extend(["-reservoir".into(), "0".into()]);
                    }
                    "vorbis" => {
                        args.extend([
                            "-minrate".into(),
                            format!("{kbps}k"),
                            "-maxrate".into(),
                            format!("{kbps}k"),
                        ]);
                    }
                    "opus" => {
                        args.extend(["-vbr".into(), "off".into()]);
                    }
                    _ => {}
                }
            }
        }

        args.extend(["-ar".into(), p.sample_rate_hz.to_string()]);
        args.extend(["-ac".into(), p.channels.to_string()]);

        if let Some(block_size) = p.audio_block_size {
            args.extend(["-block_size".into(), block_size.to_string()]);
        }

        // Strip iTunSMPB trailing padding so output frame count matches afconvert.
        // `atrim=end_sample=N` sees the post-start_pts stream (priming already removed).
        // `asetpts=PTS-STARTPTS` resets timestamps to start at 0 after the trim.
        if let Some(trim) = &p.gapless_trim {
            args.extend([
                "-af".into(),
                format!(
                    "atrim=end_sample={},asetpts=PTS-STARTPTS",
                    trim.output_frames
                ),
            ]);
        }

        for (k, v) in &p.extra {
            args.push(format!("-{k}"));
            if !v.is_empty() {
                args.push(v.clone());
            }
        }

        if p.media_type == MediaType::Video {
            args.extend(["-f".into(), p.container.clone()]);
            if let Some(movflags) = &p.movflags {
                args.extend(["-movflags".into(), movflags.clone()]);
            }
            if p.poster_artwork_path.is_some() {
                args.extend([
                    "-c:v:1".into(),
                    "copy".into(),
                    "-disposition:v:1".into(),
                    "attached_pic".into(),
                ]);
            }
        }

        args.push(node.output_path.to_string_lossy().into_owned());

        Ok(args)
    }
}

impl EncoderAdapter for FfmpegAdapter {
    fn supported_output_codecs(&self) -> &[&str] {
        &[
            // Audio
            "mp3",
            "aac",
            "flac",
            "vorbis",
            "opus",
            "wma",
            "alac",
            "pcm_s16le",
            "pcm_s24le",
            "pcm_s32le",
            "pcm_f32le",
            "wav",
            "adpcm_ima_amv",
            // Video
            "h264",
            "avc",
            "h264_avc",
            "h265",
            "hevc",
            "mpeg4",
            "mpeg4_simple_profile",
            "mpeg2",
            "xvid",
            "avi_motion_jpeg",
            "mjpeg",
            "vp8",
            "vp9",
            "av1",
        ]
    }

    fn is_available(&self) -> bool {
        self.binary.exists()
    }

    fn is_codec_available(&self, codec: &str) -> bool {
        if codec == "xvid" {
            return self.has_libxvid;
        }
        self.supported_output_codecs().contains(&codec)
    }

    fn encode(&self, node: &ExecutionNode) -> Result<ArtifactInfo, AdapterError> {
        ensure_parent(&node.output_path)?;

        let args = self.build_args(node)?;
        trace!(binary = ?self.binary, ?args, "running ffmpeg");

        let output = Command::new(&self.binary)
            .args(&args)
            .output()
            .map_err(AdapterError::Io)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            debug!(stderr = %stderr, "ffmpeg failed");
            return Err(AdapterError::EncodeFailed {
                path: node.input_path.clone(),
                stderr,
            });
        }

        probe_output(&node.output_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EncodeParams;
    use std::collections::BTreeMap;
    use uuid::Uuid;

    fn audio_node(audio_codec: &str, container: &str) -> ExecutionNode {
        audio_node_with_artwork(audio_codec, container, true)
    }

    fn audio_node_with_artwork(
        audio_codec: &str,
        container: &str,
        preserve_artwork: bool,
    ) -> ExecutionNode {
        ExecutionNode {
            id: Uuid::new_v4(),
            sequence: 0,
            input_path: "/tmp/in.flac".into(),
            input: crate::graph::SourceFingerprint {
                size_bytes: 0,
                modified_at: chrono::Utc::now(),
                duration_secs: None,
                codec_summary: "test".to_string(),
            },
            output_path: format!("/tmp/out.{container}").into(),
            adapter: "ffmpeg".to_string(),
            params: EncodeParams {
                media_type: MediaType::Audio,
                container: container.to_string(),
                extension: container.to_string(),
                cbr: true,
                audio_codec: audio_codec.to_string(),
                audio_bitrate_kbps: Some(128),
                sample_rate_hz: 44100,
                channels: 2,
                preserve_artwork,
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
                gapless_trim: None,
                extra: BTreeMap::new(),
            },
        }
    }

    fn gpx_mt861b_video_node() -> ExecutionNode {
        ExecutionNode {
            id: Uuid::new_v4(),
            sequence: 0,
            input_path: "/tmp/in.mp4".into(),
            input: crate::graph::SourceFingerprint {
                size_bytes: 0,
                modified_at: chrono::Utc::now(),
                duration_secs: None,
                codec_summary: "test".to_string(),
            },
            output_path: "/tmp/out.avi".into(),
            adapter: "ffmpeg".to_string(),
            params: EncodeParams {
                media_type: MediaType::Video,
                container: "avi".to_string(),
                extension: "avi".to_string(),
                cbr: true,
                audio_codec: "mp3".to_string(),
                audio_bitrate_kbps: Some(64),
                sample_rate_hz: 44100,
                channels: 2,
                preserve_artwork: true,
                video_codec: Some("xvid".to_string()),
                video_bitrate_kbps: Some(256),
                width: Some(320),
                height: Some(240),
                frame_rate: Some(20.0),
                pixel_format: None,
                video_filter: None,
                video_profile: None,
                video_level: None,
                poster_artwork_path: None,
                hwaccel: None,
                movflags: None,
                audio_block_size: None,
                gapless_trim: None,
                extra: BTreeMap::new(),
            },
        }
    }

    fn windows_containing<'a>(args: &'a [String], first: &str) -> Option<&'a str> {
        args.windows(2)
            .find(|w| w[0] == first)
            .map(|w| w[1].as_str())
    }

    #[test]
    fn audio_transcode_maps_audio_and_optional_cover_art() {
        let adapter = FfmpegAdapter::for_testing(false);
        let args = adapter.build_args(&audio_node("mp3", "mp3")).unwrap();

        // Must map the audio track explicitly and the optional embedded-art
        // video stream — replacing the old blanket `-vn` that dropped cover art.
        assert!(
            !args.contains(&"-vn".to_string()),
            "must not blanket-strip video/art streams"
        );
        assert_eq!(windows_containing(&args, "-map"), Some("0:a"));
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:v?"));
        assert_eq!(windows_containing(&args, "-c:v"), Some("copy"));
        assert_eq!(
            windows_containing(&args, "-disposition:v:0"),
            Some("attached_pic")
        );
    }

    #[test]
    fn mp3_target_uses_id3v2_3_for_device_compatibility() {
        let adapter = FfmpegAdapter::for_testing(false);
        let args = adapter.build_args(&audio_node("mp3", "mp3")).unwrap();
        assert_eq!(windows_containing(&args, "-id3v2_version"), Some("3"));
    }

    #[test]
    fn audio_only_target_strips_artwork_and_side_streams() {
        let adapter = FfmpegAdapter::for_testing(false);
        let args = adapter
            .build_args(&audio_node_with_artwork("mp3", "mp3", false))
            .unwrap();

        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:a"));
        assert!(!args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:v?"));
        assert!(args.contains(&"-vn".to_string()));
        assert!(args.contains(&"-sn".to_string()));
        assert!(args.contains(&"-dn".to_string()));
    }

    #[test]
    fn non_mp3_target_does_not_force_id3v2_version() {
        let adapter = FfmpegAdapter::for_testing(false);
        let args = adapter.build_args(&audio_node("alac", "m4a")).unwrap();
        assert_eq!(windows_containing(&args, "-id3v2_version"), None);
    }

    #[test]
    fn always_maps_source_metadata() {
        let adapter = FfmpegAdapter::for_testing(false);
        let args = adapter.build_args(&audio_node("mp3", "mp3")).unwrap();
        assert_eq!(windows_containing(&args, "-map_metadata"), Some("0"));
    }

    #[test]
    fn wma_target_uses_wmav2_and_skips_cover_art_mapping() {
        let adapter = FfmpegAdapter::for_testing(false);
        let args = adapter.build_args(&audio_node("wma", "wma")).unwrap();

        assert_eq!(windows_containing(&args, "-codec:a"), Some("wmav2"));
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:a"));
        assert!(!args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:v?"));
        assert_eq!(windows_containing(&args, "-c:v"), None);
    }

    #[test]
    fn gpx_mt861b_xvid_target_uses_real_libxvid_encoder() {
        let adapter = FfmpegAdapter::for_testing(true);
        let args = adapter.build_args(&gpx_mt861b_video_node()).unwrap();

        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:v:0"));
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:a:0?"));
        assert!(args.contains(&"-sn".to_string()));
        assert!(args.contains(&"-dn".to_string()));
        assert_eq!(windows_containing(&args, "-map_chapters"), Some("-1"));
        assert_eq!(windows_containing(&args, "-c:v:0"), Some("libxvid"));
        assert_eq!(
            windows_containing(&args, "-filter:v:0"),
            Some("scale=320:240")
        );
        assert_eq!(windows_containing(&args, "-r:v:0"), Some("20"));
        assert_eq!(windows_containing(&args, "-b:v:0"), Some("256k"));
        assert_eq!(windows_containing(&args, "-codec:a"), Some("libmp3lame"));
        assert_eq!(windows_containing(&args, "-b:a"), Some("64k"));
        assert_eq!(windows_containing(&args, "-ar"), Some("44100"));
        assert_eq!(windows_containing(&args, "-ac"), Some("2"));
    }

    #[test]
    fn video_target_can_attach_poster_and_legacy_ipod_muxer() {
        let adapter = FfmpegAdapter::for_testing(true);
        let mut node = gpx_mt861b_video_node();
        node.output_path = "/tmp/out.m4v".into();
        node.params.container = "ipod".to_string();
        node.params.extension = "m4v".to_string();
        node.params.video_codec = Some("h264_avc".to_string());
        node.params.video_filter = Some(
            "scale=320:240:force_original_aspect_ratio=decrease:force_divisible_by=16".to_string(),
        );
        node.params.video_profile = Some("baseline".to_string());
        node.params.video_level = Some("3.0".to_string());
        node.params.pixel_format = Some("yuv420p".to_string());
        node.params.poster_artwork_path = Some("/tmp/poster.jpg".into());
        node.params.hwaccel = Some("videotoolbox".to_string());
        node.params.movflags = Some("+faststart".to_string());

        let args = adapter.build_args(&node).unwrap();

        assert_eq!(windows_containing(&args, "-hwaccel"), Some("videotoolbox"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-i" && w[1] == "/tmp/poster.jpg"));
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:v:0"));
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:a:0?"));
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "1:v:0"));
        assert!(args.contains(&"-sn".to_string()));
        assert!(args.contains(&"-dn".to_string()));
        assert_eq!(windows_containing(&args, "-map_chapters"), Some("-1"));
        assert_eq!(windows_containing(&args, "-c:v:0"), Some("libx264"));
        assert_eq!(windows_containing(&args, "-profile:v:0"), Some("baseline"));
        assert_eq!(windows_containing(&args, "-level:v:0"), Some("3.0"));
        assert_eq!(windows_containing(&args, "-pix_fmt:v:0"), Some("yuv420p"));
        assert_eq!(windows_containing(&args, "-f"), Some("ipod"));
        assert_eq!(windows_containing(&args, "-movflags"), Some("+faststart"));
        assert_eq!(windows_containing(&args, "-c:v:1"), Some("copy"));
        assert_eq!(
            windows_containing(&args, "-disposition:v:1"),
            Some("attached_pic")
        );
    }

    #[test]
    fn is_codec_available_gates_xvid_on_runtime_libxvid_detection() {
        let without_libxvid = FfmpegAdapter::for_testing(false);
        assert!(!without_libxvid.is_codec_available("xvid"));

        let with_libxvid = FfmpegAdapter::for_testing(true);
        assert!(with_libxvid.is_codec_available("xvid"));

        // Unrelated codecs aren't affected by the libxvid flag either way.
        assert!(without_libxvid.is_codec_available("mp3"));
        assert!(with_libxvid.is_codec_available("mp3"));
    }

    #[test]
    fn detect_libxvid_true_when_encoders_list_contains_libxvid() {
        let sample = "\
Encoders:
 V..... = Video
 A..... = Audio
 -------
 V....D mpeg4                MPEG-4 part 2
 V....D libxvid              libxvidcore MPEG-4 part 2 (codec mpeg4)
 A....D libmp3lame           libmp3lame MP3 (MPEG audio layer 3)
";
        assert!(encoders_list_has_libxvid(sample));
    }

    #[test]
    fn detect_libxvid_false_when_encoders_list_omits_libxvid() {
        let sample = "\
Encoders:
 V..... = Video
 A..... = Audio
 -------
 V....D mpeg4                MPEG-4 part 2
 A....D libmp3lame           libmp3lame MP3 (MPEG audio layer 3)
";
        assert!(!encoders_list_has_libxvid(sample));
    }
}

/// Query the given FFmpeg binary's `-encoders` output for a `libxvid` line.
/// Xvid is an optional GPL encoder many FFmpeg builds omit; the codec map
/// alone can't tell us whether it's actually present.
fn detect_libxvid(binary: &std::path::Path) -> bool {
    let output = match Command::new(binary).arg("-encoders").output() {
        Ok(output) => output,
        Err(_) => return false,
    };
    encoders_list_has_libxvid(&String::from_utf8_lossy(&output.stdout))
}

/// Parses `ffmpeg -encoders` output, checking for a `libxvid` entry.
/// Each encoder line is `<6-char flags> <name> <description>`.
fn encoders_list_has_libxvid(stdout: &str) -> bool {
    stdout
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some("libxvid"))
}

/// Maps our caller-facing codec strings to FFmpeg codec names.
fn codec_to_ffmpeg(codec: &str) -> Result<&'static str, AdapterError> {
    match codec {
        // Audio
        "mp3" => Ok("libmp3lame"),
        "aac" => Ok("aac"),
        "flac" => Ok("flac"),
        "vorbis" => Ok("libvorbis"),
        "opus" => Ok("libopus"),
        "wma" => Ok("wmav2"),
        "alac" => Ok("alac"),
        "pcm_s16le" => Ok("pcm_s16le"),
        "pcm_s16be" => Ok("pcm_s16be"),
        "pcm_s24le" => Ok("pcm_s24le"),
        "pcm_s32le" => Ok("pcm_s32le"),
        "pcm_f32le" => Ok("pcm_f32le"),
        "wav" => Ok("pcm_s16le"),
        "adpcm_ima_amv" => Ok("adpcm_ima_amv"),
        // Video
        "h264" | "avc" | "h264_avc" => Ok("libx264"),
        "h265" | "hevc" => Ok("libx265"),
        "mpeg4" | "mpeg4_simple_profile" => Ok("mpeg4"),
        "mpeg2" => Ok("mpeg2video"),
        "xvid" => Ok("libxvid"),
        "avi_motion_jpeg" | "mjpeg" => Ok("mjpeg"),
        "vp8" => Ok("libvpx"),
        "vp9" => Ok("libvpx-vp9"),
        "av1" => Ok("libaom-av1"),
        other => Err(AdapterError::UnsupportedCodec(other.to_string())),
    }
}
