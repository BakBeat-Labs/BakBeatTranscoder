// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

fn bbt(args: &[&str], input: &Path, expected_exit: i32) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_bbt"))
        .args(args)
        .arg(input)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(expected_exit),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn probe(path: &Path) -> Value {
    serde_json::from_slice(&bbt(&["--json", "probe"], path, 0).stdout).unwrap()
}

#[test]
fn high_rate_alac_preserves_rate_and_requires_explicit_mp3_resampling() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let alac = dir.path().join("source.m4a");
    let generated = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=sample_rate=96000:duration=0.25",
            "-ac",
            "2",
            "-sample_fmt",
            "s32p",
            "-c:a",
            "alac",
        ])
        .arg(&alac)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{:?}", generated);
    let facts = probe(&alac);
    assert_eq!(facts["sample_rate_hz"], 96000);
    assert_eq!(facts["bits_per_sample"], 24);
    assert_eq!(facts["channels"], 2);
    assert_eq!(facts["duration_secs"], 0.25);

    let dest = dir.path().to_str().unwrap();
    bbt(
        &["transcode", "--codec", "flac", "--output", dest],
        &alac,
        0,
    );
    let flac = dir.path().join("source.flac");
    assert_eq!(probe(&flac)["sample_rate_hz"], 96000);

    let rerated = dir.path().join("rerated.flac");
    bbt(
        &[
            "transcode",
            "--codec",
            "flac",
            "--sample-rate",
            "44100",
            "--output",
            dest,
            "--output-file",
            rerated.to_str().unwrap(),
        ],
        &flac,
        0,
    );
    assert_eq!(probe(&rerated)["sample_rate_hz"], 44100);

    let skipped = bbt(&["--json", "transcode", "--codec", "flac"], &flac, 0);
    let events = String::from_utf8(skipped.stdout).unwrap();
    let complete: Value = serde_json::from_str(events.lines().last().unwrap()).unwrap();
    assert_eq!(complete["type"], "complete");
    assert_eq!(complete["skipped"], 1);

    let rejected = bbt(&["transcode", "--codec", "mp3", "--output", dest], &flac, 1);
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("MP3 cannot encode at 96000 Hz"));
    let mp3 = dir.path().join("source.mp3");
    assert!(!mp3.exists());

    // Force an encoder failure after it has had a chance to open its output.
    std::fs::write(&mp3, b"previous output").unwrap();
    bbt(
        &[
            "transcode",
            "--codec",
            "mp3",
            "--sample-rate",
            "48000",
            "--channels",
            "3",
            "--output",
            dest,
        ],
        &flac,
        2,
    );
    assert_eq!(std::fs::read(&mp3).unwrap(), b"previous output");
    assert!(dir.path().read_dir().unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".bbt-encode-")
    }));
}
