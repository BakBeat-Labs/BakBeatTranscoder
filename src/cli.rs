// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "bbt",
    version,
    about = "BakBeat Transcoder — deterministic audio transcoding",
    long_about = "Deterministic audio transcoder for BakBeat device sync.\n\
                  Produces identical output for identical inputs and parameters.\n\
                  Source files are never modified."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Output machine-readable JSON instead of human-readable text.
    /// Exit code 0 = success, non-zero = error.
    #[arg(long, global = true)]
    pub json: bool,

    /// Log verbosity (off, error, warn, info, debug, trace)
    #[arg(long, global = true, default_value = "warn", env = "BBT_LOG")]
    pub log_level: String,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Transcode audio files: probe → plan → resolve → execute → verify.
    /// Produces a manifest.json in the output directory.
    Transcode(TranscodeArgs),

    /// Build an execution graph without encoding.
    /// Saves a graph.json you can inspect or pass to `execute`.
    Plan(PlanArgs),

    /// Execute a previously generated graph.json.
    Execute(ExecuteArgs),

    /// Verify artifacts against a manifest.json.
    Verify(VerifyArgs),

    /// Resume a previous run: re-encode failed or missing artifacts, carry forward intact ones.
    Resume(ResumeArgs),

    /// Probe an audio file and show its format, codec, and metadata.
    Probe(ProbeArgs),

    /// List available device profiles.
    Profiles(ProfilesArgs),

    /// Check which encoder backends are available on this system.
    Check,
}

#[derive(Args, Debug)]
pub struct TranscodeArgs {
    /// Input audio files or directories (directories are searched recursively)
    #[arg(required = true, num_args = 1..)]
    pub inputs: Vec<PathBuf>,

    /// Device profile (e.g. minidisc-lp2, generic-mp3-320).
    /// Run `bbt profiles` to list all available profiles.
    #[arg(short, long, conflicts_with = "codec")]
    pub profile: Option<String>,

    /// Output codec (e.g. mp3, aac, flac, vorbis, opus, alac, wma, atrac3).
    #[arg(long, conflicts_with = "profile")]
    pub codec: Option<String>,

    /// Output container (e.g. mp3, m4a, ogg, wav, asf, oma).
    /// Defaults to the codec value when not specified.
    #[arg(long, conflicts_with = "profile")]
    pub container: Option<String>,

    /// Output file extension. Defaults to container when not specified.
    #[arg(long, conflicts_with = "profile")]
    pub extension: Option<String>,

    /// Output bitrate in kbps (for lossy codecs)
    #[arg(long)]
    pub bitrate: Option<u32>,

    /// Output sample rate in Hz (default: preserve source)
    #[arg(long)]
    pub sample_rate: Option<u32>,

    /// Output channel count (default: preserve source)
    #[arg(long)]
    pub channels: Option<u8>,

    /// Force constant bitrate encoding (default: true for lossy codecs)
    #[arg(long, default_value = "true")]
    pub cbr: bool,

    /// Output directory (default: ./transcoded)
    #[arg(short, long, default_value = "transcoded")]
    pub output: PathBuf,

    /// Source root for computing relative output paths.
    /// Defaults to the common directory prefix of all inputs.
    #[arg(long)]
    pub source_root: Option<PathBuf>,

    /// Where to save the manifest (default: <output>/manifest.json)
    #[arg(long)]
    pub manifest: Option<PathBuf>,

    /// Additional profile directories to search
    #[arg(long)]
    pub profile_dir: Vec<PathBuf>,

    /// Abort the entire batch on the first encode error
    #[arg(long)]
    pub stop_on_error: bool,

    /// Disable the "already in target format" skip check: always encode every
    /// input to the requested spec, or fail. Use this when you (the caller)
    /// have already decided an encode is required — e.g. re-rating an MP3 to
    /// a lower bitrate, where source and target codec match but bitrate does
    /// not. Without this flag, bbt treats matching codec+container as "already
    /// satisfies" and skips the file, which is wrong for same-codec re-rates.
    #[arg(long)]
    pub no_skip: bool,
}

#[derive(Args, Debug)]
pub struct PlanArgs {
    #[arg(required = true, num_args = 1..)]
    pub inputs: Vec<PathBuf>,

    #[arg(short, long, conflicts_with = "codec")]
    pub profile: Option<String>,

    /// Output codec (e.g. mp3, aac, flac, vorbis, opus, alac, wma, atrac3).
    #[arg(long, conflicts_with = "profile")]
    pub codec: Option<String>,

    /// Output container (e.g. mp3, m4a, ogg, wav, asf, oma).
    /// Defaults to the codec value when not specified.
    #[arg(long, conflicts_with = "profile")]
    pub container: Option<String>,

    /// Output file extension. Defaults to container when not specified.
    #[arg(long, conflicts_with = "profile")]
    pub extension: Option<String>,

    #[arg(long)]
    pub bitrate: Option<u32>,

    #[arg(long)]
    pub sample_rate: Option<u32>,

    #[arg(long)]
    pub channels: Option<u8>,

    #[arg(long, default_value = "true")]
    pub cbr: bool,

    #[arg(short, long, default_value = "transcoded")]
    pub output: PathBuf,

    #[arg(long)]
    pub source_root: Option<PathBuf>,

    /// Where to save the graph (default: graph.json)
    #[arg(long, default_value = "graph.json")]
    pub graph_out: PathBuf,

    #[arg(long)]
    pub profile_dir: Vec<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ExecuteArgs {
    /// Path to a graph.json produced by `bbt plan`
    pub graph: PathBuf,

    /// Where to save the manifest (default: manifest.json)
    #[arg(long, default_value = "manifest.json")]
    pub manifest: PathBuf,

    #[arg(long)]
    pub stop_on_error: bool,
}

#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Path to a manifest.json to verify
    pub manifest: PathBuf,
}

#[derive(Args, Debug)]
pub struct ResumeArgs {
    /// Manifest from a previous run. The graph is loaded from inside it.
    pub manifest: PathBuf,

    /// Where to save the new manifest (default: <original dir>/manifest-resumed.json)
    #[arg(long)]
    pub output_manifest: Option<PathBuf>,

    #[arg(long)]
    pub stop_on_error: bool,
}

#[derive(Args, Debug)]
pub struct ProbeArgs {
    /// Audio file to probe
    pub file: PathBuf,
}

#[derive(Args, Debug)]
pub struct ProfilesArgs {
    /// Show full profile details instead of a summary list
    #[arg(long)]
    pub detail: bool,

    /// Filter by profile ID prefix
    pub filter: Option<String>,
}
