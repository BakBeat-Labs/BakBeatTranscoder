# BakBeat Transcoder

Deterministic audio and video transcoder for device sync. Part of the [BakBeat](https://github.com/BakBeat) ecosystem, open source under MPL-2.0.

The core guarantee: identical inputs with identical parameters always produce identical outputs. Every run is fully planned before any encoding starts, and every output is verified against a SHA-256 manifest.

---

## How it works

Transcoding runs through five phases in order:

```
probe → plan → resolve → encode → verify
```

**Probe** — each input file is inspected natively (Symphonia for audio, ffprobe for video). Format, codec, sample rate, resolution, and metadata are read without touching the file.

**Plan** — inputs are matched against the target profile. Files already in the correct format are skipped. Everything else gets a fully resolved job: all encoding parameters explicit, output paths determined.

**Resolve** — the system checks that every required encoder backend is available before any work begins. If anything is missing, the entire batch fails here — no partial runs.

**Encode** — jobs execute sequentially. Each produces an artifact with its SHA-256 hash recorded.

**Verify** — every output is checked against its recorded hash. The result is written to a manifest JSON file.

The manifest is the ground truth record of what was produced. You can re-verify it at any time with `bbt verify`, and re-run any failures with `bbt resume`.

---

## Prerequisites

**Required for most transcoding:**
- [FFmpeg](https://ffmpeg.org) — must be in your PATH. Handles MP3, AAC, FLAC, OGG, Opus, ALAC, WAV, and video encoding.

**Required for MiniDisc ATRAC encoding:**
- [atracdenc](https://github.com/dcherednik/atracdenc) — open source ATRAC encoder, must be in your PATH.

**Required for video probing:**
- ffprobe — ships with FFmpeg, no separate install needed.

---

## Building

```bash
# Clone and build
git clone https://github.com/BakBeat/BakBeatTranscoder
cd BakBeatTranscoder
cargo build --release

# The binary is at target/release/bbt
# Optionally install it
cargo install --path .
```

Requires Rust 1.75 or later. Cross-platform: macOS, Linux, Windows.

---

## Quick start

```bash
# Transcode a folder of FLAC files to MP3 320 kbps
bbt transcode ~/Music/Artist/ --profile generic-mp3-320 --output ~/Transcoded/

# Transcode for MiniDisc LP2
bbt transcode ~/Music/ --profile minidisc-lp2 --output ~/ForMinidisc/

# Specify format manually without a profile
bbt transcode track.flac --codec mp3 --bitrate 192 --output ./out/

# Probe a file to see its format and metadata
bbt probe track.flac
bbt probe video.mp4

# Check what encoder backends are available
bbt check

# List available device profiles
bbt profiles
bbt profiles --detail
```

---

## Commands

### `bbt transcode`

The main command. Runs all five phases in sequence and writes a `manifest.json` to the output directory.

```bash
bbt transcode <inputs...> --profile <id> --output <dir>
bbt transcode <inputs...> --codec <codec> [--container <fmt>] [--bitrate <kbps>] --output <dir>
```

| Flag | Description |
|---|---|
| `--profile <id>` | Use a built-in or custom device profile |
| `--codec <codec>` | Audio codec: `mp3`, `aac`, `flac`, `vorbis`, `opus`, `alac`, `atrac3` |
| `--container <fmt>` | Container format. Defaults to codec value. |
| `--extension <ext>` | Output file extension. Defaults to container. |
| `--bitrate <kbps>` | Audio bitrate in kbps |
| `--sample-rate <hz>` | Output sample rate. Defaults to source. |
| `--channels <n>` | Output channels. Defaults to source. |
| `--cbr` | Force constant bitrate (default: true) |
| `--output <dir>` | Output directory (default: `./transcoded`) |
| `--source-root <dir>` | Root for computing relative output paths |
| `--manifest <file>` | Where to save the manifest (default: `<output>/manifest.json`) |
| `--stop-on-error` | Abort the entire batch on the first failure |
| `--json` | Emit NDJSON progress events to stdout (see [JSON output](#json-output)) |

Relative directory structure is preserved. If you transcode `Music/Artist/Album/track.flac` with `--source-root Music/` and `--output Transcoded/`, the output is `Transcoded/Artist/Album/track.mp3`.

---

### `bbt plan`

Build an execution graph without encoding. Saves a `graph.json` you can inspect or pass to `bbt execute` later.

```bash
bbt plan ~/Music/ --profile generic-mp3-320 --output ~/Out/ --graph-out plan.json
```

The graph JSON contains every input file's SHA-256, all resolved encoding parameters, and the graph's own hash. If any parameter changes, the hash changes.

---

### `bbt execute`

Run a previously generated graph.

```bash
bbt execute plan.json --manifest manifest.json
```

Re-validates encoder availability before starting. Warns if the graph hash has changed since it was created.

---

### `bbt resume`

Re-run a previous operation, skipping files that are still intact.

```bash
bbt resume manifest.json
```

For each artifact in the manifest:
- If the output file exists and its SHA-256 still matches → **carried forward**, not re-encoded
- If the file is missing, failed, or the hash has drifted → **re-encoded**

Produces a new manifest with `resumed_from` set to the original manifest's ID, creating an audit chain.

---

### `bbt verify`

Check all artifacts in a manifest against their recorded SHA-256 hashes.

```bash
bbt verify manifest.json
```

Exits with code 2 if any artifact is missing, size-mismatched, or hash-mismatched.

---

### `bbt probe`

Inspect a media file. Uses Symphonia for audio (in-process) and ffprobe for video.

```bash
bbt probe track.flac
bbt probe video.mp4
bbt probe track.flac --json
```

---

### `bbt profiles`

List available device profiles.

```bash
bbt profiles              # summary list
bbt profiles --detail     # full parameters for each profile
bbt profiles minidisc     # filter by ID prefix
```

---

### `bbt check`

Report which encoder backends are available on this system.

```bash
bbt check
bbt check --json
```

---

## Device profiles

Profiles are TOML files that declare what format, codec, and parameters a target device requires. Built-in profiles cover common devices; you can also supply your own.

### Built-in profiles

| ID | Format | Use case |
|---|---|---|
| `minidisc-sp` | ATRAC1 ~292 kbps | MiniDisc Standard Play |
| `minidisc-lp2` | ATRAC3 132 kbps | MiniDisc LP2 (2× time) |
| `minidisc-lp4` | ATRAC3 66 kbps | MiniDisc LP4 (4× time) |
| `himd-sp` | ATRAC3+ 256 kbps | HiMD Standard Play |
| `generic-mp3-128` | MP3 128 kbps CBR | Maximum device compatibility |
| `generic-mp3-192` | MP3 192 kbps CBR | Good balance of quality and size |
| `generic-mp3-320` | MP3 320 kbps CBR | Maximum MP3 quality |
| `generic-aac-128` | AAC-LC 128 kbps | iOS, Android, modern players |
| `generic-aac-256` | AAC-LC 256 kbps | Transparent AAC quality |
| `generic-flac` | FLAC lossless | Archival or lossless-capable devices |
| `generic-ogg-192` | Vorbis 192 kbps CBR | Rockbox and Ogg-capable players |

### Writing a custom profile

Create a `.toml` file and pass its directory with `--profile-dir`:

```toml
# profiles/my-player.toml
id          = "my-player"
name        = "My MP3 Player"
description = "Cheap MP3 player that only accepts 128 kbps CBR MP3"
container   = "mp3"
audio_codec = "mp3"
audio_bitrate_kbps = 128
sample_rate_hz = 44100   # omit to preserve source sample rate
channels    = 2          # omit to preserve source channels
cbr         = true
extension   = "mp3"
```

```bash
bbt transcode ~/Music/ --profile my-player --profile-dir ./profiles/ --output ~/Out/
```

### Video profile example

```toml
# profiles/ipod-video.toml
id          = "ipod-video-5g"
name        = "iPod 5th Generation Video"
vendor      = "Apple"
media_type  = "video"
container   = "mp4"
video_codec = "h264"
video_bitrate_kbps = 1500
width       = 640
height      = 480
pixel_format = "yuv420p"
audio_codec = "aac"
audio_bitrate_kbps = 128
sample_rate_hz = 44100
channels    = 2
cbr         = true
extension   = "mp4"
notes       = "H.264 Baseline Level 3.0, 640x480 max for 5th gen iPod."
```

Omitting `width`, `height`, or `frame_rate` preserves the source file's values.

---

## JSON output

Pass `--json` to any command to receive machine-readable NDJSON on stdout — one complete JSON event per line. Designed for programmatic consumers like BakBeat's status lane.

```bash
bbt transcode input.flac --profile generic-mp3-320 --output ./out/ --json
```

```json
{"type":"phase_start","phase":"probe","total":1}
{"type":"file_complete","phase":"probe","current":1,"total":1,"file":"input.flac","elapsed_ms":12}
{"type":"phase_complete","phase":"probe","total":1}
{"type":"phase_start","phase":"plan"}
{"type":"phase_complete","phase":"plan","jobs":1,"skipped":0}
{"type":"phase_start","phase":"resolve"}
{"type":"phase_complete","phase":"resolve"}
{"type":"phase_start","phase":"encode","total":1}
{"type":"encode_start","current":1,"total":1,"file":"input.flac","output":"out/input.mp3"}
{"type":"file_complete","phase":"encode","current":1,"total":1,"file":"input.flac","output":"out/input.mp3","elapsed_ms":843}
{"type":"phase_complete","phase":"encode","success":1,"failed":0}
{"type":"complete","success":1,"failed":0,"total_elapsed_ms":860,"manifest":"out/manifest.json"}
```

**Failure events** are dedicated types, not boolean flags:

```json
{"type":"file_failed","phase":"encode","current":2,"total":5,"file":"bad.flac","error":"..."}
{"type":"operation_failed","phase":"resolve","error":"FFmpeg is required but not found in PATH"}
```

**Resume events** include carry-forward counts:

```json
{"type":"phase_start","phase":"encode","total":2,"carrying_forward":3}
{"type":"complete","success":2,"failed":0,"carried_forward":3,"re_encoded":2,...}
```

**Exit codes:** `0` = success, `1` = error (bad arguments, missing files, etc.), `2` = one or more encodes failed.

---

## Manifest format

Every run produces a manifest JSON. Keep it alongside your transcoded files.

```json
{
  "schema_version": "1.0",
  "manifest_id": "a3f2c1d4-...",
  "completed_at": "2025-06-05T19:00:00Z",
  "success_count": 12,
  "failure_count": 0,
  "carried_forward_count": 0,
  "graph": { ... },
  "artifacts": [
    {
      "node_id": "...",
      "output_path": "out/track.mp3",
      "sha256": "e3b0c44298fc...",
      "size_bytes": 8421376,
      "encode_elapsed_ms": 843,
      "status": { "type": "success" }
    }
  ]
}
```

Re-verify at any time: `bbt verify manifest.json`

Re-run failures: `bbt resume manifest.json`

---

## License

Mozilla Public License 2.0 — see [LICENSE](LICENSE).

FFmpeg and ffprobe are called as external subprocesses and are not bundled. Users provide their own installation. atracdenc is similarly external. This keeps licensing clean: MPL-2.0 applies to this codebase only.
