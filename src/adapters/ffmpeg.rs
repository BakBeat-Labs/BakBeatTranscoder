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

        // Strip video streams for audio-only output; preserve them for video encodes
        if p.media_type == MediaType::Audio {
            args.push("-vn".into());
        }

        args.extend(["-map_metadata".into(), "0".into()]);

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
                            "-minrate".into(), format!("{kbps}k"),
                            "-maxrate".into(), format!("{kbps}k"),
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
    fn name(&self) -> &str {
        "ffmpeg"
    }

    fn supported_output_codecs(&self) -> &[&str] {
        &[
            // Audio
            "mp3", "aac", "flac", "vorbis", "opus",
            "alac", "pcm_s16le", "pcm_s24le", "pcm_s32le", "pcm_f32le", "wav",
            // Video
            "h264", "avc", "h265", "hevc", "mpeg4", "mpeg2", "vp8", "vp9", "av1",
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

/// Maps our codec strings to FFmpeg codec names.
/// Used for both audio codec:a and video codec:v arguments.
fn audio_codec_to_ffmpeg(codec: &str) -> Result<&'static str, AdapterError> {
    match codec {
        // Audio
        "mp3"       => Ok("libmp3lame"),
        "aac"       => Ok("aac"),
        "flac"      => Ok("flac"),
        "vorbis"    => Ok("libvorbis"),
        "opus"      => Ok("libopus"),
        "alac"      => Ok("alac"),
        "pcm_s16le" => Ok("pcm_s16le"),
        "pcm_s16be" => Ok("pcm_s16be"),
        "pcm_s24le" => Ok("pcm_s24le"),
        "pcm_s32le" => Ok("pcm_s32le"),
        "pcm_f32le" => Ok("pcm_f32le"),
        "wav"       => Ok("pcm_s16le"),
        // Video
        "h264" | "avc"   => Ok("libx264"),
        "h265" | "hevc"  => Ok("libx265"),
        "mpeg4"          => Ok("mpeg4"),
        "mpeg2"          => Ok("mpeg2video"),
        "vp8"            => Ok("libvpx"),
        "vp9"            => Ok("libvpx-vp9"),
        "av1"            => Ok("libaom-av1"),
        other => Err(AdapterError::UnsupportedCodec(other.to_string())),
    }
}
