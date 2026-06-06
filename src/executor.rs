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

use std::collections::HashMap;

use crate::graph::ExecutionGraph;
use crate::progress::{Emitter, Event, Phase};
use crate::resolver::ResolvedCapabilities;
use crate::verifier::{
    artifact_still_valid, ArtifactRecord, ArtifactStatus, TranscodeManifest,
};

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
        carrying_forward: None,
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

                emitter.emit(Event::FileFailed {
                    phase: Phase::Encode,
                    current,
                    total,
                    file: node.input_path.to_string_lossy().into_owned(),
                    output: Some(node.output_path.to_string_lossy().into_owned()),
                    elapsed_ms,
                    error: e.to_string(),
                });

                if stop_on_error {
                    emitter.emit(Event::OperationFailed {
                        phase: Some(Phase::Encode),
                        error: format!("aborted after failure: {e}"),
                    });
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

/// Resume a previously failed or incomplete run.
///
/// For each node in the original graph:
///   - If its artifact was successful AND the output file is still intact
///     (SHA-256 matches) → carry it forward, do not re-encode.
///   - Otherwise → re-encode.
///
/// Produces a new manifest with `resumed_from` set to the original manifest_id.
pub fn resume_graph(
    prior: &TranscodeManifest,
    caps: &ResolvedCapabilities,
    emitter: &mut Emitter,
    stop_on_error: bool,
) -> Result<TranscodeManifest> {
    let graph = &prior.graph;

    // Build lookup: node_id → prior artifact record
    let prior_artifacts: HashMap<_, _> = prior
        .artifacts
        .iter()
        .map(|r| (r.node_id, r))
        .collect();

    // Partition nodes into carry-forward vs needs-encode
    let mut carry_forward: Vec<ArtifactRecord> = Vec::new();
    let mut needs_encode: Vec<_> = Vec::new();

    for node in &graph.nodes {
        let prior_record = prior_artifacts.get(&node.id);
        let is_valid = prior_record
            .filter(|r| r.status.is_good())
            .map(|r| artifact_still_valid(r))
            .unwrap_or(false);

        if is_valid {
            let record = (*prior_record.unwrap()).clone();
            carry_forward.push(ArtifactRecord {
                status: ArtifactStatus::CarriedForward {
                    from_manifest_id: prior.manifest_id,
                },
                verified_at: Some(chrono::Utc::now()),
                ..record
            });
        } else {
            needs_encode.push(node);
        }
    }

    let carried_forward_count = carry_forward.len();
    let encode_total = needs_encode.len();

    emitter.emit(Event::PhaseStart {
        phase: Phase::Encode,
        total: Some(encode_total),
        carrying_forward: Some(carried_forward_count),
    });

    let batch_start = Instant::now();
    let mut new_artifacts: Vec<ArtifactRecord> = Vec::new();
    let mut success_count = 0usize;
    let mut failure_count = 0usize;

    for (idx, node) in needs_encode.iter().enumerate() {
        let current = idx + 1;

        emitter.emit(Event::EncodeStart {
            current,
            total: encode_total,
            file: node.input_path.to_string_lossy().into_owned(),
            output: node.output_path.to_string_lossy().into_owned(),
        });

        let adapter = caps.adapters.get(&node.adapter).ok_or_else(|| {
            anyhow::anyhow!("adapter '{}' assigned but unavailable — planner bug", node.adapter)
        })?;

        let encode_start = Instant::now();
        let result = adapter.encode(node);
        let elapsed_ms = encode_start.elapsed().as_millis() as u64;

        let record = match result {
            Ok(info) => {
                success_count += 1;
                tracing::info!(output = ?info.output_path, elapsed_ms, "re-encoded");

                emitter.emit(Event::FileComplete {
                    phase: Phase::Encode,
                    current,
                    total: encode_total,
                    file: node.input_path.to_string_lossy().into_owned(),
                    output: Some(info.output_path.to_string_lossy().into_owned()),
                    elapsed_ms,
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
                tracing::error!(input = ?node.input_path, error = %e, "re-encode failed");

                emitter.emit(Event::FileFailed {
                    phase: Phase::Encode,
                    current,
                    total: encode_total,
                    file: node.input_path.to_string_lossy().into_owned(),
                    output: Some(node.output_path.to_string_lossy().into_owned()),
                    elapsed_ms,
                    error: e.to_string(),
                });

                if stop_on_error {
                    emitter.emit(Event::OperationFailed {
                        phase: Some(Phase::Encode),
                        error: format!("aborted after failure: {e}"),
                    });
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
                    status: ArtifactStatus::Failed { error: e.to_string() },
                }
            }
        };

        new_artifacts.push(record);
    }

    emitter.emit(Event::PhaseComplete {
        phase: Phase::Encode,
        total: Some(encode_total),
        jobs: None,
        skipped: None,
        success: Some(success_count),
        failed: Some(failure_count),
    });

    // Merge: carried-forward first (preserves original ordering), then newly encoded.
    // Re-sort by node sequence to restore graph order.
    let mut all_artifacts = carry_forward;
    all_artifacts.extend(new_artifacts);
    let node_sequence: HashMap<_, _> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id, i))
        .collect();
    all_artifacts.sort_by_key(|r| node_sequence.get(&r.node_id).copied().unwrap_or(usize::MAX));

    Ok(TranscodeManifest::new_resumed(
        graph.clone(),
        all_artifacts,
        batch_start.elapsed().as_millis() as u64,
        success_count,
        failure_count,
        carried_forward_count,
        prior.manifest_id,
    ))
}
