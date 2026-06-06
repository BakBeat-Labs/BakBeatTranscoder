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
use crate::error::AdapterError;
use crate::graph::ExecutionNode;

pub struct FfmpegAdapter {
    binary: PathBuf,
}

impl FfmpegAdapter {
    /// Locate ffmpeg in PATH and return an adapter, or None if not found.
    pub fn detect() -> Option<Self> {
        which::which("ffmpeg").ok().map(|p| Self { binary: p })
    }

    /// Build the ffmpeg argument list for the given node.
    fn build_args(&self, node: &ExecutionNode) -> Result<Vec<String>, AdapterError> {
        let p = &node.params;
        let mut args: Vec<String> = Vec::new();

        // Overwrite output without prompt
        args.push("-y".into());

        // Input
        args.extend(["-i".into(), node.input_path.to_string_lossy().into_owned()]);

        // No video streams (audio-only output)
        args.push("-vn".into());

        // Preserve all metadata from input
        args.extend(["-map_metadata".into(), "0".into()]);

        // Codec
        let ffmpeg_codec = codec_to_ffmpeg_name(&p.codec)?;
        args.extend(["-codec:a".into(), ffmpeg_codec.into()]);

        // Bitrate (for lossy codecs)
        if let Some(kbps) = p.bitrate_kbps {
            args.extend(["-b:a".into(), format!("{kbps}k")]);

            // CBR enforcement per codec
            if p.cbr {
                match p.codec.as_str() {
                    "mp3" => {
                        // libmp3lame: disable bit reservoir for strict CBR
                        args.extend(["-reservoir".into(), "0".into()]);
                    }
                    "vorbis" => {
                        // libvorbis: clamp min/max to force CBR
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

        // Sample rate
        args.extend(["-ar".into(), p.sample_rate_hz.to_string()]);

        // Channels
        args.extend(["-ac".into(), p.channels.to_string()]);

        // Codec-specific extra args from params
        for (k, v) in &p.extra {
            args.push(format!("-{k}"));
            if !v.is_empty() {
                args.push(v.clone());
            }
        }

        // Output path
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
            "mp3", "aac", "flac", "vorbis", "opus",
            "alac", "pcm_s16le", "pcm_s24le", "pcm_s32le",
            "pcm_f32le", "wav",
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
            // ffmpeg writes progress/info to stderr; capture it for error reporting
            .output()
            .map_err(|e| AdapterError::Io(e))?;

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
            duration_ms: None, // verifier will probe if needed
        })
    }
}

/// Map our codec strings to FFmpeg's -codec:a argument values.
fn codec_to_ffmpeg_name(codec: &str) -> Result<&'static str, AdapterError> {
    match codec {
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
        "wav"       => Ok("pcm_s16le"), // WAV container, 16-bit PCM default
        other => Err(AdapterError::UnsupportedCodec(other.to_string())),
    }
}
