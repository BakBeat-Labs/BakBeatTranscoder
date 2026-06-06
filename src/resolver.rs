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
use crate::adapters::EncoderAdapter;
use crate::error::BbtError;
use crate::planner::PlannedJob;

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

        Self { adapters }
    }

    pub fn has_adapter(&self, name: &str) -> bool {
        self.adapters.contains_key(name)
    }

    /// Find the best available adapter for a given output codec.
    /// ATRAC codecs route exclusively to the atrac adapter.
    /// All other codecs route to ffmpeg.
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

        // Check each job individually for unresolvable codec
        for job in jobs {
            if self.adapter_for_codec(&job.params.audio_codec).is_none() {
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
            job.assigned_adapter = self
                .adapter_for_codec(&job.params.audio_codec)
                .map(str::to_string);
        }
    }
}

