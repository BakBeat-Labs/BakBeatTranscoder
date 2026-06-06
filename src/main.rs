// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod adapters;
mod cli;
mod error;
mod executor;
mod graph;
mod planner;
mod probe;
mod profiles;
mod progress;
mod resolver;
mod verifier;

use std::path::PathBuf;
use std::process;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use cli::{Cli, Commands};
use graph::MediaType;

fn main() {
    let cli = Cli::parse();

    // Initialize tracing — respects BBT_LOG env var or --log-level flag
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(&cli.log_level)),
        )
        .with_target(false)
        .with_writer(std::io::stderr) // log to stderr; stdout is for output data
        .init();

    let result = run(cli);

    match result {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {e:#}");
            process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Transcode(args) => cmd_transcode(args, cli.json),
        Commands::Plan(args) => cmd_plan(args, cli.json),
        Commands::Execute(args) => cmd_execute(args, cli.json),
        Commands::Verify(args) => cmd_verify(args, cli.json),
        Commands::Resume(args) => cmd_resume(args, cli.json),
        Commands::Probe(args) => cmd_probe(args, cli.json),
        Commands::Profiles(args) => cmd_profiles(args, cli.json),
        Commands::Check => cmd_check(cli.json),
    }
}

// ── transcode ─────────────────────────────────────────────────────────────────

fn cmd_transcode(args: cli::TranscodeArgs, json: bool) -> Result<()> {
    use progress::{Emitter, Event, Phase};

    let profile = resolve_profile(&args.profile, &args.codec, &args.container,
                                  &args.extension, args.bitrate, args.sample_rate,
                                  args.channels, args.cbr, &args.profile_dir)?;

    let inputs = expand_inputs(&args.inputs)?;
    if inputs.is_empty() {
        eprintln!("no audio files found in the given inputs");
        return Ok(());
    }

    let source_root = args.source_root.as_deref().or_else(|| common_prefix(&inputs));
    let mut emitter = Emitter::new(json);

    // Phase 1: Probe + Plan
    emitter.emit(Event::PhaseStart { phase: Phase::Probe, total: Some(inputs.len()), carrying_forward: None });

    let mut plan = planner::build_plan(
        &inputs, &profile, &args.output, source_root,
        |current, total, path, elapsed_ms| {
            emitter.emit(Event::FileComplete {
                phase: Phase::Probe,
                current,
                total,
                file: path.to_string_lossy().into_owned(),
                output: None,
                elapsed_ms,
            });
        },
    )?;

    emitter.emit(Event::PhaseComplete {
        phase: Phase::Probe,
        total: Some(inputs.len()),
        jobs: None,
        skipped: None,
        success: None,
        failed: None,
    });

    emitter.emit(Event::PhaseStart { phase: Phase::Plan, total: None, carrying_forward: None });
    emitter.emit(Event::PhaseComplete {
        phase: Phase::Plan,
        total: None,
        jobs: Some(plan.jobs.len()),
        skipped: Some(plan.skipped_count),
        success: None,
        failed: None,
    });

    if plan.jobs.is_empty() {
        emitter.emit(Event::Complete {
            success: 0,
            failed: 0,
            total_elapsed_ms: 0,
            manifest: String::new(),
            carried_forward: None,
            re_encoded: None,
        });
        return Ok(());
    }

    // Phase 2: Resolve capabilities
    emitter.emit(Event::PhaseStart { phase: Phase::Resolve, total: None, carrying_forward: None });
    let caps = resolver::ResolvedCapabilities::detect();
    if let Err(e) = caps.validate_plan(&plan.jobs) {
        emitter.emit(Event::OperationFailed {
            phase: Some(Phase::Resolve),
            error: e.to_string(),
        });
        return Err(e);
    }
    caps.assign_adapters(&mut plan.jobs);
    emitter.emit(Event::PhaseComplete {
        phase: Phase::Resolve,
        total: None,
        jobs: None,
        skipped: None,
        success: None,
        failed: None,
    });

    // Phase 3: Build graph
    let graph = planner::plan_to_graph(&plan)?;

    // Phase 4: Execute (emitter handed to executor for per-file events)
    let manifest = executor::execute_graph(&graph, &caps, &mut emitter, args.stop_on_error)?;

    // Phase 5: Save manifest + emit complete
    let manifest_path = args.manifest.unwrap_or_else(|| args.output.join("manifest.json"));
    manifest.save_to_file(&manifest_path)?;

    emitter.emit(Event::Complete {
        success: manifest.success_count,
        failed: manifest.failure_count,
        total_elapsed_ms: manifest.total_elapsed_ms,
        manifest: manifest_path.to_string_lossy().into_owned(),
        carried_forward: None,
        re_encoded: None,
    });

    // In JSON mode, also emit the full manifest as the final object
    if json {
        println!("{}", serde_json::to_string_pretty(&manifest)?);
    }

    if manifest.failure_count > 0 {
        process::exit(2);
    }

    Ok(())
}

