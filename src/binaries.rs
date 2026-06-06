// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Centralized binary discovery for external encoder tools.
//!
//! Resolution order for every binary:
//!   1. Explicit env var (e.g. BBT_FFMPEG_PATH) — BakBeat sets these to its
//!      bundled copies so users never need to install anything manually.
//!   2. Same directory as the bbt binary — dropping ffmpeg next to bbt works.
//!   3. System PATH — fallback for standalone / developer use.
//!
//! This lets the BakBeat app ship ffmpeg, ffprobe, and atracdenc bundled
//! alongside bbt without requiring end users to install anything.

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
/// BakBeat should set `BBT_ATRACDENC_PATH` to its bundled copy.
pub fn find_atracdenc() -> Option<PathBuf> {
    find_tool("atracdenc", "BBT_ATRACDENC_PATH")
        .or_else(|| find_tool("atracenc", "BBT_ATRACENC_PATH"))
}

/// Report the resolved path (or None) for each tool. Used by `bbt check`.
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
    // 1. Explicit env var override
    if let Ok(val) = std::env::var(env_var) {
        let p = PathBuf::from(&val);
        if p.exists() {
            tracing::debug!(tool = name, source = "env", path = %p.display(), "found");
            return Some(p);
        }
        // Env var set but path doesn't exist — warn and keep looking
        tracing::warn!(
            tool = name,
            env_var,
            path = %p.display(),
            "env var set but binary not found at that path"
        );
    }

    // 2. Same directory as the bbt binary
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
