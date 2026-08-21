# BBT handoff: Shuffle 1G unprotected M4B

Copy this file into the BakBeat Transcoder repo. It is the artifact spec BakBeatMac will send. BBT owns FFmpeg, muxing, priming, and verification. BakBeatMac owns when to copy vs create, which device profile applies, and the iTunesSD write.

Do not add an MP3 audiobook path. Shuffle music already plays MP3; audiobooks stay `.m4b`.

Hardware: physical iPod shuffle 1st generation, 2026-08-21. Writer already on BakBeatMac (`Shuffle1GAudiobookOperation`, iTunesSD AAC type, bookmark=1, shuffle=0). That writer is not the problem.

## What already works without BBT

| File | Shape | Shuffle playback |
| --- | --- | --- |
| Animal Farm `.m4b` | AAC-LC stereo **44100 Hz**, ~126 kbps, **0 priming**, max packet 578, chapters + cover, Lavf60 remux | Plays |
| Original Alien `.m4b` | AAC-LC stereo **22050 Hz**, ~63 kbps, **0 priming**, max packet 591, chapters + cover | Dies after bumper, restarts same file |
| ffmpeg native AAC 44.1 / 128k CBR `.m4b` | **1024 priming** (elst media_time 1024) | Dies / restarts |
| ffmpeg `aac_at` 44.1 / 64k CBR `.m4b` | **2112 priming** | Dies / restarts |
| Same Alien bumper as **music MP3** 44.1 / 128k | MP3 | Plays as music, not an audiobook format |
| `ShuffleTest4 Alien Farm-shape.m4b` | AAC-LC stereo **44100 Hz**, ~128 kbps CBR, **0 priming**, ipod mux, no chapters/cover, max packet 1036 | **Plays** |

Packet size is not the failure. Priming and 22050 Hz are.

## Exact output BBT must produce

When BakBeat asks for a Shuffle audiobook derivative, the file on disk must match this:

| Field | Required value | Why |
| --- | --- | --- |
| Extension | `m4b` | Profile `[audiobooks] allowed_extensions = ["m4b"]` |
| Container muxer | FFmpeg `-f ipod` | Same muxer as the playing Farm-shape bumper and iPod video path |
| Audio codec | AAC-LC (`mp4a.40.2`), not HE-AAC / SBR / PS | Animal Farm and Farm-shape |
| Sample rate | **44100 Hz** | 22050 Hz died. 48000 / 32000 untested — do not emit them |
| Channels | **2** | Proven. Mono was only tried with primed `aac_at` and died; do not ship mono |
| Bitrate | **128 kbps CBR** | Matches Animal Farm (~126 kbps) and the playing bumper |
| Encoder priming | **0** | `afinfo` style: `valid frames + 0 priming`. MP4 edit list `media_time == 0`. Native ffmpeg AAC leaving 1024 delay **fails** on this device. `aac_at` / AudioToolbox 2112 **fails**. |
| Cover / chapters / text streams | Strip for this target | Shuffle has no screen and no chapter MHOD 17. Animal Farm played *with* chapters+cover, but the encode we will ship is the Farm-shape bumper (audio-only). Safer and matches `--audio-only`. |
| DRM | Refuse `.m4p` / Audible AAX | Never write FairPlay |

Do not require Animal Farm’s 578-byte max packet. The playing bumper had max packet **1036**.

Do not use `-c:a aac_at`.

## Proven encode (one-off that played)

This is the pipeline that produced `ShuffleTest4 Alien Farm-shape.m4b`. BBT should own an equivalent, not a host-app ffmpeg script.

1. Decode source audio to PCM, resample **44100 Hz stereo**.
2. Encode **native** FFmpeg `aac`, profile `aac_low`, `-b:a 128k` CBR, 44100 / 2 ch. The working encode also set `-aac_pns 0 -aac_is 0 -aac_tns 0 -aac_ms 0`. Those flags are **not** hardware-proven as necessary (packet size was not the discriminator). Keep them if they are free; do not treat them as the reason it played.
3. Write **ADTS**.
4. Drop the **first AAC frame** (native encoder delay = 1024 samples).
5. Remux copy: `-f ipod -movflags +faststart` into `.m4b`.
6. Confirm priming 0 and sample rate 44100. Fail the job if not.

Full-length books (hours) must use this path, not a 3-minute-only trick.

## What BBT already has (do not reinvent)

Repo: BakBeat Transcoder. Existing caller shape from BakBeatMac audio derivation:

```text
bbt transcode <in> --codec aac --container <ext> --extension <ext>
  [--bitrate N] [--sample-rate N] [--channels N] [--cbr] [--audio-only]
  --source-root … --output … --manifest … --stop-on-error
```

Relevant current behavior:

- `--audio-only` already strips artwork / side streams.
- `--container ipod` already maps to `-f ipod` for **video** only (`src/adapters/ffmpeg.rs`). Audio encodes currently do **not** pass `-f` / `-movflags`.
- Native `-c:a aac` is the default AAC encoder. That is the encoder that writes **1024 priming** if you mux MP4/M4B directly.
- Gapless `atrim` today is for **source** iTunSMPB trailing padding on M4A→other. It does not strip **output** encoder delay. Animal Farm / original Alien had **no** iTunSMPB; priming lived in the `elst` media_time.
- `audiobook-probe` is a thin ffprobe JSON dump. It reports `sample_rate` but **not** priming.
- Planner skip is codec+container. An already-AAC `.m4b` at 22050 Hz would skip a 44100 target unless the caller passes `--no-skip`.

