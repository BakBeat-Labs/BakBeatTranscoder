// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `bbt-wmv9` — Windows-only WMV9 (Main Profile) encoder helper for `bbt`.
//!
//! # Why this exists
//!
//! FFmpeg can only *encode* WMV7 (`wmv1`) and WMV8 (`wmv2`); it decodes but
//! cannot encode WMV9 (`wmv3`) / VC-1 — Microsoft never released the encoder
//! spec. Some target devices (e.g. the Sony NWZ-E370 Walkman) hard-require true
//! WMV9 Main/Simple Profile and reject a WMV8 stream. The only encoder that
//! produces real WMV9 is Microsoft's own, which ships built into every Windows
//! since Vista as the "Windows Media Video 9 Encoder" Media Foundation Transform
//! (`wmvencod.dll`). This helper drives that OS-provided encoder — it
//! redistributes nothing of Microsoft's.
//!
//! # Contract (called by `bbt`'s wmv9 adapter — see src/adapters/wmv9.rs)
//!
//! ```text
//! bbt-wmv9 --video  <raw_nv12_file>   frames of width*height*3/2 bytes, NV12
//!          --width  <px> --height <px>
//!          --fps    <f>                frames per second (e.g. 24, 29.97)
//!          --vbitrate <kbps>           WMV9 average bitrate
//!          --audio  <wav_file>         PCM s16le WAV (44.1 kHz stereo typical)
//!          --abitrate <kbps>           WMA average bitrate
//!          --output <out.wmv>          ASF/.wmv output
//! ```
//!
//! `bbt` uses FFmpeg (stage 1) to decode/scale/letterbox the source into the
//! raw NV12 stream and the intermediate WAV; this helper (stage 2) only does the
//! WMV9 + WMA encode and the ASF mux. This mirrors the ATRAC adapter's
//! two-stage pipeline.
//!
//! # Determinism
//!
//! Hardware transforms are disabled so encoding always runs through the software
//! WMV9 encoder. Note that Microsoft's encoder is not guaranteed bit-identical
//! across Windows versions/CPUs, so `bbt` scopes its byte-identical guarantee for
//! WMV outputs accordingly (see README).

// On non-Windows the encode path is a stub, so the MF-only helpers (Args fields,
// WAV parser) are unused there. They are all exercised on Windows.
#![cfg_attr(not(windows), allow(dead_code))]

use std::error::Error;
use std::path::PathBuf;

type BoxErr = Box<dyn Error>;

/// Parsed command-line arguments. Platform-independent so the crate compiles
/// everywhere; the actual encode is Windows-only.
struct Args {
    video: PathBuf,
    width: u32,
    height: u32,
    fps: f64,
    vbitrate_kbps: u32,
    audio: PathBuf,
    abitrate_kbps: u32,
    output: PathBuf,
}

fn parse_args() -> Result<Args, BoxErr> {
    let mut video = None;
    let mut width = None;
    let mut height = None;
    let mut fps = None;
    let mut vbitrate = None;
    let mut audio = None;
    let mut abitrate = None;
    let mut output = None;

    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || -> Result<String, BoxErr> {
            it.next()
                .ok_or_else(|| BoxErr::from(format!("missing value for {flag}")))
        };
        match flag.as_str() {
            "--video" => video = Some(PathBuf::from(value()?)),
            "--width" => width = Some(value()?.parse()?),
            "--height" => height = Some(value()?.parse()?),
            "--fps" => fps = Some(value()?.parse()?),
            "--vbitrate" => vbitrate = Some(value()?.parse()?),
            "--audio" => audio = Some(PathBuf::from(value()?)),
            "--abitrate" => abitrate = Some(value()?.parse()?),
            "--output" => output = Some(PathBuf::from(value()?)),
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    Ok(Args {
        video: video.ok_or("missing --video")?,
        width: width.ok_or("missing --width")?,
        height: height.ok_or("missing --height")?,
        fps: fps.ok_or("missing --fps")?,
        vbitrate_kbps: vbitrate.ok_or("missing --vbitrate")?,
        audio: audio.ok_or("missing --audio")?,
        abitrate_kbps: abitrate.ok_or("missing --abitrate")?,
        output: output.ok_or("missing --output")?,
    })
}

fn print_usage() {
    eprintln!(
        "bbt-wmv9 --video <nv12> --width <px> --height <px> --fps <f> \
         --vbitrate <kbps> --audio <wav> --abitrate <kbps> --output <out.wmv>"
    );
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("bbt-wmv9: {e}");
            print_usage();
            std::process::exit(2);
        }
    };

    if let Err(e) = run(&args) {
        eprintln!("bbt-wmv9: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn run(_args: &Args) -> Result<(), BoxErr> {
    Err("bbt-wmv9 is Windows-only — WMV9 encoding requires the OS Media \
         Foundation encoder, which exists only on Windows"
        .into())
}

// ─────────────────────────────────────────────────────────────────────────────
// Windows implementation
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(windows)]
fn run(args: &Args) -> Result<(), BoxErr> {
    win::encode(args)
}

