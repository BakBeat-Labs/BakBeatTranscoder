// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Sequential graph executor.
//!
//! The graph model supports independent-node parallelism, but execution is
//! intentionally sequential in v1. Determinism, observability, and debuggability
//! take priority over throughput. Concurrency will be added as an explicit,
//! bounded option in a future release.

use std::time::Instant;

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};

use crate::graph::ExecutionGraph;
use crate::resolver::ResolvedCapabilities;
use crate::verifier::{ArtifactRecord, ArtifactStatus, TranscodeManifest};

pub struct ExecutorOptions {
    pub show_progress: bool,
    pub stop_on_error: bool,
}

impl Default for ExecutorOptions {
    fn default() -> Self {
        Self {
            show_progress: true,
            stop_on_error: false,
        }
    }
}

pub fn execute_graph(
    graph: &ExecutionGraph,
    caps: &ResolvedCapabilities,
    opts: ExecutorOptions,
) -> Result<TranscodeManifest> {
    let total = graph.nodes.len() as u64;

    let progress = if opts.show_progress && total > 0 {
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        Some(pb)
    } else {
        None
    };

    let batch_start = Instant::now();
    let mut artifacts: Vec<ArtifactRecord> = Vec::with_capacity(graph.nodes.len());
    let mut success_count = 0usize;
    let mut failure_count = 0usize;

    for node in &graph.nodes {
        if let Some(pb) = &progress {
            pb.set_message(
                node.input_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
            );
        }

        let adapter = caps.adapters.get(&node.adapter).ok_or_else(|| {
            anyhow::anyhow!(
                "adapter '{}' was assigned but is not available — this is a planner bug",
                node.adapter
            )
        })?;

        let encode_start = Instant::now();
        let result = adapter.encode(node);
        let encode_elapsed_ms = encode_start.elapsed().as_millis() as u64;

        let record = match result {
            Ok(info) => {
                success_count += 1;
                tracing::info!(
                    output = ?info.output_path,
                    elapsed_ms = encode_elapsed_ms,
                    "encoded"
                );
                ArtifactRecord {
                    node_id: node.id,
                    output_path: info.output_path,
                    sha256: info.sha256,
                    size_bytes: info.size_bytes,
                    duration_ms: info.duration_ms,
                    encode_elapsed_ms,
                    verified_at: Some(chrono::Utc::now()),
                    status: ArtifactStatus::Success,
                }
            }
            Err(e) => {
                failure_count += 1;
                tracing::error!(
                    input = ?node.input_path,
                    error = %e,
                    "encode failed"
                );

                if opts.stop_on_error {
                    if let Some(pb) = progress {
                        pb.abandon_with_message("aborted on error");
                    }
                    return Err(e.into());
                }

                ArtifactRecord {
                    node_id: node.id,
                    output_path: node.output_path.clone(),
                    sha256: String::new(),
                    size_bytes: 0,
                    duration_ms: None,
                    encode_elapsed_ms,
                    verified_at: None,
                    status: ArtifactStatus::Failed {
                        error: e.to_string(),
                    },
                }
            }
        };

        artifacts.push(record);
        if let Some(pb) = &progress {
            pb.inc(1);
        }
    }

    if let Some(pb) = progress {
        pb.finish_with_message(format!(
            "done — {success_count} succeeded, {failure_count} failed"
        ));
    }

    Ok(TranscodeManifest::new(
        graph.clone(),
        artifacts,
        batch_start.elapsed().as_millis() as u64,
        success_count,
        failure_count,
    ))
}
