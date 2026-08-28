// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Merge a folder of chapter files into one AAC `.m4b` with named chapters.
//!
//! This is the library-import path (catalog + chapter markers), not the
//! Shuffle 1G zero-priming encode. BakBeat calls this, then imports the
//! resulting `.m4b`. Device-specific remux stays on `bbt transcode`.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};

use crate::binaries;
use crate::cli::AudiobookMergeArgs;
use crate::probe;
use crate::progress::{Emitter, Event, Phase};

const CHAPTER_EXTS: &[&str] = &["mp3", "m4a"];

pub fn run(args: AudiobookMergeArgs, json: bool) -> Result<()> {
    let ffmpeg = binaries::find_ffmpeg()
        .ok_or_else(|| anyhow!("ffmpeg not found; cannot merge audiobook chapters"))?;
    let ffprobe = binaries::find_ffprobe();

    let input = args
        .input
        .canonicalize()
        .with_context(|| format!("input not found: {}", args.input.display()))?;
    if !input.is_dir() {
        return Err(anyhow!(
            "audiobook-merge expects a folder of chapter files, got {}",
            input.display()
        ));
    }

    let files = collect_chapter_files(&input)?;
    if files.is_empty() {
        return Err(anyhow!(
            "no .mp3 or .m4a chapter files found in {}",
            input.display()
        ));
    }

    let title = args
        .title
        .clone()
        .unwrap_or_else(|| folder_title(&input, &files));
    let album = args.album.clone().unwrap_or_else(|| title.clone());
    let output = resolve_output_path(&args, &input, &title)?;
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut emitter = Emitter::new(json);
    let total = files.len();
    let batch_start = Instant::now();

    emitter.emit(Event::PhaseStart {
        phase: Phase::Probe,
        total: Some(total),
        carrying_forward: None,
    });

    let mut chapters = Vec::with_capacity(total);
    let mut cursor_ms: u64 = 0;
    for (idx, path) in files.iter().enumerate() {
        let t0 = Instant::now();
        let duration_ms = probe_duration_ms(path, ffprobe.as_deref())?;
        let start_ms = cursor_ms;
        let end_ms = cursor_ms + duration_ms;
        chapters.push(Chapter {
            path: path.clone(),
            title: chapter_title(path),
            start_ms,
            end_ms,
        });
        cursor_ms = end_ms;
        emitter.emit(Event::FileComplete {
            phase: Phase::Probe,
            current: idx + 1,
            total,
            file: path.to_string_lossy().into_owned(),
            output: None,
            elapsed_ms: t0.elapsed().as_millis() as u64,
        });
    }

    emitter.emit(Event::PhaseComplete {
        phase: Phase::Probe,
        total: Some(total),
        jobs: None,
        skipped: None,
        success: Some(total),
        failed: Some(0),
    });

    emitter.emit(Event::PhaseStart {
        phase: Phase::Encode,
        total: Some(1),
        carrying_forward: None,
    });
    emitter.emit(Event::EncodeStart {
        current: 1,
        total: 1,
        file: input.to_string_lossy().into_owned(),
        output: output.to_string_lossy().into_owned(),
    });

    let encode_start = Instant::now();
    if let Err(e) = encode_m4b(
        &ffmpeg,
        &chapters,
        &output,
        &title,
        args.artist.as_deref(),
        &album,
        args.bitrate,
        args.sample_rate,
        args.channels,
    ) {
        emitter.emit(Event::FileFailed {
            phase: Phase::Encode,
            current: 1,
            total: 1,
            file: input.to_string_lossy().into_owned(),
            output: Some(output.to_string_lossy().into_owned()),
            elapsed_ms: encode_start.elapsed().as_millis() as u64,
            error: e.to_string(),
        });
        emitter.emit(Event::OperationFailed {
            phase: Some(Phase::Encode),
            error: e.to_string(),
        });
        return Err(e);
    }

    if let Err(e) = verify_m4b(&output, chapters.len(), cursor_ms, ffprobe.as_deref()) {
        emitter.emit(Event::FileFailed {
            phase: Phase::Encode,
            current: 1,
            total: 1,
            file: input.to_string_lossy().into_owned(),
            output: Some(output.to_string_lossy().into_owned()),
            elapsed_ms: encode_start.elapsed().as_millis() as u64,
            error: e.to_string(),
        });
        emitter.emit(Event::OperationFailed {
            phase: Some(Phase::Encode),
            error: e.to_string(),
        });
        return Err(e);
    }

    emitter.emit(Event::FileComplete {
        phase: Phase::Encode,
        current: 1,
        total: 1,
        file: input.to_string_lossy().into_owned(),
        output: Some(output.to_string_lossy().into_owned()),
        elapsed_ms: encode_start.elapsed().as_millis() as u64,
    });
    emitter.emit(Event::PhaseComplete {
        phase: Phase::Encode,
        total: Some(1),
        jobs: Some(1),
        skipped: Some(0),
        success: Some(1),
        failed: Some(0),
    });
    emitter.emit(Event::Complete {
        success: 1,
        failed: 0,
        total_elapsed_ms: batch_start.elapsed().as_millis() as u64,
        manifest: String::new(),
        carried_forward: None,
        re_encoded: None,
        output: Some(output.to_string_lossy().into_owned()),
        chapters: Some(chapters.len()),
    });

    Ok(())
}

