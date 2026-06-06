// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Structured progress emission for both human and machine consumers.
//!
//! In `--json` mode: NDJSON on stdout — one complete JSON event per line.
//! BakBeat reads this line-by-line and maps it to its status lane.
//!
//! In human mode: indicatif progress bars on stderr, summary on stdout.

use std::path::Path;

use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;

// ── Event types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Probe,
    Plan,
    Resolve,
    Encode,
}

/// A single progress event. Serialized as a tagged JSON object.
/// Every event is self-contained — no shared state required to interpret it.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A pipeline phase is beginning.
    PhaseStart {
        phase: Phase,
        /// Total item count, if known before the phase runs.
        #[serde(skip_serializing_if = "Option::is_none")]
        total: Option<usize>,
    },

    /// A file was processed within a phase.
    FileComplete {
        phase: Phase,
        current: usize,
        total: usize,
        file: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        elapsed_ms: u64,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Encoding of a specific file is starting (encode phase only).
    /// Emitted before the encode begins so the consumer can show "currently encoding X".
    EncodeStart {
        current: usize,
        total: usize,
        file: String,
        output: String,
    },

    /// A pipeline phase completed.
    PhaseComplete {
        phase: Phase,
        #[serde(skip_serializing_if = "Option::is_none")]
        total: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        jobs: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        skipped: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        success: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        failed: Option<usize>,
    },

    /// All phases complete. Final summary event.
    Complete {
        success: usize,
        failed: usize,
        total_elapsed_ms: u64,
        manifest: String,
    },
}

// ── Emitter ───────────────────────────────────────────────────────────────────

/// Emits progress events to either NDJSON (machine) or indicatif (human).
pub struct Emitter {
    json_mode: bool,
    bar: Option<ProgressBar>,
}

impl Emitter {
    pub fn new(json_mode: bool) -> Self {
        Self {
            json_mode,
            bar: None,
        }
    }

    pub fn emit(&mut self, event: Event) {
        if self.json_mode {
            println!(
                "{}",
                serde_json::to_string(&event).expect("progress event always serializes")
            );
        } else {
            self.render_human(event);
        }
    }

    fn render_human(&mut self, event: Event) {
        match event {
            Event::PhaseStart { phase, total } => {
                // Finish any previous bar
                if let Some(bar) = self.bar.take() {
                    bar.finish_and_clear();
                }
                match phase {
                    Phase::Probe => {
                        let n = total.unwrap_or(0) as u64;
                        let bar = ProgressBar::new(n);
                        bar.set_style(
                            ProgressStyle::with_template(
                                "  {spinner:.dim} Probing [{pos}/{len}] {msg}",
                            )
                            .unwrap(),
                        );
                        self.bar = Some(bar);
                    }
                    Phase::Plan => {
                        eprint!("  Planning...");
                    }
                    Phase::Resolve => {
                        eprint!("  Checking capabilities...");
                    }
                    Phase::Encode => {
                        let n = total.unwrap_or(0) as u64;
                        let bar = ProgressBar::new(n);
                        bar.set_style(
                            ProgressStyle::with_template(
                                "  {spinner:.green} [{elapsed_precise}] \
                                 [{bar:40.cyan/blue}] {pos}/{len}  {msg}",
                            )
                            .unwrap()
                            .progress_chars("=>-"),
                        );
                        self.bar = Some(bar);
                    }
                }
            }

            Event::FileComplete {
                phase,
                file,
                success,
                error,
                ..
            } => match phase {
                Phase::Probe => {
                    if let Some(bar) = &self.bar {
                        let name = Path::new(&file)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        bar.set_message(name);
                        bar.inc(1);
                    }
                }
                Phase::Encode => {
                    if let Some(bar) = &self.bar {
                        bar.inc(1);
                    }
                    if !success {
                        if let Some(e) = error {
                            eprintln!("\n  error: {e}");
                        }
                    }
                }
                _ => {}
            },

            Event::EncodeStart { file, .. } => {
                if let Some(bar) = &self.bar {
                    let name = Path::new(&file)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    bar.set_message(name);
                }
            }

            Event::PhaseComplete {
                phase,
                jobs,
                skipped,
                success,
                failed,
                ..
            } => {
                if let Some(bar) = self.bar.take() {
                    bar.finish_and_clear();
                }
                match phase {
                    Phase::Probe => {
                        // probe completion is implicit — plan will report counts
                    }
                    Phase::Plan => {
                        let j = jobs.unwrap_or(0);
                        let s = skipped.unwrap_or(0);
                        eprintln!(
                            " {j} to transcode{}",
                            if s > 0 {
                                format!(", {s} already in target format")
                            } else {
                                String::new()
                            }
                        );
                    }
                    Phase::Resolve => {
                        eprintln!(" ok");
                    }
                    Phase::Encode => {
                        let ok = success.unwrap_or(0);
                        let fail = failed.unwrap_or(0);
                        eprintln!("  encoded: {ok} ok, {fail} failed");
                    }
                }
            }

            Event::Complete {
                success,
                failed,
                total_elapsed_ms,
                manifest,
            } => {
                if let Some(bar) = self.bar.take() {
                    bar.finish_and_clear();
                }
                let secs = total_elapsed_ms as f64 / 1000.0;
                println!(
                    "\n{success} succeeded, {failed} failed  ({secs:.1}s)\nmanifest: {manifest}"
                );
            }
        }
    }
}