## Add in BBT

### 1. Honor ipod mux + movflags on audio `.m4b`

If spec is `audio_codec=aac`, `extension=m4b` (and/or `container=ipod` or `m4b`):

- `-f ipod`
- `-movflags +faststart` when requested
- `--audio-only` → `-vn -sn -dn -map_chapters -1`

BakBeat will send `--container ipod --extension m4b` so this matches the video iPod muxer signal.

### 2. Zero-priming AAC-LC output (the actual work)

Direct `ffmpeg -c:a aac -f ipod out.m4b` is **not** acceptable for this target. It leaves elst media_time 1024 and Shuffle dies.

Implement an AAC-LC ipod/m4b encode that finishes with **0 priming**. The ADTS → drop first frame → ipod remux above is the proven method. Other methods are fine only if verify proves priming 0.

Refuse AudioToolbox `aac_at` for this target.

### 3. Verify the artifact, do not trust ffmpeg exit 0

After encode, fail the job unless:

- audio codec is AAC-LC
- sample_rate_hz == requested (44100)
- channels == requested (2)
- priming_samples == 0 (edit list and/or equivalent of afinfo priming)
- extension/container is m4b / ipod-compatible MP4

Record priming in the manifest if you add the field. Do not pass a 1024-priming file as success.

### 4. Probe facts BakBeat needs for copy vs create

BakBeat decides copyExisting vs create. BBT must expose enough that BakBeat does not guess.

Minimum additions (either `bbt probe --json` or `bbt audiobook-probe --json`, structured — not a new ffprobe dump):

| Field | Meaning |
| --- | --- |
| `codec` | `aac` |
| `profile` | `LC` vs HE/SBR |
| `sample_rate_hz` | integer |
| `channels` | integer |
| `bitrate_kbps` | approximate OK |
| `priming_samples` | 0 or N from elst / decoder delay. **Required.** Animal Farm and original Alien are both 0; they still differ on sample rate. |
| `has_chapters` / `has_artwork` | optional; Shuffle encode will strip |

If probe cannot see priming, say so explicitly (`null`) so BakBeat does not treat the file as Shuffle-ready.

Skip/no-op inside `transcode` must compare **codec + sample rate + channels + priming 0 + LC**, not codec+extension only. BakBeat will still pass `--no-skip` when it has already planned `create`.

### 5. CLI BakBeatMac will call (once wired)

```bash
bbt transcode "$SOURCE_M4B" \
  --codec aac \
  --container ipod \
  --extension m4b \
  --bitrate 128 \
  --sample-rate 44100 \
  --channels 2 \
  --cbr \
  --audio-only \
  --movflags +faststart \
  --no-skip \
  --source-root "$SOURCE_DIR" \
  --output "$OUT_DIR" \
  --manifest "$MANIFEST" \
  --stop-on-error \
  --json
```

If you need a named profile flag instead of inferring from `ipod`+`m4b`, `--aac-priming 0` or equivalent is acceptable. Do not make BakBeat pass raw ffmpeg graphs.

## Acceptance (BBT repo tests)

Use fixtures, not the user’s library paths in CI. Replay the hardware matrix:

1. **Encode** a 22050 Hz AAC source (Alien-like) with the CLI above. Output must be 44100 / LC / 2 ch / 0 priming / `.m4b`. `ffmpeg -c:a aac` straight to m4b must **not** be what this test accepts.
2. **Encode** a 44100 Hz source that ffmpeg would normally tag with 1024 priming. Output priming must be 0.
3. **Reject** success if you deliberately mux with priming left in.
4. **Probe** Animal-Farm-like (44100, priming 0) vs 22050 priming 0 vs primed 44100 — three distinguishable JSON objects.
5. Do not add tests that encode Shuffle audiobooks as MP3.

Manual hardware re-check stays on BakBeatMac Add to Device. BBT done means the bytes match the playing Farm-shape file, not a new Shuffle stamp.

## Out of scope for this BBT slice

- iTunesSD / iTunesDB / bookmark flags (BakBeatMac Shuffle writer)
- ArtworkDB, chapter MHOD 17, Shuffle UI
- MP3 audiobook import or write
- Device profile policy, copy-vs-create, library catalog
- Host-app ffmpeg/ffprobe
- WMV9 / video (existing BBT video rules unchanged)
- Claiming 48 kHz, mono, or HE-AAC are Shuffle-safe

## BakBeatMac follow-up (not this BBT change)

After BBT ships the above:

- Plan Shuffle audiobook add: copyExisting only when probe says AAC-LC 44100 stereo 0 priming `.m4b`; otherwise create via this spec; else block.
- Materialize through existing `bbt transcode` (same pattern as `LibraryDeviceAudioDerivationMaterializationOperation`), not a new pebble ffmpeg.
- Keep `[audiobooks] allowed_extensions = ["m4b"]`.