#[derive(Debug, Clone)]
struct Chapter {
    path: PathBuf,
    title: String,
    start_ms: u64,
    end_ms: u64,
}

fn collect_chapter_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_chapters(root, &mut files)?;
    files.sort_by(|a, b| {
        let a_rel = a.strip_prefix(root).unwrap_or(a);
        let b_rel = b.strip_prefix(root).unwrap_or(b);
        natural_cmp(&a_rel.to_string_lossy(), &b_rel.to_string_lossy())
    });
    Ok(files)
}

fn walk_chapters(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            walk_chapters(&path, out)?;
        } else {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if CHAPTER_EXTS.contains(&ext.as_str()) {
                out.push(path);
            }
        }
    }
    Ok(())
}

fn folder_title(input: &Path, files: &[PathBuf]) -> String {
    if let Some(name) = input.file_name().and_then(|n| n.to_str()) {
        if !name.is_empty() {
            return name.to_string();
        }
    }
    files
        .first()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("Audiobook")
        .to_string()
}

fn chapter_title(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn resolve_output_path(args: &AudiobookMergeArgs, input: &Path, title: &str) -> Result<PathBuf> {
    if let Some(path) = &args.output_file {
        return Ok(path.clone());
    }
    let mut name = sanitize_filename(title);
    if name.is_empty() {
        name = sanitize_filename(
            input
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("audiobook"),
        );
    }
    Ok(args.output.join(format!("{name}.m4b")))
}

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    cleaned.trim().to_string()
}

fn probe_duration_ms(path: &Path, ffprobe: Option<&Path>) -> Result<u64> {
    match probe::probe_audio_file(path) {
        Ok(info) => {
            if let Some(secs) = info.duration_secs.filter(|d| *d > 0.0) {
                return Ok((secs * 1000.0).round() as u64);
            }
        }
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "symphonia duration missing");
        }
    }
    let ffprobe = ffprobe.ok_or_else(|| {
        anyhow!(
            "could not read duration for {} and ffprobe is unavailable",
            path.display()
        )
    })?;
    ffprobe_duration_ms(ffprobe, path)
}

