# BakBeat Transcoder

`bbt` is BakBeat's structured media transcoder. It is public for licensing and
third-party-tool boundary reasons, but it is designed as a BakBeat runtime
component, not as a general end-user transcoding product.

BakBeat owns device policy. `bbt` owns execution:

```text
BakBeat decides device, policy, format, destination, and user intent.
bbt receives a concrete requested artifact spec.
bbt selects the required adapter, runs it, and reports structured phases/errors.
```

## Contract

`bbt` should answer one question:

> Given this input and this exact requested output shape, can you produce it?

It should not decide which device profile applies, which optimization policy is
appropriate, where device files belong, or whether a user wanted a conversion.
Those decisions belong in BakBeat.

## Basic Usage

Audio:

```bash
bbt transcode "track.flac" \
  --codec mp3 \
  --container mp3 \
  --extension mp3 \
  --bitrate 128 \
  --sample-rate 44100 \
  --channels 2 \
  --output ./out \
  --json
```

Video:

```bash
bbt transcode "movie.mov" \
  --media video \
  --codec aac \
  --bitrate 128 \
  --video-codec h264 \
  --video-bitrate 768 \
  --width 480 \
  --height 272 \
  --container mp4 \
  --extension m4v \
  --output ./out \
  --json
```

Audio-only output, with cover art and side streams stripped by `bbt` instead of
by a caller-side `ffmpeg` remux:

```bash
bbt transcode "track.m4a" \
  --codec aac \
  --container m4a \
  --extension m4a \
  --audio-only \
  --output ./out \
  --json
```

## Commands

### `bbt transcode`

Runs the full execution path:

```text
probe -> plan -> resolve -> encode -> complete
```

The plan step is internal plumbing: it resolves caller-supplied specs against
source facts, such as preserving source sample rate/channels when the caller did
not supply them. It is not device-profile selection.

Useful flags:

| Flag | Meaning |
| --- | --- |
| `--media audio\|video` | Requested media kind. Defaults to `audio`. |
| `--codec <codec>` | Requested audio codec. |
| `--container <container>` | Requested output container. Defaults to `--codec`. |
| `--extension <extension>` | Requested file extension. Defaults to container. |
| `--bitrate <kbps>` | Requested audio bitrate. |
| `--sample-rate <hz>` | Requested audio sample rate. Omit to preserve source when available. |
| `--channels <count>` | Requested channel count. Omit to preserve source when available. |
| `--video-codec <codec>` | Requested video codec. Required with `--media video`. |
| `--video-bitrate <kbps>` | Requested video bitrate. |
| `--width <px>` / `--height <px>` | Requested video dimensions. Omit to preserve source when available. |
| `--frame-rate <fps>` | Requested frame rate. Omit to preserve source when available. |
| `--pixel-format <fmt>` | Adapter pixel format, such as `yuv420p`. |
| `--audio-only` | Strip artwork/video/subtitle/data side streams from audio outputs. |
| `--no-skip` | Force an encode even if codec/container already match. |
| `--json` | Emit NDJSON progress events on stdout. |

### `bbt probe`

Inspects media and prints source facts. Audio probing is native via Symphonia;
video probing uses `ffprobe` as a subprocess.

```bash
bbt probe "track.flac" --json
bbt probe "movie.mp4" --json
```

### `bbt check`

Reports which execution adapters and external tools are available.

```bash
bbt check --json
```

`plan`, `execute`, `verify`, and `resume` remain available as low-level internal
tooling around the execution graph/manifest machinery. They are not intended to
be BakBeat's device-policy interface.

## Structured Output

With `--json`, stdout is newline-delimited JSON, one event per line:

```json
{"type":"phase_start","phase":"probe","total":1}
{"type":"file_complete","phase":"probe","current":1,"total":1,"file":"track.flac","elapsed_ms":12}
{"type":"phase_complete","phase":"plan","jobs":1,"skipped":0}
{"type":"phase_start","phase":"resolve"}
{"type":"phase_start","phase":"encode","total":1}
{"type":"encode_start","current":1,"total":1,"file":"track.flac","output":"out/track.mp3"}
{"type":"file_complete","phase":"encode","current":1,"total":1,"file":"track.flac","output":"out/track.mp3","elapsed_ms":860}
{"type":"complete","success":1,"failed":0,"total_elapsed_ms":900,"manifest":"out/manifest.json"}
```

## Tool Boundary

`bbt` calls external encoders such as FFmpeg, ffprobe, and atracdenc as
subprocesses. BakBeat should call `bbt`, not those tools directly, for
transcoding/remuxing work that needs structured progress and error reporting.

The public repository carries the MPL-2.0 source and third-party notices so the
tool boundary stays clear for BakBeat's proprietary app.
