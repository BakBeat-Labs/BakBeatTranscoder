// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Centralized binary discovery for external encoder tools.
//!
//! # Legal structure
//!
//! BakBeatTranscoder (MPL-2.0) acts as the legal buffer between BakBeat
//! (proprietary, commercial) and LGPL-licensed tools (FFmpeg, atracdenc).
//!
//!   BakBeat (proprietary)
//!       ↓ subprocess call only — clean boundary
//!   bbt (MPL-2.0) ← LGPL compliance sits here, not in BakBeat
//!       ↓ subprocess calls
//!   ffmpeg / ffprobe / atracdenc (LGPL)
//!
//! BakBeat ships `bbt` and nothing else. All LGPL tooling is bbt's
//! responsibility. BakBeat has no LGPL exposure because it never directly
//! touches these tools.
//!
//! # Resolution order
//!
//! For each binary, the search order is:
//!   1. Explicit env var (e.g. BBT_FFMPEG_PATH) — lets the caller specify
//!      an exact path, useful when bbt is installed in a non-standard location.
//!   2. Same directory as the bbt binary — platform releases of bbt bundle
//!      ffmpeg, ffprobe, and atracdenc alongside the bbt binary so that a
//!      single directory installation just works.
//!   3. System PATH — fallback for developers and power users.

use std::path::PathBuf;

/// Locate the `ffmpeg` binary.
pub fn find_ffmpeg() -> Option<PathBuf> {
    find_tool("ffmpeg", "BBT_FFMPEG_PATH")
}

/// Locate the `ffprobe` binary.
pub fn find_ffprobe() -> Option<PathBuf> {
    find_tool("ffprobe", "BBT_FFPROBE_PATH")
}

/// Locate the ATRAC encoder binary.
/// Prefers `atracdenc` (open source, LGPL) over `atracenc` (Sony binary).
pub fn find_atracdenc() -> Option<PathBuf> {
    find_tool("atracdenc", "BBT_ATRACDENC_PATH")
        .or_else(|| find_tool("atracenc", "BBT_ATRACENC_PATH"))
}

/// Resolved paths for all external tools. Used by `bbt check`.
pub struct BinaryPaths {
    pub ffmpeg: Option<PathBuf>,
    pub ffprobe: Option<PathBuf>,
    pub atracdenc: Option<PathBuf>,
}

impl BinaryPaths {
    pub fn detect() -> Self {
        Self {
            ffmpeg: find_ffmpeg(),
            ffprobe: find_ffprobe(),
            atracdenc: find_atracdenc(),
        }
    }
}

// ── Core discovery ────────────────────────────────────────────────────────────

fn find_tool(name: &str, env_var: &str) -> Option<PathBuf> {
    // 1. Explicit env var
    if let Ok(val) = std::env::var(env_var) {
        let p = PathBuf::from(&val);
        if p.exists() {
            tracing::debug!(tool = name, source = "env", path = %p.display(), "found");
            return Some(p);
        }
        tracing::warn!(
            tool = name,
            env_var,
            path = %p.display(),
            "env var set but binary not found at that path"
        );
    }

    // 2. Alongside the bbt binary (bbt platform releases bundle tools here)
    if let Some(p) = alongside_bbt(name) {
        tracing::debug!(tool = name, source = "alongside", path = %p.display(), "found");
        return Some(p);
    }

    // 3. System PATH
    if let Ok(p) = which::which(name) {
        tracing::debug!(tool = name, source = "PATH", path = %p.display(), "found");
        return Some(p);
    }

    None
}

fn alongside_bbt(name: &str) -> Option<PathBuf> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_owned();
    // which_in handles adding .exe on Windows automatically
    which::which_in(name, Some(exe_dir.as_os_str()), ".").ok()
}
