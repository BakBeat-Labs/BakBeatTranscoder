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

use crate::graph::ExecutionGraph;
use crate::progress::{Emitter, Event, Phase};
use crate::resolver::ResolvedCapabilities;
use crate::verifier::{ArtifactRecord, ArtifactStatus, TranscodeManifest};

pub fn execute_graph(
    graph: &ExecutionGraph,
    caps: &ResolvedCapabilities,
    emitter: &mut Emitter,
    stop_on_error: bool,
) -> Result<TranscodeManifest> {
    let total = graph.nodes.len();

    emitter.emit(Event::PhaseStart {
        phase: Phase::Encode,
        total: Some(total),
    });

    let batch_start = Instant::now();
    let mut artifacts: Vec<ArtifactRecord> = Vec::with_capacity(total);
    let mut success_count = 0usize;
    let mut failure_count = 0usize;

    for (idx, node) in graph.nodes.iter().enumerate() {
        let current = idx + 1;

        emitter.emit(Event::EncodeStart {
            current,
            total,
            file: node.input_path.to_string_lossy().into_owned(),
            output: node.output_path.to_string_lossy().into_owned(),
        });

        let adapter = caps.adapters.get(&node.adapter).ok_or_else(|| {
            anyhow::anyhow!(
                "adapter '{}' assigned but unavailable — planner bug",
                node.adapter
            )
        })?;

        let encode_start = Instant::now();
        let result = adapter.encode(node);
        let elapsed_ms = encode_start.elapsed().as_millis() as u64;

        let record = match result {
            Ok(info) => {
                success_count += 1;
                tracing::info!(output = ?info.output_path, elapsed_ms, "encoded");

                emitter.emit(Event::FileComplete {
                    phase: Phase::Encode,
                    current,
                    total,
                    file: node.input_path.to_string_lossy().into_owned(),
                    output: Some(info.output_path.to_string_lossy().into_owned()),
                    elapsed_ms,
                    success: true,
                    error: None,
                });

                ArtifactRecord {
                    node_id: node.id,
                    output_path: info.output_path,
                    sha256: info.sha256,
                    size_bytes: info.size_bytes,
                    duration_ms: info.duration_ms,
                    encode_elapsed_ms: elapsed_ms,
                    verified_at: Some(chrono::Utc::now()),
                    status: ArtifactStatus::Success,
                }
            }
            Err(e) => {
                failure_count += 1;
                tracing::error!(input = ?node.input_path, error = %e, "encode failed");

                emitter.emit(Event::FileComplete {
                    phase: Phase::Encode,
                    current,
                    total,
                    file: node.input_path.to_string_lossy().into_owned(),
                    output: Some(node.output_path.to_string_lossy().into_owned()),
                    elapsed_ms,
                    success: false,
                    error: Some(e.to_string()),
                });

                if stop_on_error {
                    return Err(e.into());
                }

                ArtifactRecord {
                    node_id: node.id,
                    output_path: node.output_path.clone(),
                    sha256: String::new(),
                    size_bytes: 0,
                    duration_ms: None,
                    encode_elapsed_ms: elapsed_ms,
                    verified_at: None,
                    status: ArtifactStatus::Failed {
                        error: e.to_string(),
                    },
                }
            }
        };

        artifacts.push(record);
    }

    emitter.emit(Event::PhaseComplete {
        phase: Phase::Encode,
        total: Some(total),
        jobs: None,
        skipped: None,
        success: Some(success_count),
        failed: Some(failure_count),
    });

    Ok(TranscodeManifest::new(
        graph.clone(),
        artifacts,
        batch_start.elapsed().as_millis() as u64,
        success_count,
        failure_count,
    ))
}
