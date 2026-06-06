// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ATRAC encoder adapter using atracdenc (preferred, open source, LGPL) or
//! atracenc (Sony binary, if available). Handles MiniDisc SP, LP2, and LP4 modes.
//!
//! ATRAC encoding requires a two-step pipeline:
//!   1. FFmpeg decodes input to an intermediate PCM WAV (44.1 kHz stereo)
//!   2. atracdenc encodes the WAV to the ATRAC output format
//!
//! The intermediate WAV is written to a temp directory and cleaned up after encode.
//!
//! Output formats:
//!   atrac1  (MD SP)  → .aea  (raw ATRAC1 data, ~292 kbps)
//!   atrac3  (MD LP2) → .oma  (ATRAC3, 132 kbps)
//!   atrac3  (MD LP4) → .oma  (ATRAC3, 66 kbps)

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;
use tracing::{debug, trace};

use crate::adapters::{ensure_parent, sha256_file, ArtifactInfo, EncoderAdapter};
use crate::error::AdapterError;
use crate::graph::ExecutionNode;

pub struct AtracAdapter {
    /// atracdenc binary (preferred, open source)
    atracdenc: Option<PathBuf>,
    /// atracenc binary (Sony, fallback)
    atracenc: Option<PathBuf>,
    /// FFmpeg binary — required for the decode-to-WAV step
    ffmpeg: Option<PathBuf>,
}

impl AtracAdapter {
    pub fn detect() -> Self {
        Self {
            atracdenc: which::which("atracdenc").ok(),
            atracenc: which::which("atracenc").ok(),
            ffmpeg: which::which("ffmpeg").ok(),
        }
    }

    fn encoder_binary(&self) -> Option<(&PathBuf, AtracTool)> {
        if let Some(p) = &self.atracdenc {
            return Some((p, AtracTool::Atracdenc));
        }
        if let Some(p) = &self.atracenc {
            return Some((p, AtracTool::Atracenc));
        }
        None
    }

    /// Decode input to a temporary WAV file using FFmpeg.
    fn decode_to_wav(
        &self,
        input: &std::path::Path,
        tmp_dir: &TempDir,
        sample_rate: u32,
        channels: u8,
    ) -> Result<PathBuf, AdapterError> {
        let ffmpeg = self.ffmpeg.as_ref().ok_or_else(|| AdapterError::BinaryNotFound {
            binary: "ffmpeg".to_string(),
        })?;

        let wav_path = tmp_dir.path().join("intermediate.wav");
        trace!(?ffmpeg, input = ?input, wav = ?wav_path, "decoding to intermediate WAV");

        let output = Command::new(ffmpeg)
            .args([
                "-y",
                "-i",
                &input.to_string_lossy(),
                "-ar",
                &sample_rate.to_string(),
                "-ac",
                &channels.to_string(),
                // 16-bit signed PCM — atracdenc expects standard WAV
                "-codec:a", "pcm_s16le",
                "-f", "wav",
                &wav_path.to_string_lossy(),
            ])
            .output()
            .map_err(AdapterError::Io)?;

        if !output.status.success() {
            return Err(AdapterError::DecodeFailed {
                path: input.to_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(wav_path)
    }

    fn encode_atrac(
        &self,
        binary: &PathBuf,
        tool: AtracTool,
        wav_input: &std::path::Path,
        output: &std::path::Path,
        codec: &str,
        bitrate_kbps: Option<u32>,
    ) -> Result<(), AdapterError> {
        let args = match tool {
            AtracTool::Atracdenc => build_atracdenc_args(wav_input, output, codec, bitrate_kbps)?,
            AtracTool::Atracenc => build_atracenc_args(wav_input, output, codec, bitrate_kbps)?,
        };

        trace!(?binary, ?args, "running ATRAC encoder");

        let result = Command::new(binary)
            .args(&args)
            .output()
            .map_err(AdapterError::Io)?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr).to_string();
            debug!(stderr = %stderr, "ATRAC encoder failed");
            return Err(AdapterError::EncodeFailed {
                path: wav_input.to_owned(),
                stderr,
            });
        }

        Ok(())
    }
}

impl EncoderAdapter for AtracAdapter {
    fn name(&self) -> &str {
        "atrac"
    }

    fn supported_output_codecs(&self) -> &[&str] {
        &["atrac1", "atrac3", "atrac3p"]
    }

    fn is_available(&self) -> bool {
        self.encoder_binary().is_some() && self.ffmpeg.is_some()
    }

    fn encode(&self, node: &ExecutionNode) -> Result<ArtifactInfo, AdapterError> {
        let (binary, tool) = self.encoder_binary().ok_or_else(|| AdapterError::BinaryNotFound {
            binary: "atracdenc or atracenc".to_string(),
        })?;

        ensure_parent(&node.output_path)?;

        let tmp_dir = tempfile::tempdir().map_err(AdapterError::Io)?;

        // Step 1: decode input to WAV
        let wav_path = self.decode_to_wav(
            &node.input_path,
            &tmp_dir,
            node.params.sample_rate_hz,
            node.params.channels,
        )?;

        // Step 2: encode WAV to ATRAC
        self.encode_atrac(
            binary,
            tool,
            &wav_path,
            &node.output_path,
            &node.params.audio_codec,
            node.params.audio_bitrate_kbps,
        )?;

        // tmp_dir drops here, cleaning up the intermediate WAV

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

#[derive(Clone, Copy)]
enum AtracTool {
    Atracdenc,
    Atracenc,
}

/// Build args for the atracdenc open-source encoder.
/// https://github.com/dcherednik/atracdenc
fn build_atracdenc_args(
    input: &std::path::Path,
    output: &std::path::Path,
    codec: &str,
    bitrate_kbps: Option<u32>,
) -> Result<Vec<String>, AdapterError> {
    let mut args = Vec::new();

    match codec {
        "atrac1" => {
            // MD SP: ATRAC1, ~292 kbps
            args.extend(["-e".into(), "atrac1".into()]);
        }
        "atrac3" => {
            // MD LP2 (132 kbps default) or LP4 (66 kbps)
            args.extend(["-e".into(), "atrac3".into()]);
            if let Some(kbps) = bitrate_kbps {
                if kbps <= 66 {
                    args.extend(["--bitrate".into(), "66".into()]);
                }
                // 132 kbps is atracdenc's default for atrac3; no flag needed
            }
        }
        other => return Err(AdapterError::UnsupportedCodec(other.to_string())),
    }

    args.extend([
        "-i".into(), input.to_string_lossy().into_owned(),
        "-o".into(), output.to_string_lossy().into_owned(),
    ]);

    Ok(args)
}

/// Build args for the Sony atracenc binary (fallback, closed source).
/// Exact flags may vary by version; document and adjust as needed.
fn build_atracenc_args(
    input: &std::path::Path,
    output: &std::path::Path,
    codec: &str,
    bitrate_kbps: Option<u32>,
) -> Result<Vec<String>, AdapterError> {
    let mut args = Vec::new();

    match codec {
        "atrac1" => {
            args.push("--mode=SP".into());
        }
        "atrac3" => {
            let kbps = bitrate_kbps.unwrap_or(132);
            if kbps <= 66 {
                args.push("--mode=LP4".into());
            } else {
                args.push("--mode=LP2".into());
            }
        }
        other => return Err(AdapterError::UnsupportedCodec(other.to_string())),
    }

    args.extend([
        input.to_string_lossy().into_owned(),
        output.to_string_lossy().into_owned(),
    ]);

    Ok(args)
}