// ── plan ──────────────────────────────────────────────────────────────────────

fn cmd_plan(args: cli::PlanArgs, json: bool) -> Result<()> {
    use progress::{Emitter, Event, Phase};

    let profile = resolve_profile(&args.profile, &args.codec, &args.container,
                                  &args.extension, args.bitrate, args.sample_rate,
                                  args.channels, args.cbr, &args.profile_dir)?;

    let inputs = expand_inputs(&args.inputs)?;
    let source_root = args.source_root.as_deref().or_else(|| common_prefix(&inputs));
    let mut emitter = Emitter::new(json);

    emitter.emit(Event::PhaseStart { phase: Phase::Probe, total: Some(inputs.len()), carrying_forward: None });

    let mut plan = planner::build_plan(
        &inputs, &profile, &args.output, source_root,
        |current, total, path, elapsed_ms| {
            emitter.emit(Event::FileComplete {
                phase: Phase::Probe,
                current,
                total,
                file: path.to_string_lossy().into_owned(),
                output: None,
                elapsed_ms,
            });
        },
    )?;

    emitter.emit(Event::PhaseComplete {
        phase: Phase::Probe,
        total: Some(inputs.len()),
        jobs: Some(plan.jobs.len()),
        skipped: Some(plan.skipped_count),
        success: None,
        failed: None,
    });

    emitter.emit(Event::PhaseStart { phase: Phase::Resolve, total: None, carrying_forward: None });
    let caps = resolver::ResolvedCapabilities::detect();
    caps.validate_plan(&plan.jobs)?;
    caps.assign_adapters(&mut plan.jobs);
    emitter.emit(Event::PhaseComplete {
        phase: Phase::Resolve,
        total: None,
        jobs: None,
        skipped: None,
        success: None,
        failed: None,
    });

    let graph = planner::plan_to_graph(&plan)?;
    graph.save_to_file(&args.graph_out)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&graph)?);
    } else {
        println!(
            "graph written to {} ({} nodes, hash: {})",
            args.graph_out.display(),
            graph.nodes.len(),
            &graph.graph_hash[..16]
        );
    }

    Ok(())
}

// ── execute ───────────────────────────────────────────────────────────────────

