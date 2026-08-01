// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "bbt",
    version,
    about = "BakBeat Transcoder — structured media transcoding",
    long_about = "Structured transcoder for BakBeat device sync.\n\
                  BakBeat supplies the requested artifact spec; bbt executes it.\n\
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
    /// Transcode media files: probe → plan → resolve → execute → verify.
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

    /// Probe a media file and show its format, codec, and metadata.
    Probe(ProbeArgs),

    /// Check which encoder backends are available on this system.
    Check,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliMediaType {
    Audio,
    Video,
}

#[derive(Args, Debug)]
pub struct TranscodeArgs {
    /// Input media files or directories (directories are searched recursively)
    #[arg(required = true, num_args = 1..)]
    pub inputs: Vec<PathBuf>,

    /// Requested media type.
    #[arg(long, value_enum, default_value = "audio")]
    pub media: CliMediaType,

    /// Output audio codec. `--codec` is kept as the short caller-facing alias.
    #[arg(long, alias = "audio-codec")]
    pub codec: Option<String>,

    /// Output container (e.g. mp3, m4a, ogg, wav, asf, oma).
    /// Defaults to the codec value when not specified.
    #[arg(long)]
    pub container: Option<String>,

    /// Output file extension. Defaults to container when not specified.
    #[arg(long)]
    pub extension: Option<String>,

    /// Output audio bitrate in kbps. `--bitrate` is kept for existing callers.
    #[arg(long)]
    pub bitrate: Option<u32>,

    /// Output sample rate in Hz (default: preserve source)
    #[arg(long)]
    pub sample_rate: Option<u32>,

    /// Output channel count (default: preserve source)
    #[arg(long)]
    pub channels: Option<u8>,

    /// Output video codec for video requests.
    #[arg(long)]
    pub video_codec: Option<String>,

    /// Output video bitrate in kbps.
    #[arg(long)]
    pub video_bitrate: Option<u32>,

    /// Output video width in pixels. Defaults to preserve source.
    #[arg(long)]
    pub width: Option<u32>,

    /// Output video height in pixels. Defaults to preserve source.
    #[arg(long)]
    pub height: Option<u32>,

    /// Output frame rate. Defaults to preserve source.
    #[arg(long)]
    pub frame_rate: Option<f32>,

    /// FFmpeg pixel format, e.g. yuv420p. Defaults to adapter choice.
    #[arg(long)]
    pub pixel_format: Option<String>,

    /// Emit only the primary audio stream; do not copy cover art or side streams.
    #[arg(long)]
    pub audio_only: bool,

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

    /// Requested media type.
    #[arg(long, value_enum, default_value = "audio")]
    pub media: CliMediaType,

    /// Output audio codec. `--codec` is kept as the short caller-facing alias.
    #[arg(long, alias = "audio-codec")]
    pub codec: Option<String>,

    /// Output container (e.g. mp3, m4a, ogg, wav, asf, oma).
    /// Defaults to the codec value when not specified.
    #[arg(long)]
    pub container: Option<String>,

    /// Output file extension. Defaults to container when not specified.
    #[arg(long)]
    pub extension: Option<String>,

    #[arg(long)]
    pub bitrate: Option<u32>,

    #[arg(long)]
    pub sample_rate: Option<u32>,

    #[arg(long)]
    pub channels: Option<u8>,

    /// Output video codec for video requests.
    #[arg(long)]
    pub video_codec: Option<String>,

    /// Output video bitrate in kbps.
    #[arg(long)]
    pub video_bitrate: Option<u32>,

    /// Output video width in pixels. Defaults to preserve source.
    #[arg(long)]
    pub width: Option<u32>,

    /// Output video height in pixels. Defaults to preserve source.
    #[arg(long)]
    pub height: Option<u32>,

    /// Output frame rate. Defaults to preserve source.
    #[arg(long)]
    pub frame_rate: Option<f32>,

    /// FFmpeg pixel format, e.g. yuv420p. Defaults to adapter choice.
    #[arg(long)]
    pub pixel_format: Option<String>,

    /// Emit only the primary audio stream; do not copy cover art or side streams.
    #[arg(long)]
    pub audio_only: bool,

    #[arg(long, default_value = "true")]
    pub cbr: bool,

    #[arg(short, long, default_value = "transcoded")]
    pub output: PathBuf,

    #[arg(long)]
    pub source_root: Option<PathBuf>,

    /// Where to save the graph (default: graph.json)
    #[arg(long, default_value = "graph.json")]
    pub graph_out: PathBuf,
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
    /// Media file to probe
    pub file: PathBuf,
}