fn ffprobe_duration_ms(ffprobe: &Path, path: &Path) -> Result<u64> {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .with_context(|| format!("running ffprobe on {}", path.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "ffprobe failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let secs: f64 = text
        .parse()
        .with_context(|| format!("ffprobe duration for {}: {text}", path.display()))?;
    if secs <= 0.0 {
        return Err(anyhow!("zero duration: {}", path.display()));
    }
    Ok((secs * 1000.0).round() as u64)
}

#[allow(clippy::too_many_arguments)]
fn encode_m4b(
    ffmpeg: &Path,
    chapters: &[Chapter],
    output: &Path,
    title: &str,
    artist: Option<&str>,
    album: &str,
    bitrate_kbps: u32,
    sample_rate: Option<u32>,
    channels: Option<u8>,
) -> Result<()> {
    let tmp = tempfile::tempdir().context("creating merge temp dir")?;
    let concat_path = tmp.path().join("concat.txt");
    let meta_path = tmp.path().join("metadata.txt");

    let mut concat = String::new();
    for ch in chapters {
        concat.push_str(&format!("file {}\n", ffmpeg_concat_path(&ch.path)));
    }
    std::fs::write(&concat_path, concat)?;
    std::fs::write(&meta_path, build_ffmetadata(title, artist, album, chapters))?;

    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&concat_path)
        .arg("-i")
        .arg(&meta_path)
        .args([
            "-map",
            "0:a",
            "-map_metadata",
            "1",
            "-c:a",
            "aac",
            "-profile:a",
            "aac_low",
            "-b:a",
            &format!("{bitrate_kbps}k"),
            "-vn",
            "-sn",
            "-dn",
            "-f",
            "ipod",
            "-movflags",
            "+faststart",
            "-progress",
            "pipe:1",
            "-nostats",
            "-loglevel",
            "error",
        ]);
    if let Some(rate) = sample_rate {
        cmd.args(["-ar", &rate.to_string()]);
    }
    if let Some(ch) = channels {
        cmd.args(["-ac", &ch.to_string()]);
    }
    cmd.arg(output);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawning ffmpeg for audiobook merge")?;
    let stderr_thread = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut buf);
            buf
        })
    });
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            tracing::trace!(ffmpeg_progress = %line);
        }
    }
    let status = child.wait()?;
    let stderr = stderr_thread
        .map(|t| t.join().unwrap_or_default())
        .unwrap_or_default();
    if !status.success() {
        return Err(anyhow!(
            "ffmpeg audiobook merge failed with status {status}: {stderr}"
        ));
    }
    Ok(())
}

/// ffmpeg concat demuxer path: single-quoted, with `'` escaped as `'\''`.
fn ffmpeg_concat_path(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let escaped = raw.replace('\'', r"'\''");
    format!("'{escaped}'")
}

fn build_ffmetadata(
    title: &str,
    artist: Option<&str>,
    album: &str,
    chapters: &[Chapter],
) -> String {
    let mut lines = vec![
        ";FFMETADATA1".to_string(),
        format!("title={}", escape_ffmetadata(title)),
        format!("album={}", escape_ffmetadata(album)),
    ];
    if let Some(artist) = artist {
        lines.push(format!("artist={}", escape_ffmetadata(artist)));
        lines.push(format!("album_artist={}", escape_ffmetadata(artist)));
    }
    lines.push(String::new());
    for ch in chapters {
        lines.push("[CHAPTER]".into());
        lines.push("TIMEBASE=1/1000".into());
        lines.push(format!("START={}", ch.start_ms));
        lines.push(format!("END={}", ch.end_ms));
        lines.push(format!("title={}", escape_ffmetadata(&ch.title)));
        lines.push(String::new());
    }
    let mut text = lines.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn escape_ffmetadata(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '=' | ';' | '#' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\\n"),
            _ => out.push(c),
        }
    }
    out
}