fn cmd_execute(args: cli::ExecuteArgs, json: bool) -> Result<()> {
    let graph = graph::ExecutionGraph::load_from_file(&args.graph)?;

    if !graph.verify_hash() {
        eprintln!("warning: graph hash mismatch — graph may have been modified");
    }

    let caps = resolver::ResolvedCapabilities::detect();

    // Re-validate: adapter availability may have changed since plan time
    let dummy_jobs: Vec<planner::PlannedJob> = graph.nodes.iter().map(|n| {
        use planner::PlannedJob;
        use probe::AudioInfo;
        PlannedJob {
            source_path: n.input_path.clone(),
            source_info: AudioInfo {
                path: n.input_path.clone(),
                container: String::new(),
                codec: String::new(),
                sample_rate_hz: None,
                channels: None,
                bits_per_sample: None,
                duration_secs: None,
                bitrate_kbps: None,
                tags: std::collections::BTreeMap::new(),
            },
            output_path: n.output_path.clone(),
            params: n.params.clone(),
            assigned_adapter: Some(n.adapter.clone()),
        }
    }).collect();
    caps.validate_plan(&dummy_jobs)?;

    let mut emitter = progress::Emitter::new(json);
    let manifest = executor::execute_graph(&graph, &caps, &mut emitter, args.stop_on_error)?;

    manifest.save_to_file(&args.manifest)?;

    emitter.emit(progress::Event::Complete {
        success: manifest.success_count,
        failed: manifest.failure_count,
        total_elapsed_ms: manifest.total_elapsed_ms,
        manifest: args.manifest.to_string_lossy().into_owned(),
        carried_forward: None,
        re_encoded: None,
    });

    if json {
        println!("{}", serde_json::to_string_pretty(&manifest)?);
    }

    if manifest.failure_count > 0 {
        process::exit(2);
    }

    Ok(())
}

// ── verify ────────────────────────────────────────────────────────────────────

fn cmd_verify(args: cli::VerifyArgs, json: bool) -> Result<()> {
    let manifest = verifier::TranscodeManifest::load_from_file(&args.manifest)?;
    let results = manifest.verify();

    let ok_count = results.iter().filter(|r| {
        matches!(&r.status, verifier::VerificationStatus::Ok)
    }).count();
    let fail_count = results.len() - ok_count;

    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        for r in &results {
            match &r.status {
                verifier::VerificationStatus::Ok => {
                    println!("  ok  {}", r.output_path.display());
                }
                verifier::VerificationStatus::Missing => {
                    println!("  MISSING  {}", r.output_path.display());
                }
                verifier::VerificationStatus::HashMismatch { expected, actual } => {
                    println!(
                        "  HASH MISMATCH  {} (expected {}, got {})",
                        r.output_path.display(),
                        &expected[..16],
                        &actual[..16]
                    );
                }
                verifier::VerificationStatus::SizeMismatch { expected, actual } => {
                    println!(
                        "  SIZE MISMATCH  {} (expected {} bytes, got {})",
                        r.output_path.display(),
                        expected,
                        actual
                    );
                }
                verifier::VerificationStatus::OriginallyFailed { error } => {
                    println!(
                        "  ORIGINALLY FAILED  {}: {error}",
                        r.output_path.display()
                    );
                }
                verifier::VerificationStatus::CarriedForward => {
                    println!("  carried  {}", r.output_path.display());
                }
            }
        }
        println!("\n{ok_count} ok, {fail_count} failed");
    }

    if fail_count > 0 {
        process::exit(2);
    }

    Ok(())
}

// ── resume ────────────────────────────────────────────────────────────────────

fn cmd_resume(args: cli::ResumeArgs, json: bool) -> Result<()> {
    use progress::{Emitter, Event, Phase};

    let prior = verifier::TranscodeManifest::load_from_file(&args.manifest)?;

    if !prior.graph.verify_hash() {
        eprintln!("warning: graph hash mismatch in manifest — graph may have been modified");
    }

    let caps = resolver::ResolvedCapabilities::detect();

    // Build dummy jobs for capability validation using the graph nodes
    let dummy_jobs: Vec<planner::PlannedJob> = prior.graph.nodes.iter().map(|n| {
        use probe::AudioInfo;
        planner::PlannedJob {
            source_path: n.input_path.clone(),
            source_info: AudioInfo {
                path: n.input_path.clone(),
                container: String::new(),
                codec: String::new(),
                sample_rate_hz: None,
                channels: None,
                bits_per_sample: None,
                duration_secs: None,
                bitrate_kbps: None,
                tags: std::collections::BTreeMap::new(),
            },
            output_path: n.output_path.clone(),
            params: n.params.clone(),
            assigned_adapter: Some(n.adapter.clone()),
        }
    }).collect();

    let mut emitter = Emitter::new(json);

    if let Err(e) = caps.validate_plan(&dummy_jobs) {
        emitter.emit(Event::OperationFailed {
            phase: Some(Phase::Resolve),
            error: e.to_string(),
        });
        return Err(e);
    }

    let manifest = executor::resume_graph(&prior, &caps, &mut emitter, args.stop_on_error)?;

    let out_path = args.output_manifest.unwrap_or_else(|| {
        let stem = args.manifest
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        args.manifest
            .with_file_name(format!("{stem}-resumed.json"))
    });

    manifest.save_to_file(&out_path)?;

    emitter.emit(Event::Complete {
        success: manifest.success_count,
        failed: manifest.failure_count,
        total_elapsed_ms: manifest.total_elapsed_ms,
        manifest: out_path.to_string_lossy().into_owned(),
        carried_forward: Some(manifest.carried_forward_count),
        re_encoded: Some(manifest.success_count),
    });

    if json {
        println!("{}", serde_json::to_string_pretty(&manifest)?);
    }

    if manifest.failure_count > 0 {
        process::exit(2);
    }

    Ok(())
}

