// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WMV9 (Main Profile) video encoder adapter.
//!
//! FFmpeg cannot encode WMV9 (`wmv3`) / VC-1 — it only encodes WMV7/WMV8 — and
//! there is no cross-platform open-source WMV9 encoder. Some devices (e.g. the
//! Sony NWZ-E370 Walkman) hard-require true WMV9 Main Profile and reject a WMV8
//! stream. The only encoder that produces real WMV9 is Microsoft's, which ships
//! built into Windows as a Media Foundation Transform. `bbt` drives it through
//! the `bbt-wmv9` helper (see helpers/bbt-wmv9), which exists on Windows only.
//!
//! As a result, WMV9 output is **Windows-only**. On macOS/Linux the helper is
//! absent, this adapter is unavailable, and the resolver fails the plan with a
//! clear "requires the Windows build" message rather than a silent skip.
//!
//! Encoding is a two-step pipeline, mirroring the ATRAC adapter:
//!   1. FFmpeg decodes the source, scales + letterboxes it to the target frame,
//!      and writes a raw NV12 video stream plus an intermediate PCM WAV.
//!   2. `bbt-wmv9` encodes the NV12 → WMV9 and the WAV → WMA, muxing to ASF/.wmv.
//!
//! The intermediate files are written to a temp directory and cleaned up after.

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;
use tracing::{debug, trace};

use crate::adapters::{ensure_parent, sha256_file, ArtifactInfo, EncoderAdapter};
use crate::binaries;
use crate::error::AdapterError;
use crate::graph::ExecutionNode;

pub struct Wmv9Adapter {
    /// The `bbt-wmv9` helper (Windows-only Media Foundation encoder).
    helper: PathBuf,
    /// FFmpeg — required for the stage-1 decode/scale to raw NV12 + WAV.
    ffmpeg: Option<PathBuf>,
}

impl Wmv9Adapter {
    /// Detect the adapter. Returns `None` when the helper is not present, which
    /// is the normal case on non-Windows platforms.
    pub fn detect() -> Option<Self> {
        let helper = binaries::find_wmv9_helper()?;
        Some(Self {
            helper,
            ffmpeg: binaries::find_ffmpeg(),
        })
    }

