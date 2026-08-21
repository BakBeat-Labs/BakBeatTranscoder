// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod adapters;
mod binaries;
mod cli;
mod error;
mod executor;
mod gapless;
mod graph;
mod mp4_aac;
mod planner;
mod probe;
mod progress;
mod resolver;
mod spec;
mod verifier;

use std::path::PathBuf;
use std::process::{self, Command};

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use cli::{Cli, CliMediaType, Commands};
use graph::MediaType;
use spec::TranscodeSpec;

fn main() {
    let cli = Cli::parse();

    // Initialize tracing — respects BBT_LOG env var or --log-level flag
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cli.log_level)),
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
        Commands::AudiobookProbe(args) => cmd_audiobook_probe(args, cli.json),
        Commands::ExtractArtwork(args) => cmd_extract_artwork(args),
        Commands::Check => cmd_check(cli.json),
        Commands::Hwaccels => cmd_hwaccels(cli.json),
    }
}

// ── transcode ─────────────────────────────────────────────────────────────────

fn cmd_transcode(args: cli::TranscodeArgs, json: bool) -> Result<()> {
    use progress::{Emitter, Event, Phase};

    let spec = resolve_transcode_spec(
        args.media,
        &args.codec,
        &args.container,
        &args.extension,
        args.bitrate,
        args.sample_rate,
        args.channels,
        args.cbr,
        &args.video_codec,
        args.video_bitrate,
        args.width,
        args.height,
        args.frame_rate,
        &args.pixel_format,
        &args.video_filter,
        &args.video_profile,
        &args.video_level,
        &args.poster_artwork,
        &args.hwaccel,
        &args.movflags,
        args.audio_block_size,
        args.audio_only,
        args.aac_priming,
    )?;

    let inputs = expand_inputs(&args.inputs)?;
    if inputs.is_empty() {
        eprintln!("no audio files found in the given inputs");
        return Ok(());
    }

    let source_root = args
        .source_root
        .as_deref()
        .or_else(|| common_prefix(&inputs));
    let mut emitter = Emitter::new(json);

    // Phase 1: Probe + Plan
    emitter.emit(Event::PhaseStart {
        phase: Phase::Probe,
        total: Some(inputs.len()),
        carrying_forward: None,
    });

    let mut plan = planner::build_plan(
        &inputs,
        &spec,
        &args.output,
        args.output_file.as_deref(),
        source_root,
        args.no_skip,
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

    emitter.emit(Event::PhaseStart {
        phase: Phase::Plan,
        total: None,
        carrying_forward: None,
    });
    emitter.emit(Event::PhaseComplete {
        phase: Phase::Plan,
        total: None,
        jobs: Some(plan.jobs.len()),
        skipped: Some(plan.skipped_count),
        success: None,
        failed: None,
    });

    if plan.jobs.is_empty() {
        // With --no-skip every input must produce a job; an empty plan here
        // means the caller's spec was unactionable (e.g. no decodable audio
        // found), not "nothing to do" — fail loudly instead of exiting 0
        // with an empty output directory.
        if args.no_skip {
            let err = anyhow::anyhow!(
                "--no-skip was set but no encode jobs were planned for {} input(s) — \
                 refusing to exit 0 with an empty output directory",
                inputs.len()
            );
            emitter.emit(Event::OperationFailed {
                phase: Some(Phase::Plan),
                error: err.to_string(),
            });
            return Err(err);
        }

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
    emitter.emit(Event::PhaseStart {
        phase: Phase::Resolve,
        total: None,
        carrying_forward: None,
    });
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
    let manifest_path = args
        .manifest
        .unwrap_or_else(|| args.output.join("manifest.json"));
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

    let spec = resolve_transcode_spec(
        args.media,
        &args.codec,
        &args.container,
        &args.extension,
        args.bitrate,
        args.sample_rate,
        args.channels,
        args.cbr,
        &args.video_codec,
        args.video_bitrate,
        args.width,
        args.height,
        args.frame_rate,
        &args.pixel_format,
        &args.video_filter,
        &args.video_profile,
        &args.video_level,
        &args.poster_artwork,
        &args.hwaccel,
        &args.movflags,
        args.audio_block_size,
        args.audio_only,
        args.aac_priming,
    )?;

    let inputs = expand_inputs(&args.inputs)?;
    let source_root = args
        .source_root
        .as_deref()
        .or_else(|| common_prefix(&inputs));
    let mut emitter = Emitter::new(json);

    emitter.emit(Event::PhaseStart {
        phase: Phase::Probe,
        total: Some(inputs.len()),
        carrying_forward: None,
    });

    let mut plan = planner::build_plan(
        &inputs,
        &spec,
        &args.output,
        args.output_file.as_deref(),
        source_root,
        false,
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

    emitter.emit(Event::PhaseStart {
        phase: Phase::Resolve,
        total: None,
        carrying_forward: None,
    });
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
    let dummy_jobs: Vec<planner::PlannedJob> = graph
        .nodes
        .iter()
        .map(|n| planner::PlannedJob {
            source_path: n.input_path.clone(),
            output_path: n.output_path.clone(),
            params: n.params.clone(),
            assigned_adapter: Some(n.adapter.clone()),
            fingerprint: n.input.clone(),
        })
        .collect();
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

    let ok_count = results
        .iter()
        .filter(|r| matches!(&r.status, verifier::VerificationStatus::Ok))
        .count();
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
                verifier::VerificationStatus::Empty => {
                    println!("  EMPTY  {}", r.output_path.display());
                }
                verifier::VerificationStatus::Unreadable { error } => {
                    println!("  UNREADABLE  {}: {error}", r.output_path.display());
                }
                verifier::VerificationStatus::ShapeMismatch { detail } => {
                    println!("  SHAPE MISMATCH  {} ({detail})", r.output_path.display());
                }
                verifier::VerificationStatus::DurationMismatch {
                    expected_secs,
                    actual_secs,
                } => {
                    println!(
                        "  DURATION MISMATCH  {} (expected {:.1}s, got {:.1}s)",
                        r.output_path.display(),
                        expected_secs,
                        actual_secs
                    );
                }
                verifier::VerificationStatus::OriginallyFailed { error } => {
                    println!("  ORIGINALLY FAILED  {}: {error}", r.output_path.display());
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
    let dummy_jobs: Vec<planner::PlannedJob> = prior
        .graph
        .nodes
        .iter()
        .map(|n| planner::PlannedJob {
            source_path: n.input_path.clone(),
            output_path: n.output_path.clone(),
            params: n.params.clone(),
            assigned_adapter: Some(n.adapter.clone()),
            fingerprint: n.input.clone(),
        })
        .collect();

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
        let stem = args
            .manifest
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        args.manifest.with_file_name(format!("{stem}-resumed.json"))
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
    use probe::MediaInfo;

    let info = probe::probe_media(&args.file)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }

    match &info {
        MediaInfo::Audio(a) => {
            println!("path:        {}", a.path.display());
            println!("type:        audio");
            println!("container:   {}", a.container);
            println!("codec:       {}", a.codec);
            if let Some(sr) = a.sample_rate_hz {
                println!("sample rate: {sr} Hz");
            }
            if let Some(ch) = a.channels {
                println!("channels:    {ch}");
            }
            if let Some(bps) = a.bits_per_sample {
                println!("bit depth:   {bps}");
            }
            if let Some(dur) = a.duration_secs {
                println!(
                    "duration:    {:.1}s ({:.0}m{:.0}s)",
                    dur,
                    (dur / 60.0).floor(),
                    dur % 60.0
                );
            }
            if let Some(br) = a.bitrate_kbps {
                println!("bitrate:     ~{br} kbps");
            }
            println!(
                "artwork:     {}",
                if a.has_artwork { "present" } else { "absent" }
            );
            if let Some(profile) = &a.profile {
                println!("profile:     {profile}");
            }
            match a.priming_samples {
                Some(n) => println!("priming:     {n} samples"),
                None => println!("priming:     unknown"),
            }
            println!(
                "chapters:    {}",
                if a.has_chapters { "present" } else { "absent" }
            );
            if !a.tags.is_empty() {
                println!("tags:");
                for (k, v) in &a.tags {
                    println!("  {k}: {v}");
                }
            }
        }
        MediaInfo::Video(v) => {
            println!("path:        {}", v.path.display());
            println!("type:        video");
            println!("container:   {}", v.container);
            if let Some(dur) = v.duration_secs {
                println!(
                    "duration:    {:.1}s ({:.0}m{:.0}s)",
                    dur,
                    (dur / 60.0).floor(),
                    dur % 60.0
                );
            }
            for (i, vs) in v.video_streams.iter().enumerate() {
                println!("video[{i}]:    {} {}x{}", vs.codec, vs.width, vs.height);
                if let Some(fps) = vs.frame_rate {
                    println!("  fps:       {fps:.3}");
                }
                if let Some(br) = vs.bitrate_kbps {
                    println!("  bitrate:   ~{br} kbps");
                }
                if let Some(pf) = &vs.pixel_format {
                    println!("  pixel fmt: {pf}");
                }
            }
            for (i, aus) in v.audio_streams.iter().enumerate() {
                println!("audio[{i}]:    {}", aus.codec);
                if let Some(sr) = aus.sample_rate_hz {
                    println!("  sample rate: {sr} Hz");
                }
                if let Some(ch) = aus.channels {
                    println!("  channels:    {ch}");
                }
                if let Some(br) = aus.bitrate_kbps {
                    println!("  bitrate:     ~{br} kbps");
                }
            }
            if !v.tags.is_empty() {
                println!("tags:");
                for (k, val) in &v.tags {
                    println!("  {k}: {val}");
                }
            }
        }
    }

    Ok(())
}

fn cmd_audiobook_probe(args: cli::ProbeArgs, _json: bool) -> Result<()> {
    let facts = probe::probe_audiobook_facts(&args.file)?;
    println!("{}", serde_json::to_string_pretty(&facts)?);
    Ok(())
}

fn cmd_extract_artwork(args: cli::ExtractArtworkArgs) -> Result<()> {
    let ffmpeg = binaries::find_ffmpeg()
        .ok_or_else(|| anyhow::anyhow!("ffmpeg not found; cannot extract artwork"))?;
    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let output = Command::new(ffmpeg)
        .args(["-y", "-i"])
        .arg(&args.input)
        .args(["-an", "-vcodec", "copy"])
        .arg(&args.output)
        .output()?;

    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        process::exit(output.status.code().unwrap_or(1));
    }

    Ok(())
}

// ── check ─────────────────────────────────────────────────────────────────────

fn cmd_check(json: bool) -> Result<()> {
    let bins = binaries::BinaryPaths::detect();
    let caps = resolver::ResolvedCapabilities::detect();

    if json {
        let info: serde_json::Value = serde_json::json!({
            "ffmpeg":    { "available": bins.ffmpeg.is_some(),   "path": bins.ffmpeg },
            "ffprobe":   { "available": bins.ffprobe.is_some(),  "path": bins.ffprobe },
            "atracdenc": { "available": bins.atracdenc.is_some(),"path": bins.atracdenc },
        });
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        let show = |name: &str, path: &Option<std::path::PathBuf>| match path {
            Some(p) => println!("  {name:<12} found    {}", p.display()),
            None => println!("  {name:<12} NOT FOUND"),
        };
        println!("Binaries:");
        show("ffmpeg", &bins.ffmpeg);
        show("ffprobe", &bins.ffprobe);
        show("atracdenc", &bins.atracdenc);
        println!("\nEncoder adapters:");
        for (name, adapter) in &caps.adapters {
            println!(
                "  {name}: available ({})",
                adapter.supported_output_codecs().join(", ")
            );
        }
        for name in ["ffmpeg", "atrac"] {
            if !caps.adapters.contains_key(name) {
                println!("  {name}: not available");
            }
        }
    }

    Ok(())
}

fn cmd_hwaccels(json: bool) -> Result<()> {
    let ffmpeg = binaries::find_ffmpeg()
        .ok_or_else(|| anyhow::anyhow!("ffmpeg not found; cannot query hardware acceleration"))?;
    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-hwaccels"])
        .output()?;
    if !output.status.success() {
        process::exit(output.status.code().unwrap_or(1));
    }

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let methods: Vec<String> = combined
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.ends_with(':'))
        .map(ToOwned::to_owned)
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&methods)?);
    } else {
        for method in methods {
            println!("{method}");
        }
    }

    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn resolve_transcode_spec(
    media: CliMediaType,
    codec: &Option<String>,
    container: &Option<String>,
    extension: &Option<String>,
    bitrate: Option<u32>,
    sample_rate: Option<u32>,
    channels: Option<u8>,
    cbr: bool,
    video_codec: &Option<String>,
    video_bitrate: Option<u32>,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<f32>,
    pixel_format: &Option<String>,
    video_filter: &Option<String>,
    video_profile: &Option<String>,
    video_level: &Option<String>,
    poster_artwork: &Option<PathBuf>,
    hwaccel: &Option<String>,
    movflags: &Option<String>,
    audio_block_size: Option<u32>,
    audio_only: bool,
    aac_priming: Option<u32>,
) -> Result<TranscodeSpec> {
    let codec = codec
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--codec must be specified"))?;
    if matches!(media, CliMediaType::Video) && video_codec.is_none() {
        return Err(anyhow::anyhow!(
            "--video-codec must be specified for --media video"
        ));
    }
    // container defaults to codec (e.g. --codec mp3 → container "mp3")
    let container = container.as_deref().unwrap_or(codec);
    // extension defaults to container (e.g. container "m4a" → extension "m4a")
    let extension = extension.as_deref().unwrap_or(container);

    Ok(TranscodeSpec {
        media_type: match media {
            CliMediaType::Audio => MediaType::Audio,
            CliMediaType::Video => MediaType::Video,
        },
        container: container.to_string(),
        audio_codec: codec.to_string(),
        audio_bitrate_kbps: bitrate,
        sample_rate_hz: sample_rate,
        channels,
        video_codec: video_codec.clone(),
        video_bitrate_kbps: video_bitrate,
        width,
        height,
        frame_rate,
        pixel_format: pixel_format.clone(),
        video_filter: video_filter.clone(),
        video_profile: video_profile.clone(),
        video_level: video_level.clone(),
        poster_artwork_path: poster_artwork.clone(),
        hwaccel: hwaccel.clone(),
        movflags: movflags.clone(),
        audio_block_size,
        cbr,
        extension: extension.to_string(),
        preserve_artwork: !audio_only,
        aac_priming,
    })
}

/// Expand a list of paths: files are used directly, directories are
/// walked recursively for known media extensions.
fn expand_inputs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    const MEDIA_EXTENSIONS: &[&str] = &[
        "mp3", "flac", "m4a", "m4b", "aac", "ogg", "opus", "wav", "aiff", "aif", "wma", "ape",
        "wv", "mka", "mp2", "mp1", "mp4", "m4v", "mov", "avi", "mkv", "webm", "wmv", "mpg", "mpeg",
        "3gp", "3g2",
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
                if MEDIA_EXTENSIONS.contains(&ext.as_str()) {
                    files.push(entry);
                }
            }
        } else {
            return Err(anyhow::anyhow!("input not found: {}", input.display()));
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