// ── probe ─────────────────────────────────────────────────────────────────────

fn cmd_probe(args: cli::ProbeArgs, json: bool) -> Result<()> {
    let info = probe::probe_file(&args.file)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!("path:        {}", info.path.display());
        println!("container:   {}", info.container);
        println!("codec:       {}", info.codec);
        if let Some(sr) = info.sample_rate_hz {
            println!("sample rate: {sr} Hz");
        }
        if let Some(ch) = info.channels {
            println!("channels:    {ch}");
        }
        if let Some(bps) = info.bits_per_sample {
            println!("bit depth:   {bps}");
        }
        if let Some(dur) = info.duration_secs {
            println!("duration:    {:.1}s ({:.0}m{:.0}s)",
                dur,
                (dur / 60.0).floor(),
                dur % 60.0
            );
        }
        if let Some(br) = info.bitrate_kbps {
            println!("bitrate:     ~{br} kbps");
        }
        if !info.tags.is_empty() {
            println!("tags:");
            for (k, v) in &info.tags {
                println!("  {k}: {v}");
            }
        }
    }

    Ok(())
}

// ── profiles ──────────────────────────────────────────────────────────────────

fn cmd_profiles(args: cli::ProfilesArgs, json: bool) -> Result<()> {
    let all = profiles::list_builtin_profiles();

    let filtered: Vec<_> = if let Some(filter) = &args.filter {
        all.iter()
            .filter(|(id, _)| id.starts_with(filter.as_str()))
            .collect()
    } else {
        all.iter().collect()
    };

    if json {
        let ids: Vec<&str> = filtered.iter().map(|(id, _)| id.as_str()).collect();
        println!("{}", serde_json::to_string_pretty(&ids)?);
    } else if args.detail {
        for (id, _) in &filtered {
            match profiles::DeviceProfile::load_by_id(id, &[]) {
                Ok(p) => {
                    println!("── {} ──", p.id);
                    println!("  name:      {}", p.name);
                    if let Some(v) = &p.vendor { println!("  vendor:    {v}"); }
                    println!("  media:     {:?}", p.media_type);
                    println!("  audio:     {}", p.audio_codec);
                    if let Some(br) = p.audio_bitrate_kbps { println!("  a-bitrate: {br} kbps"); }
                    if let Some(vc) = &p.video_codec { println!("  video:     {vc}"); }
                    if let Some(vbr) = p.video_bitrate_kbps { println!("  v-bitrate: {vbr} kbps"); }
                    if let (Some(w), Some(h)) = (p.width, p.height) { println!("  res:       {w}x{h}"); }
                    println!("  container: {}", p.container);
                    if let Some(sr) = p.sample_rate_hz { println!("  samplerate:{sr} Hz"); }
                    println!("  cbr:       {}", p.cbr);
                    if let Some(n) = &p.notes { println!("  notes:     {n}"); }
                    println!();
                }
                Err(e) => eprintln!("  {id}: error loading profile: {e}"),
            }
        }
    } else {
        println!("{:<25} {}", "ID", "NAME");
        println!("{}", "─".repeat(60));
        for (id, name) in &filtered {
            println!("{id:<25} {name}");
        }
    }

    Ok(())
}

