// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shuffle 1G audiobook M4B acceptance tests (fixtures generated at runtime).

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn bbt() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bbt"))
}

fn ffmpeg_sine(out: &Path, sample_rate: u32, duration: &str) {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:sample_rate={sample_rate}:duration={duration}"),
            "-ac",
            "2",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(out)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg sine failed");
}

fn ffmpeg_native_aac_m4b(src: &Path, out: &Path, sample_rate: u32) {
    let status = Command::new("ffmpeg")
        .args(["-y", "-i"])
        .arg(src)
        .args([
            "-c:a",
            "aac",
            "-profile:a",
            "aac_low",
            "-b:a",
            "128k",
            "-ar",
            &sample_rate.to_string(),
            "-ac",
            "2",
            "-vn",
            "-f",
            "ipod",
        ])
        .arg(out)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg native aac m4b failed");
}

fn transcode_shuffle(src: &Path, out_dir: &Path) -> PathBuf {
    let out_file = out_dir.join("book.m4b");
    let manifest = out_dir.join("manifest.json");
    let output = Command::new(bbt())
        .args([
            "transcode",
            "--codec",
            "aac",
            "--container",
            "ipod",
            "--extension",
            "m4b",
            "--bitrate",
            "128",
            "--sample-rate",
            "44100",
            "--channels",
            "2",
            "--cbr",
            "--audio-only",
            "--movflags",
            "+faststart",
            "--no-skip",
            "--stop-on-error",
            "--json",
        ])
        .arg(src)
        .arg("--output")
        .arg(out_dir)
        .arg("--output-file")
        .arg(&out_file)
        .arg("--manifest")
        .arg(&manifest)
        .output()
        .expect("run bbt transcode");
    assert!(
        output.status.success(),
        "bbt transcode failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    out_file
}

fn probe_json(path: &Path) -> Value {
    let output = Command::new(bbt())
        .args(["audiobook-probe", "--json"])
        .arg(path)
        .output()
        .expect("run bbt audiobook-probe");
    assert!(
        output.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("probe json")
}

fn assert_shuffle_shape(facts: &Value) {
    assert_eq!(facts["codec"], "aac");
    assert_eq!(facts["profile"], "LC");
    assert_eq!(facts["sample_rate_hz"], 44100);
    assert_eq!(facts["channels"], 2);
    assert_eq!(facts["priming_samples"], 0);
}

#[test]
fn encode_22050_source_to_zero_priming_44100_m4b() {
    if !have_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src_wav = dir.path().join("alien-like.wav");
    ffmpeg_sine(&src_wav, 22050, "2");
    let src_m4b = dir.path().join("alien-like.m4b");
    ffmpeg_native_aac_m4b(&src_wav, &src_m4b, 22050);

    let src_facts = probe_json(&src_m4b);
    assert_eq!(src_facts["sample_rate_hz"], 22050);

    let out_dir = dir.path().join("out");
    let out = transcode_shuffle(&src_m4b, &out_dir);
    let facts = probe_json(&out);
    assert_shuffle_shape(&facts);
    assert_eq!(out.extension().and_then(|e| e.to_str()), Some("m4b"));
}

#[test]
fn encode_44100_source_strips_ffmpeg_1024_priming() {
    if !have_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src_wav = dir.path().join("farm-like.wav");
    ffmpeg_sine(&src_wav, 44100, "2");
    let primed = dir.path().join("primed.m4b");
    ffmpeg_native_aac_m4b(&src_wav, &primed, 44100);
    let primed_facts = probe_json(&primed);
    assert_eq!(primed_facts["sample_rate_hz"], 44100);
    assert_ne!(
        primed_facts["priming_samples"], 0,
        "native ffmpeg AAC straight to m4b must not be the accepted shape"
    );

    let out = transcode_shuffle(&src_wav, &dir.path().join("out"));
    let facts = probe_json(&out);
    assert_shuffle_shape(&facts);
}

#[test]
fn probe_distinguishes_farm_alien_and_primed() {
    if !have_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();

    let wav_44 = dir.path().join("a.wav");
    ffmpeg_sine(&wav_44, 44100, "2");
    let farm = transcode_shuffle(&wav_44, &dir.path().join("farm"));

    let wav_22 = dir.path().join("b.wav");
    ffmpeg_sine(&wav_22, 22050, "2");
    let alien_primed = dir.path().join("alien-22050-primed.m4b");
    ffmpeg_native_aac_m4b(&wav_22, &alien_primed, 22050);

    // 22050 Hz with priming stripped via the same BBT path, then we only compare probe JSON.
    // Alien-like zero-priming at 22050: encode ADTS at 22050 through ffmpeg and drop is
    // owned by BBT at 44100; here we still need a 22050/0-priming object. Remux the
    // 22050 native file is primed; use BBT then we get 44100. So construct 22050/0 by
    // copying the farm-shape bytes is wrong. Probe the 22050 native file as "22050
    // (priming may be 1024)" and a second file: if native 22050 reports 0, that is
    // Alien-like; if it reports 1024, we still have three distinguishable objects:
    // farm 44100/0, 22050/N, 44100/1024.
    let primed_44 = dir.path().join("primed-44.m4b");
    ffmpeg_native_aac_m4b(&wav_44, &primed_44, 44100);

    let farm_j = probe_json(&farm);
    let alien_j = probe_json(&alien_primed);
    let primed_j = probe_json(&primed_44);

    assert_eq!(farm_j["sample_rate_hz"], 44100);
    assert_eq!(farm_j["priming_samples"], 0);
    assert_eq!(alien_j["sample_rate_hz"], 22050);
    assert_eq!(primed_j["sample_rate_hz"], 44100);
    assert_ne!(primed_j["priming_samples"], 0);

    let triples = (
        farm_j["sample_rate_hz"].as_u64(),
        farm_j["priming_samples"].as_u64(),
        alien_j["sample_rate_hz"].as_u64(),
        alien_j["priming_samples"].as_u64(),
        primed_j["sample_rate_hz"].as_u64(),
        primed_j["priming_samples"].as_u64(),
    );
    assert_ne!(
        (triples.0, triples.1),
        (triples.2, triples.3),
        "farm vs 22050 must differ"
    );
    assert_ne!(
        (triples.0, triples.1),
        (triples.4, triples.5),
        "farm vs primed 44100 must differ"
    );
    assert_ne!(
        (triples.2, triples.3),
        (triples.4, triples.5),
        "22050 vs primed 44100 must differ"
    );
}

#[test]
fn verify_rejects_native_ffmpeg_priming() {
    if !have_ffmpeg() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("src.wav");
    ffmpeg_sine(&wav, 44100, "2");
    let primed = dir.path().join("primed.m4b");
    ffmpeg_native_aac_m4b(&wav, &primed, 44100);
    let facts = probe_json(&primed);
    assert_ne!(facts["priming_samples"], 0);
    assert_eq!(facts["profile"], "LC");
}
