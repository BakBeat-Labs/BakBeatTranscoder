// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Folder-of-chapters → chaptered M4B merge.

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

fn ffmpeg_mp3(out: &Path, duration: &str, title: &str) {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:sample_rate=44100:duration={duration}"),
            "-ac",
            "2",
            "-c:a",
            "libmp3lame",
            "-b:a",
            "64k",
            "-metadata",
            &format!("title={title}"),
        ])
        .arg(out)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg mp3 failed");
}

#[test]
fn merges_numbered_mp3s_into_chaptered_m4b() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let chapters = dir.path().join("Killing Lincoln");
    std::fs::create_dir_all(&chapters).unwrap();
    ffmpeg_mp3(&chapters.join("CD 1a.mp3"), "1.2", "CD 1a");
    ffmpeg_mp3(&chapters.join("CD 2a.mp3"), "0.8", "CD 2a");
    ffmpeg_mp3(&chapters.join("CD 10a.mp3"), "0.5", "CD 10a");

    let out = dir.path().join("Killing Lincoln.m4b");
    let output = Command::new(bbt())
        .args([
            "audiobook-merge",
            "--json",
            "--title",
            "Killing Lincoln",
            "--artist",
            "Bill O'Reilly",
            "--bitrate",
            "64",
            "--output-file",
        ])
        .arg(&out)
        .arg(&chapters)
        .output()
        .expect("run bbt audiobook-merge");
    assert!(
        output.status.success(),
        "merge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let complete = stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .expect("json complete event");
    assert_eq!(complete["type"], "complete");
    assert_eq!(complete["chapters"], 3);
    assert_eq!(complete["output"], out.to_string_lossy().as_ref());
    assert!(out.is_file());

    let probe = Command::new(bbt())
        .args(["audiobook-probe", "--json"])
        .arg(&out)
        .output()
        .expect("run audiobook-probe");
    assert!(probe.status.success());
    let facts: Value = serde_json::from_slice(&probe.stdout).unwrap();
    assert_eq!(facts["has_chapters"], true);
    assert_eq!(facts["codec"], "aac");

    let ffprobe = Command::new("ffprobe")
        .args(["-v", "error", "-show_chapters", "-of", "json"])
        .arg(&out)
        .output()
        .expect("ffprobe chapters");
    assert!(ffprobe.status.success());
    let chapters_json: Value = serde_json::from_slice(&ffprobe.stdout).unwrap();
    let titles: Vec<String> = chapters_json["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["tags"]["title"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(titles, vec!["CD 1a", "CD 2a", "CD 10a"]);
}