// ── check ─────────────────────────────────────────────────────────────────────

fn cmd_check(json: bool) -> Result<()> {
    let caps = resolver::ResolvedCapabilities::detect();

    if json {
        let info: serde_json::Value = serde_json::json!({
            "ffmpeg": caps.has_adapter("ffmpeg"),
            "atrac": caps.has_adapter("atrac"),
        });
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        resolver::print_capability_summary(&caps);
    }

    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Build a DeviceProfile from either a named profile or explicit codec/container flags.
fn resolve_profile(
    profile_id: &Option<String>,
    codec: &Option<String>,
    container: &Option<String>,
    extension: &Option<String>,
    bitrate: Option<u32>,
    sample_rate: Option<u32>,
    channels: Option<u8>,
    cbr: bool,
    profile_dirs: &[PathBuf],
) -> Result<profiles::DeviceProfile> {
    if let Some(id) = profile_id {
        return profiles::DeviceProfile::load_by_id(id, profile_dirs);
    }

    let codec = codec.as_deref().ok_or_else(|| {
        anyhow::anyhow!("either --profile or --codec must be specified")
    })?;
    // container defaults to codec (e.g. --codec mp3 → container "mp3")
    let container = container.as_deref().unwrap_or(codec);
    // extension defaults to container (e.g. container "m4a" → extension "m4a")
    let extension = extension.as_deref().unwrap_or(container);

    Ok(profiles::DeviceProfile {
        id: "custom".to_string(),
        name: format!("Custom ({codec})"),
        vendor: None,
        description: "Manually specified format".to_string(),
        media_type: MediaType::Audio,
        container: container.to_string(),
        audio_codec: codec.to_string(),
        audio_bitrate_kbps: bitrate,
        sample_rate_hz: sample_rate,
        channels,
        video_codec: None,
        video_bitrate_kbps: None,
        width: None,
        height: None,
        frame_rate: None,
        pixel_format: None,
        cbr,
        extension: extension.to_string(),
        notes: None,
    })
}

/// Expand a list of paths: files are used directly, directories are
/// walked recursively for known audio extensions.
fn expand_inputs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    const AUDIO_EXTENSIONS: &[&str] = &[
        "mp3", "flac", "m4a", "aac", "ogg", "opus", "wav", "aiff", "aif",
        "wma", "ape", "wv", "mka", "mp2", "mp1",
    ];

    let mut files = Vec::new();

    for input in inputs {
        if input.is_file() {
            files.push(input.clone());
        } else if input.is_dir() {
            for entry in walkdir(input)? {
                let ext = entry
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if AUDIO_EXTENSIONS.contains(&ext.as_str()) {
                    files.push(entry);
                }
            }
        } else {
            return Err(anyhow::anyhow!(
                "input not found: {}",
                input.display()
            ));
        }
    }

    // Sort for stable, deterministic ordering across runs
    files.sort();
    Ok(files)
}

fn walkdir(dir: &PathBuf) -> Result<Vec<PathBuf>> {
    let mut results = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            results.extend(walkdir(&path)?);
        } else {
            results.push(path);
        }
    }
    Ok(results)
}

fn common_prefix(paths: &[PathBuf]) -> Option<&std::path::Path> {
    let first = paths.first()?.parent()?;
    let prefix = paths.iter().skip(1).fold(first, |acc, p| {
        let parent = p.parent().unwrap_or(p.as_path());
        // Walk up until we find a common ancestor
        let mut a = acc;
        loop {
            if parent.starts_with(a) {
                return a;
            }
            match a.parent() {
                Some(p) => a = p,
                None => return a,
            }
        }
    });
    Some(prefix)
}
