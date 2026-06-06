// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Device profiles declare what a target device requires.
//! Profiles are TOML files — human-readable and human-authored.
//! They express intent ("this device wants MP3 at 320 kbps") but
//! do not contain resolved parameters; that is the planner's job.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::error::BbtError;

/// A device profile: what format, codec, and constraints a target device requires.
/// `None` values mean "preserve from source" — the planner resolves them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceProfile {
    pub id: String,
    pub name: String,
    pub vendor: Option<String>,
    pub description: String,

    /// Target container (e.g. "mp3", "m4a", "ogg", "oma", "wav")
    pub container: String,
    /// Target codec (e.g. "mp3", "aac", "flac", "vorbis", "atrac1", "atrac3")
    pub codec: String,
    /// Target bitrate in kbps. Required for lossy codecs.
    pub bitrate_kbps: Option<u32>,
    /// Target sample rate in Hz. None = preserve source.
    pub sample_rate_hz: Option<u32>,
    /// Target channel count. None = preserve source.
    pub channels: Option<u8>,
    /// Whether to force CBR encoding. Should be true for deterministic output.
    pub cbr: bool,
    /// File extension for output files (e.g. "mp3", "m4a", "oma")
    pub extension: String,
    /// Human-readable notes about this profile
    pub notes: Option<String>,
}

impl DeviceProfile {
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|_| BbtError::ProfileNotFound(path.display().to_string()))?;
        toml::from_str(&content).map_err(|e| {
            BbtError::InvalidProfile {
                path: path.to_owned(),
                reason: e.to_string(),
            }
            .into()
        })
    }

    pub fn load_by_id(id: &str, profile_dirs: &[PathBuf]) -> Result<Self> {
        // Check built-in profiles first
        if let Some(profile) = builtin_profile(id) {
            return Ok(profile);
        }

        // Then search profile dirs
        for dir in profile_dirs {
            let path = dir.join(format!("{id}.toml"));
            if path.exists() {
                return Self::load_from_file(&path)
                    .with_context(|| format!("loading profile '{id}'"));
            }
        }

        Err(BbtError::ProfileNotFound(id.to_string()).into())
    }
}

/// Lists all built-in profile IDs with their display names.
pub fn list_builtin_profiles() -> Vec<(String, String)> {
    BUILTIN_PROFILES
        .iter()
        .map(|(id, content)| {
            let parsed: toml::Value = toml::from_str(content).expect("builtin profile is valid");
            let name = parsed
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .to_string();
            (id.to_string(), name)
        })
        .collect()
}

fn builtin_profile(id: &str) -> Option<DeviceProfile> {
    BUILTIN_PROFILES
        .iter()
        .find(|(pid, _)| *pid == id)
        .map(|(_, content)| toml::from_str(content).expect("builtin profile is valid TOML"))
}

// Built-in profiles are embedded at compile time.
static BUILTIN_PROFILES: &[(&str, &str)] = &[
    ("minidisc-sp",       include_str!("../profiles/minidisc-sp.toml")),
    ("minidisc-lp2",      include_str!("../profiles/minidisc-lp2.toml")),
    ("minidisc-lp4",      include_str!("../profiles/minidisc-lp4.toml")),
    ("himd-sp",           include_str!("../profiles/himd-sp.toml")),
    ("generic-mp3-128",   include_str!("../profiles/generic-mp3-128.toml")),
    ("generic-mp3-192",   include_str!("../profiles/generic-mp3-192.toml")),
    ("generic-mp3-320",   include_str!("../profiles/generic-mp3-320.toml")),
    ("generic-aac-128",   include_str!("../profiles/generic-aac-128.toml")),
    ("generic-aac-256",   include_str!("../profiles/generic-aac-256.toml")),
    ("generic-flac",      include_str!("../profiles/generic-flac.toml")),
    ("generic-ogg-192",   include_str!("../profiles/generic-ogg-192.toml")),
];
