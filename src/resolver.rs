// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Capability resolver — detects available encoder backends and validates
//! that a planned set of jobs can be fully executed before any encoding starts.
//!
//! The resolver enforces the invariant: fail completely before starting,
//! never fail partway through a batch.

use std::collections::HashMap;

use anyhow::Result;

use crate::adapters::atrac::AtracAdapter;
use crate::adapters::ffmpeg::FfmpegAdapter;
use crate::adapters::wmv9::Wmv9Adapter;
use crate::adapters::EncoderAdapter;
use crate::error::BbtError;
use crate::graph::{EncodeParams, MediaType};
use crate::planner::PlannedJob;

/// True when a job targets WMV9 video (routes to the Windows-only helper).
fn is_wmv9_job(params: &EncodeParams) -> bool {
    params.media_type == MediaType::Video && params.video_codec.as_deref() == Some("wmv9")
}

/// The set of encoder adapters available on this system.
pub struct ResolvedCapabilities {
    pub adapters: HashMap<String, Box<dyn EncoderAdapter>>,
}

impl ResolvedCapabilities {
    /// Detect all available adapters.
    pub fn detect() -> Self {
        let mut adapters: HashMap<String, Box<dyn EncoderAdapter>> = HashMap::new();

        if let Some(ffmpeg) = FfmpegAdapter::detect() {
            adapters.insert("ffmpeg".to_string(), Box::new(ffmpeg));
        }

        let atrac = AtracAdapter::detect();
        if atrac.is_available() {
            adapters.insert("atrac".to_string(), Box::new(atrac));
        }

        // WMV9 helper is Windows-only; absent elsewhere by design.
        if let Some(wmv9) = Wmv9Adapter::detect() {
            if wmv9.is_available() {
                adapters.insert("wmv9".to_string(), Box::new(wmv9));
            }
        }

        Self { adapters }
    }

    pub fn has_adapter(&self, name: &str) -> bool {
        self.adapters.contains_key(name)
    }

    /// Find the best available adapter for a given audio output codec.
    /// ATRAC codecs route exclusively to the atrac adapter.
    /// All other audio codecs route to ffmpeg.
    pub fn adapter_for_codec(&self, codec: &str) -> Option<&str> {
        let preference: &[&str] = match codec {
            "atrac1" | "atrac3" | "atrac3p" => &["atrac"],
            _ => &["ffmpeg"],
        };
        for name in preference {
            if self.adapters.contains_key(*name) {
                return Some(name);
            }
        }
        None
    }

    /// Route a fully-resolved job to an adapter.
    ///
    /// Video jobs route on the *video* codec: WMV9 goes to the Windows-only
    /// `wmv9` helper, all other video (and the embedded audio track) goes to
    /// FFmpeg. Audio-only jobs route on the audio codec.
    pub fn adapter_for_params(&self, params: &EncodeParams) -> Option<&str> {
        if is_wmv9_job(params) {
            return self.adapters.contains_key("wmv9").then_some("wmv9");
        }
        if params.media_type == MediaType::Video {
            return self.adapters.contains_key("ffmpeg").then_some("ffmpeg");
        }
        self.adapter_for_codec(&params.audio_codec)
    }

    /// Validate that every job in the plan can be satisfied.
    /// Returns an error describing *all* unsatisfied requirements, not just the first.
    /// This is the "fail completely before starting" gate.
    pub fn validate_plan(&self, jobs: &[PlannedJob]) -> Result<()> {
        let mut errors: Vec<String> = Vec::new();

        // Check that at least ffmpeg is present for any non-ATRAC job
        let needs_ffmpeg = jobs.iter().any(|j| {
            !matches!(j.params.audio_codec.as_str(), "atrac1" | "atrac3" | "atrac3p")
        });
        let needs_atrac = jobs.iter().any(|j| {
            matches!(j.params.audio_codec.as_str(), "atrac1" | "atrac3" | "atrac3p")
        });

        if needs_ffmpeg && !self.has_adapter("ffmpeg") {
            errors.push(
                "FFmpeg is required but not found in PATH. \
                 Install from https://ffmpeg.org or via your package manager."
                    .to_string(),
            );
        }

        if needs_atrac && !self.has_adapter("atrac") {
            errors.push(
                "ATRAC encoding requires atracdenc (open source) or atracenc. \
                 Install atracdenc from https://github.com/dcherednik/atracdenc \
                 and ensure it is in your PATH."
                    .to_string(),
            );
        }

        // WMV9 requires the Windows-only bbt-wmv9 helper. Give a targeted
        // message instead of a generic "no adapter" error, since this is a
        // platform limitation, not a missing install the user can fix on macOS.
        let needs_wmv9 = jobs.iter().any(|j| is_wmv9_job(&j.params));
        if needs_wmv9 && !self.has_adapter("wmv9") {
            errors.push(
                "WMV9 video encoding requires the Windows build of bbt (it uses \
                 the OS Media Foundation encoder via the bundled bbt-wmv9 helper). \
                 There is no cross-platform WMV9 encoder — FFmpeg can only encode \
                 WMV7/WMV8, which these devices reject. Run this on Windows, or \
                 set BBT_WMV9_PATH to a bbt-wmv9.exe build."
                    .to_string(),
            );
        }

        // Check each job individually for an unresolvable target.
        for job in jobs {
            if self.adapter_for_params(&job.params).is_none() && !is_wmv9_job(&job.params) {
                errors.push(format!(
                    "no adapter can encode to codec '{}' (file: {})",
                    job.params.audio_codec,
                    job.source_path.display()
                ));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(BbtError::CapabilityError(errors.join("\n")).into())
        }
    }

    /// Assign an adapter name to each job. Call after validate_plan succeeds.
    pub fn assign_adapters(&self, jobs: &mut Vec<PlannedJob>) {
        for job in jobs {
            job.assigned_adapter = self.adapter_for_params(&job.params).map(str::to_string);
        }
    }
}

