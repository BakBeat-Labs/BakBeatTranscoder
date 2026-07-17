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

use crate::adapters::{ensure_parent, sha256_file, ArtifactInfo, EncoderAdapter};
use crate::binaries;
use crate::error::AdapterError;
use crate::graph::{ExecutionNode, MediaType};

pub struct FfmpegAdapter {
    binary: PathBuf,
}

impl FfmpegAdapter {
    pub fn detect() -> Option<Self> {
        binaries::find_ffmpeg().map(|p| Self { binary: p })
    }

    fn build_args(&self, node: &ExecutionNode) -> Result<Vec<String>, AdapterError> {
        let p = &node.params;
        let mut args: Vec<String> = Vec::new();

        args.push("-y".into());
        args.extend(["-i".into(), node.input_path.to_string_lossy().into_owned()]);

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
            if p.audio_codec != "wma" {
                args.extend([
                    "-map".into(),
                    "0:v?".into(),
                    "-c:v".into(),
                    "copy".into(),
                    "-disposition:v:0".into(),
                    "attached_pic".into(),
                ]);
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
                args.extend(["-codec:v".into(), audio_codec_to_ffmpeg(vcodec)?.into()]);
            }
            if let Some(vbr) = p.video_bitrate_kbps {
                args.extend(["-b:v".into(), format!("{vbr}k")]);
            }
            if let (Some(w), Some(h)) = (p.width, p.height) {
                args.extend(["-vf".into(), format!("scale={w}:{h}")]);
            }
            if let Some(fps) = p.frame_rate {
                args.extend(["-r".into(), fps.to_string()]);
            }
            if let Some(pf) = &p.pixel_format {
                args.extend(["-pix_fmt".into(), pf.clone()]);
            }
        }

        // ── Audio track args ──────────────────────────────────────────────────
        let ffmpeg_acodec = audio_codec_to_ffmpeg(&p.audio_codec)?;
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
            // Video
            "h264",
            "avc",
            "h265",
            "hevc",
            "mpeg4",
            "mpeg2",
            "vp8",
            "vp9",
            "av1",
        ]
    }

    fn is_available(&self) -> bool {
        self.binary.exists()
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

        let size_bytes = std::fs::metadata(&node.output_path)?.len();
        let sha256 = sha256_file(&node.output_path)?;

        Ok(ArtifactInfo {
            output_path: node.output_path.clone(),
            sha256,
            size_bytes,
            duration_ms: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EncodeParams;
    use std::collections::BTreeMap;
    use uuid::Uuid;

    fn audio_node(audio_codec: &str, container: &str) -> ExecutionNode {
        ExecutionNode {
            id: Uuid::new_v4(),
            sequence: 0,
            input_path: "/tmp/in.flac".into(),
            input_sha256: "deadbeef".to_string(),
            input_size_bytes: 0,
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
                video_codec: None,
                video_bitrate_kbps: None,
                width: None,
                height: None,
                frame_rate: None,
                pixel_format: None,
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
        let adapter = FfmpegAdapter {
            binary: "/usr/bin/ffmpeg".into(),
        };
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
        let adapter = FfmpegAdapter {
            binary: "/usr/bin/ffmpeg".into(),
        };
        let args = adapter.build_args(&audio_node("mp3", "mp3")).unwrap();
        assert_eq!(windows_containing(&args, "-id3v2_version"), Some("3"));
    }

    #[test]
    fn non_mp3_target_does_not_force_id3v2_version() {
        let adapter = FfmpegAdapter {
            binary: "/usr/bin/ffmpeg".into(),
        };
        let args = adapter.build_args(&audio_node("alac", "m4a")).unwrap();
        assert_eq!(windows_containing(&args, "-id3v2_version"), None);
    }

    #[test]
    fn always_maps_source_metadata() {
        let adapter = FfmpegAdapter {
            binary: "/usr/bin/ffmpeg".into(),
        };
        let args = adapter.build_args(&audio_node("mp3", "mp3")).unwrap();
        assert_eq!(windows_containing(&args, "-map_metadata"), Some("0"));
    }

    #[test]
    fn wma_target_uses_wmav2_and_skips_cover_art_mapping() {
        let adapter = FfmpegAdapter {
            binary: "/usr/bin/ffmpeg".into(),
        };
        let args = adapter.build_args(&audio_node("wma", "wma")).unwrap();

        assert_eq!(windows_containing(&args, "-codec:a"), Some("wmav2"));
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:a"));
        assert!(!args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:v?"));
        assert_eq!(windows_containing(&args, "-c:v"), None);
    }
}

/// Maps our codec strings to FFmpeg codec names.
/// Used for both audio codec:a and video codec:v arguments.
fn audio_codec_to_ffmpeg(codec: &str) -> Result<&'static str, AdapterError> {
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
        // Video
        "h264" | "avc" => Ok("libx264"),
        "h265" | "hevc" => Ok("libx265"),
        "mpeg4" => Ok("mpeg4"),
        "mpeg2" => Ok("mpeg2video"),
        "vp8" => Ok("libvpx"),
        "vp9" => Ok("libvpx-vp9"),
        "av1" => Ok("libaom-av1"),
        other => Err(AdapterError::UnsupportedCodec(other.to_string())),
    }
}