#[cfg(windows)]
mod win {
    use super::{Args, BoxErr};
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

    // MF_VERSION is (SDK_VERSION << 16) | API_VERSION. Defined locally so we do
    // not depend on the constant being re-exported by the bindings crate.
    const MF_VERSION_LOCAL: u32 = (0x0002 << 16) | 0x0070;

    /// NV12 frame size in bytes: full-res Y plane + half-res interleaved UV.
    fn nv12_frame_len(width: u32, height: u32) -> usize {
        (width as usize * height as usize * 3) / 2
    }

    /// Split a floating fps into an MF frame-rate ratio (numerator/denominator).
    fn fps_ratio(fps: f64) -> (u32, u32) {
        // 1000 denominator handles both integer (24 → 24000/1000) and drop
        // frame rates (29.97 → 29970/1000) with no precision loss for our range.
        let num = (fps * 1000.0).round() as u32;
        (num.max(1), 1000)
    }

    pub fn encode(args: &Args) -> Result<(), BoxErr> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            MFStartup(MF_VERSION_LOCAL, MFSTARTUP_FULL)?;

            let result = encode_inner(args);

            // Best-effort teardown regardless of encode result.
            let _ = MFShutdown();
            CoUninitialize();
            result
        }
    }

    unsafe fn encode_inner(args: &Args) -> Result<(), BoxErr> {
        let (fps_num, fps_den) = fps_ratio(args.fps);
        let frame_100ns: i64 = 10_000_000i64 * fps_den as i64 / fps_num as i64;

        // ── Sink writer (ASF/.wmv) ──────────────────────────────────────────
        // Disable hardware transforms (there is no HW WMV9 encoder anyway, and
        // we want the deterministic software path) and disable throttling since
        // we feed samples synchronously from files.
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 2)?;
        let attrs = attrs.ok_or("MFCreateAttributes returned null")?;
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 0)?;
        attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;

        let out_wide: Vec<u16> = args
            .output
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let writer: IMFSinkWriter =
            MFCreateSinkWriterFromURL(PCWSTR(out_wide.as_ptr()), None, &attrs)?;

        // ── Video stream: output WMV9 (WMV3 = Main/Simple Profile) ──────────
        let v_out = MFCreateMediaType()?;
        v_out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        v_out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_WMV3)?;
        v_out.SetUINT32(&MF_MT_AVG_BITRATE, args.vbitrate_kbps * 1000)?;
        v_out.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0 as u32,
        )?;
        MFSetAttributeSize(&v_out, &MF_MT_FRAME_SIZE, args.width, args.height)?;
        MFSetAttributeRatio(&v_out, &MF_MT_FRAME_RATE, fps_num, fps_den)?;
        MFSetAttributeRatio(&v_out, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
        let v_stream = writer.AddStream(&v_out)?;

        // Video input: raw NV12 frames (what FFmpeg stage 1 produced).
        let v_in = MFCreateMediaType()?;
        v_in.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        v_in.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        v_in.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0 as u32,
        )?;
        MFSetAttributeSize(&v_in, &MF_MT_FRAME_SIZE, args.width, args.height)?;
        MFSetAttributeRatio(&v_in, &MF_MT_FRAME_RATE, fps_num, fps_den)?;
        MFSetAttributeRatio(&v_in, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
        writer.SetInputMediaType(v_stream, &v_in, None)?;

        // ── Audio stream: WMA v9, picked from the encoder's enumerated types ─
        let wav = super::wav::parse(&std::fs::read(&args.audio)?)?;
        let a_out = pick_wma_output_type(args.abitrate_kbps, wav.sample_rate, wav.channels)?;
        let a_stream = writer.AddStream(&a_out)?;

        // Audio input: PCM matching the intermediate WAV.
        let a_in = MFCreateMediaType()?;
        a_in.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        a_in.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        a_in.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, wav.sample_rate)?;
        a_in.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, wav.channels)?;
        a_in.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, wav.bits_per_sample)?;
        let block_align = wav.channels * (wav.bits_per_sample / 8);
        a_in.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align)?;
        a_in.SetUINT32(
            &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
            wav.sample_rate * block_align as u32,
        )?;
        writer.SetInputMediaType(a_stream, &a_in, None)?;

        // ── Write ───────────────────────────────────────────────────────────
        writer.BeginWriting()?;

        write_video(&writer, v_stream, args, frame_100ns)?;
        write_audio(&writer, a_stream, &wav)?;

        writer.Finalize()?;
        Ok(())
    }

    /// Enumerate the OS WMA v9 encoder's available output types and choose the
    /// one matching our sample rate/channels whose bitrate is closest to target.
    ///
    /// WMA output types carry codec-private data, so they must come from the
    /// encoder's own enumeration — they cannot be hand-built the way the video
    /// output type can.
    unsafe fn pick_wma_output_type(
        abitrate_kbps: u32,
        sample_rate: u32,
        channels: u32,
    ) -> Result<IMFMediaType, BoxErr> {
        let target_bytes_per_sec = (abitrate_kbps as i64 * 1000) / 8;

        let mut coll: Option<IMFCollection> = None;
        MFTranscodeGetAudioOutputAvailableTypes(
            &MFAudioFormat_WMAudioV9,
            MFT_ENUM_FLAG_ALL.0,
            None,
            &mut coll,
        )?;
        let coll = coll.ok_or("no WMA v9 output types available")?;
        let count = coll.GetElementCount()?;
        if count == 0 {
            return Err("WMA v9 encoder returned zero output types".into());
        }

        let mut best: Option<(IMFMediaType, i64)> = None;
        let mut best_any: Option<(IMFMediaType, i64)> = None;

        for i in 0..count {
            let unk = coll.GetElement(i)?;
            let mt: IMFMediaType = unk.cast()?;

            let sr = mt.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(0);
            let ch = mt.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(0);
            let bps = mt
                .GetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND)
                .unwrap_or(0) as i64;
            let dist = (bps - target_bytes_per_sec).abs();

            // Track the closest-by-bitrate over all types as a fallback.
            if best_any.as_ref().map(|(_, d)| dist < *d).unwrap_or(true) {
                best_any = Some((mt.clone(), dist));
            }
            // Prefer an exact sample-rate/channel match.
            if sr == sample_rate && ch == channels {
                if best.as_ref().map(|(_, d)| dist < *d).unwrap_or(true) {
                    best = Some((mt, dist));
                }
            }
        }

        best.or(best_any)
            .map(|(mt, _)| mt)
            .ok_or_else(|| "could not select a WMA v9 output type".into())
    }

    /// Read raw NV12 frames from the stage-1 file and write them to the writer.
    unsafe fn write_video(
        writer: &IMFSinkWriter,
        stream: u32,
        args: &Args,
        frame_100ns: i64,
    ) -> Result<(), BoxErr> {
        let frame_len = nv12_frame_len(args.width, args.height);
        let data = std::fs::read(&args.video)?;
        if frame_len == 0 || data.len() % frame_len != 0 {
            return Err(format!(
                "raw video size {} is not a whole number of {}x{} NV12 frames ({} bytes each)",
                data.len(),
                args.width,
                args.height,
                frame_len
            )
            .into());
        }
        let frames = data.len() / frame_len;
        for i in 0..frames {
            let start = i * frame_len;
            let sample = make_sample(&data[start..start + frame_len])?;
            sample.SetSampleTime(i as i64 * frame_100ns)?;
            sample.SetSampleDuration(frame_100ns)?;
            writer.WriteSample(stream, &sample)?;
        }
        Ok(())
    }

    /// Feed the WAV PCM to the writer in fixed-size chunks with computed
    /// timestamps.
    unsafe fn write_audio(
        writer: &IMFSinkWriter,
        stream: u32,
        wav: &super::wav::Wav,
    ) -> Result<(), BoxErr> {
        let block_align = (wav.channels * (wav.bits_per_sample / 8)) as usize;
        if block_align == 0 {
            return Err("WAV block alignment is zero".into());
        }
        let total_frames = wav.data.len() / block_align;
        // ~100 ms chunks — small enough to interleave cleanly with video.
        let chunk_frames = (wav.sample_rate / 10).max(1) as usize;

        let mut f = 0usize;
        while f < total_frames {
            let n = chunk_frames.min(total_frames - f);
            let byte_start = f * block_align;
            let byte_len = n * block_align;
            let sample = make_sample(&wav.data[byte_start..byte_start + byte_len])?;
            let time = f as i64 * 10_000_000 / wav.sample_rate as i64;
            let dur = n as i64 * 10_000_000 / wav.sample_rate as i64;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(dur)?;
            writer.WriteSample(stream, &sample)?;
            f += n;
        }
        Ok(())
    }

    /// Wrap a byte slice in an IMFSample backed by a single memory buffer.
    unsafe fn make_sample(bytes: &[u8]) -> Result<IMFSample, BoxErr> {
        let sample = MFCreateSample()?;
        let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;

        let mut ptr: *mut u8 = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        buffer.SetCurrentLength(bytes.len() as u32)?;
        buffer.Unlock()?;

        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Minimal WAV (RIFF/PCM) parser — platform-independent so it is unit-testable
// without Windows.
// ─────────────────────────────────────────────────────────────────────────────

mod wav {
    use super::BoxErr;

    pub struct Wav {
        pub sample_rate: u32,
        pub channels: u32,
        pub bits_per_sample: u32,
        pub data: Vec<u8>,
    }

    fn u16le(b: &[u8], o: usize) -> u16 {
        u16::from_le_bytes([b[o], b[o + 1]])
    }
    fn u32le(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
    }

    /// Parse a canonical PCM WAV (as produced by `ffmpeg -c:a pcm_s16le -f wav`).
    pub fn parse(bytes: &[u8]) -> Result<Wav, BoxErr> {
        if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return Err("not a RIFF/WAVE file".into());
        }

        let mut channels = 0u32;
        let mut sample_rate = 0u32;
        let mut bits_per_sample = 0u32;
        let mut data: Option<Vec<u8>> = None;

        let mut pos = 12usize;
        while pos + 8 <= bytes.len() {
            let id = &bytes[pos..pos + 4];
            let size = u32le(bytes, pos + 4) as usize;
            let body = pos + 8;
            if body + size > bytes.len() {
                break;
            }
            match id {
                b"fmt " => {
                    // WAVEFORMAT(EX): channels@2, sampleRate@4, bits@14.
                    channels = u16le(bytes, body + 2) as u32;
                    sample_rate = u32le(bytes, body + 4);
                    bits_per_sample = u16le(bytes, body + 14) as u32;
                }
                b"data" => {
                    data = Some(bytes[body..body + size].to_vec());
                }
                _ => {}
            }
            // Chunks are word-aligned: skip the pad byte on odd sizes.
            pos = body + size + (size & 1);
        }

        let data = data.ok_or("WAV has no data chunk")?;
        if channels == 0 || sample_rate == 0 || bits_per_sample == 0 {
            return Err("WAV fmt chunk incomplete".into());
        }
        Ok(Wav {
            sample_rate,
            channels,
            bits_per_sample,
            data,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Build a tiny 44.1 kHz stereo s16 WAV with `frames` silent frames.
        fn synth_wav(frames: u32) -> Vec<u8> {
            let channels = 2u16;
            let sample_rate = 44100u32;
            let bits = 16u16;
            let block_align = channels * (bits / 8);
            let data_len = frames * block_align as u32;

            let mut b = Vec::new();
            b.extend_from_slice(b"RIFF");
            b.extend_from_slice(&(36 + data_len).to_le_bytes());
            b.extend_from_slice(b"WAVE");
            b.extend_from_slice(b"fmt ");
            b.extend_from_slice(&16u32.to_le_bytes());
            b.extend_from_slice(&1u16.to_le_bytes()); // PCM
            b.extend_from_slice(&channels.to_le_bytes());
            b.extend_from_slice(&sample_rate.to_le_bytes());
            b.extend_from_slice(&(sample_rate * block_align as u32).to_le_bytes());
            b.extend_from_slice(&block_align.to_le_bytes());
            b.extend_from_slice(&bits.to_le_bytes());
            b.extend_from_slice(b"data");
            b.extend_from_slice(&data_len.to_le_bytes());
            b.extend(std::iter::repeat(0u8).take(data_len as usize));
            b
        }

        #[test]
        fn parses_canonical_pcm_wav() {
            let w = parse(&synth_wav(100)).expect("valid wav");
            assert_eq!(w.sample_rate, 44100);
            assert_eq!(w.channels, 2);
            assert_eq!(w.bits_per_sample, 16);
            assert_eq!(w.data.len(), 100 * 4);
        }

        #[test]
        fn rejects_non_riff() {
            assert!(parse(b"not a wav file at all").is_err());
        }

        #[test]
        fn rejects_missing_data_chunk() {
            let mut b = Vec::new();
            b.extend_from_slice(b"RIFF");
            b.extend_from_slice(&4u32.to_le_bytes());
            b.extend_from_slice(b"WAVE");
            assert!(parse(&b).is_err());
        }
    }
}