    /// Stage 1: FFmpeg → raw NV12 frames, scaled + letterboxed to `w`×`h`.
    fn decode_to_nv12(
        &self,
        ffmpeg: &PathBuf,
        input: &std::path::Path,
        tmp_dir: &TempDir,
        w: u32,
        h: u32,
        fps: f32,
    ) -> Result<PathBuf, AdapterError> {
        let yuv_path = tmp_dir.path().join("video.nv12");
        // force_original_aspect_ratio=decrease + pad centres the source inside
        // the target frame with black bars, preserving aspect ratio. Devices in
        // this class (e.g. 128×96) reject stretched or oversized frames.
        let vf = format!(
            "scale=w={w}:h={h}:force_original_aspect_ratio=decrease,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black"
        );
        trace!(?ffmpeg, ?input, yuv = ?yuv_path, "decoding to raw NV12");

        let output = Command::new(ffmpeg)
            .args([
                "-y",
                "-i",
                &input.to_string_lossy(),
                "-an",
                "-vf",
                &vf,
                "-r",
                &fps.to_string(),
                "-pix_fmt",
                "nv12",
                "-f",
                "rawvideo",
                &yuv_path.to_string_lossy(),
            ])
            .output()
            .map_err(AdapterError::Io)?;

        if !output.status.success() {
            return Err(AdapterError::DecodeFailed {
                path: input.to_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }
        Ok(yuv_path)
    }

    /// Stage 1: FFmpeg → intermediate PCM s16le WAV for the audio track.
    fn decode_to_wav(
        &self,
        ffmpeg: &PathBuf,
        input: &std::path::Path,
        tmp_dir: &TempDir,
        sample_rate: u32,
        channels: u8,
    ) -> Result<PathBuf, AdapterError> {
        let wav_path = tmp_dir.path().join("audio.wav");
        trace!(?ffmpeg, ?input, wav = ?wav_path, "decoding to intermediate WAV");

        let output = Command::new(ffmpeg)
            .args([
                "-y",
                "-i",
                &input.to_string_lossy(),
                "-vn",
                "-ar",
                &sample_rate.to_string(),
                "-ac",
                &channels.to_string(),
                "-codec:a",
                "pcm_s16le",
                "-f",
                "wav",
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

    /// Stage 2: run the `bbt-wmv9` helper on the intermediates.
    #[allow(clippy::too_many_arguments)]
    fn encode_wmv9(
        &self,
        nv12: &std::path::Path,
        wav: &std::path::Path,
        output: &std::path::Path,
        w: u32,
        h: u32,
        fps: f32,
        vbitrate_kbps: u32,
        abitrate_kbps: u32,
    ) -> Result<(), AdapterError> {
        let args = [
            "--video".to_string(),
            nv12.to_string_lossy().into_owned(),
            "--width".to_string(),
            w.to_string(),
            "--height".to_string(),
            h.to_string(),
            "--fps".to_string(),
            fps.to_string(),
            "--vbitrate".to_string(),
            vbitrate_kbps.to_string(),
            "--audio".to_string(),
            wav.to_string_lossy().into_owned(),
            "--abitrate".to_string(),
            abitrate_kbps.to_string(),
            "--output".to_string(),
            output.to_string_lossy().into_owned(),
        ];
        trace!(helper = ?self.helper, ?args, "running bbt-wmv9");

        let result = Command::new(&self.helper)
            .args(&args)
            .output()
            .map_err(AdapterError::Io)?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr).to_string();
            debug!(stderr = %stderr, "bbt-wmv9 failed");
            return Err(AdapterError::EncodeFailed {
                path: nv12.to_owned(),
                stderr,
            });
        }
        Ok(())
    }
}

impl EncoderAdapter for Wmv9Adapter {
    fn supported_output_codecs(&self) -> &[&str] {
        &["wmv9"]
    }

    fn is_available(&self) -> bool {
        self.helper.exists() && self.ffmpeg.is_some()
    }

    fn encode(&self, node: &ExecutionNode) -> Result<ArtifactInfo, AdapterError> {
        let ffmpeg = self.ffmpeg.as_ref().ok_or_else(|| AdapterError::BinaryNotFound {
            binary: "ffmpeg".to_string(),
        })?;

        let p = &node.params;

        // WMV9 is a video codec — width, height, and frame rate must be resolved
        // to concrete values by the planner before we get here.
        let (w, h) = match (p.width, p.height) {
            (Some(w), Some(h)) => (w, h),
            _ => {
                return Err(AdapterError::UnsupportedCodec(
                    "wmv9 requires explicit width and height".to_string(),
                ))
            }
        };
        let fps = p.frame_rate.ok_or_else(|| {
            AdapterError::UnsupportedCodec("wmv9 requires an explicit frame rate".to_string())
        })?;
        let vbitrate = p.video_bitrate_kbps.ok_or_else(|| {
            AdapterError::UnsupportedCodec("wmv9 requires an explicit video bitrate".to_string())
        })?;
        let abitrate = p.audio_bitrate_kbps.ok_or_else(|| {
            AdapterError::UnsupportedCodec("wmv9 requires an explicit audio bitrate".to_string())
        })?;

        ensure_parent(&node.output_path)?;
        let tmp_dir = tempfile::tempdir().map_err(AdapterError::Io)?;

        // Stage 1: normalize with FFmpeg.
        let nv12 = self.decode_to_nv12(ffmpeg, &node.input_path, &tmp_dir, w, h, fps)?;
        let wav = self.decode_to_wav(
            ffmpeg,
            &node.input_path,
            &tmp_dir,
            p.sample_rate_hz,
            p.channels,
        )?;

        // Stage 2: encode WMV9 + WMA and mux to ASF.
        self.encode_wmv9(
            &nv12,
            &wav,
            &node.output_path,
            w,
            h,
            fps,
            vbitrate,
            abitrate,
        )?;

        // tmp_dir drops here, cleaning up the intermediates.

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