fn verify_m4b(
    output: &Path,
    expected_chapters: usize,
    expected_ms: u64,
    ffprobe: Option<&Path>,
) -> Result<()> {
    let size = std::fs::metadata(output)
        .with_context(|| format!("merged output missing: {}", output.display()))?
        .len();
    if size == 0 {
        return Err(anyhow!("merged output is empty: {}", output.display()));
    }

    let facts = probe::probe_audiobook_facts(output)?;
    if facts.has_chapters != Some(true) {
        return Err(anyhow!(
            "merged output has no chapter markers: {}",
            output.display()
        ));
    }

    let Some(ffprobe) = ffprobe else {
        return Ok(());
    };
    let output_probe = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-show_chapters",
            "-of",
            "json",
        ])
        .arg(output)
        .output()
        .context("ffprobe verify")?;
    if !output_probe.status.success() {
        return Err(anyhow!(
            "ffprobe verify failed: {}",
            String::from_utf8_lossy(&output_probe.stderr)
        ));
    }
    let data: serde_json::Value = serde_json::from_slice(&output_probe.stdout)?;
    let actual_ms = data["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|s| (s * 1000.0).round() as i64)
        .ok_or_else(|| anyhow!("ffprobe verify: missing duration"))?;
    let delta = (actual_ms - expected_ms as i64).unsigned_abs();
    if delta > 5_000 {
        return Err(anyhow!(
            "merged duration mismatch: got {actual_ms} ms, expected {expected_ms} ms"
        ));
    }
    let actual_chapters = data
        .get("chapters")
        .and_then(|c| c.as_array())
        .map(|c| c.len())
        .unwrap_or(0);
    if actual_chapters != expected_chapters {
        return Err(anyhow!(
            "merged chapter count mismatch: got {actual_chapters}, expected {expected_chapters}"
        ));
    }
    let _ = writeln!(
        std::io::stderr(),
        "  verified {} chapters, {:.1} MB",
        actual_chapters,
        size as f64 / 1_048_576.0
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum NatPart<'a> {
    Num(u64),
    Text(&'a str),
}

fn natural_parts(s: &str) -> Vec<NatPart<'_>> {
    let mut parts = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let digit_end = rest
            .char_indices()
            .find(|(_, c)| !c.is_ascii_digit())
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        if digit_end > 0 {
            let (num, next) = rest.split_at(digit_end);
            parts.push(NatPart::Num(num.parse().unwrap_or(u64::MAX)));
            rest = next;
            continue;
        }
        let text_end = rest
            .char_indices()
            .find(|(_, c)| c.is_ascii_digit())
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let (text, next) = rest.split_at(text_end);
        parts.push(NatPart::Text(text));
        rest = next;
    }
    parts
}

fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let a_lower = a.to_ascii_lowercase();
    let b_lower = b.to_ascii_lowercase();
    let a_parts = natural_parts(&a_lower);
    let b_parts = natural_parts(&b_lower);
    for (left, right) in a_parts.iter().zip(b_parts.iter()) {
        let ord = match (left, right) {
            (NatPart::Num(x), NatPart::Num(y)) => x.cmp(y),
            (NatPart::Text(x), NatPart::Text(y)) => x.cmp(y),
            (NatPart::Num(_), NatPart::Text(_)) => Ordering::Less,
            (NatPart::Text(_), NatPart::Num(_)) => Ordering::Greater,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a_parts.len().cmp(&b_parts.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_sort_orders_cd_tracks() {
        let mut names = vec!["CD 10a.mp3", "CD 2a.mp3", "CD 1b.mp3", "CD 1a.mp3"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            vec!["CD 1a.mp3", "CD 1b.mp3", "CD 2a.mp3", "CD 10a.mp3"]
        );
    }

    #[test]
    fn ffmetadata_escapes_specials() {
        assert_eq!(escape_ffmetadata(r"A=B;C#D\E"), r"A\=B\;C\#D\\E");
    }

    #[test]
    fn concat_escapes_apostrophe() {
        let path = PathBuf::from("/Volumes/Data/Bill O'Reilly/CD 1a.mp3");
        assert_eq!(
            ffmpeg_concat_path(&path),
            "'/Volumes/Data/Bill O'\\''Reilly/CD 1a.mp3'"
        );
    }

    #[test]
    fn metadata_includes_chapter_titles() {
        let chapters = vec![Chapter {
            path: PathBuf::from("CD 1a.mp3"),
            title: "CD 1a".into(),
            start_ms: 0,
            end_ms: 1000,
        }];
        let text = build_ffmetadata(
            "Killing Lincoln",
            Some("Bill O'Reilly"),
            "Killing Lincoln",
            &chapters,
        );
        assert!(text.contains("title=Killing Lincoln"));
        assert!(text.contains("artist=Bill O'Reilly"));
        assert!(text.contains("START=0"));
        assert!(text.contains("title=CD 1a"));
    }
}
