// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! iTunSMPB gapless metadata parsing.
//!
//! Aligns with BakBeat `gapless_ffi.rs` / `parse_itunsmpb_payload` semantics.
//! Only handles compensation-eligible sources (iTunSMPB from tag); no heuristics.

/// Parsed iTunSMPB gapless compensation parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct ItunesSmpb {
    /// Encoder priming / lead-in samples (hex word 1).
    pub encoder_delay: u64,
    /// Trailing padding samples to strip from the end (hex word 2).
    pub trailing_padding: u64,
    /// Authoritative total valid PCM sample count (hex word 3, optional).
    /// Matches afconvert output length when present.
    pub total_pcm_samples: Option<u64>,
}

/// Parse an iTunSMPB tag value into gapless compensation parameters.
///
/// Format: space-separated hex words `<w0> <w1> <w2> [<w3> ...]`
/// - w1 = encoder_delay_samples
/// - w2 = trailing_padding_samples
/// - w3 = total_pcm_samples (optional, authoritative when present)
///
/// Returns `None` if the value has fewer than 3 words or contains invalid hex.
pub fn parse_itunsmpb(value: &str) -> Option<ItunesSmpb> {
    let words: Vec<&str> = value.split_whitespace().collect();
    if words.len() < 3 {
        return None;
    }
    let encoder_delay = u64::from_str_radix(words[1], 16).ok()?;
    let trailing_padding = u64::from_str_radix(words[2], 16).ok()?;
    let total_pcm_samples = words.get(3).and_then(|w| u64::from_str_radix(w, 16).ok());
    Some(ItunesSmpb { encoder_delay, trailing_padding, total_pcm_samples })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dbpoweramp_fixture() {
        // iTunSMPB value from dbpoweramp-m4a.m4a
        // delay=0x840=2112, trailing=0x3C8=968, total=0xAE13F8=11408376
        let tag = "00000000 00000840 000003C8 0000000000AE13F8";
        let smpb = parse_itunsmpb(tag).expect("should parse");
        assert_eq!(smpb.encoder_delay, 2112);
        assert_eq!(smpb.trailing_padding, 968);
        assert_eq!(smpb.total_pcm_samples, Some(11408376));
    }

    #[test]
    fn parse_without_total_pcm() {
        let tag = "00000000 00000840 000003C8";
        let smpb = parse_itunsmpb(tag).expect("should parse");
        assert_eq!(smpb.encoder_delay, 2112);
        assert_eq!(smpb.trailing_padding, 968);
        assert_eq!(smpb.total_pcm_samples, None);
    }

    #[test]
    fn parse_zero_trailing_padding() {
        let tag = "00000000 00000840 00000000 0000000000AE13F8";
        let smpb = parse_itunsmpb(tag).expect("should parse");
        assert_eq!(smpb.trailing_padding, 0);
    }

    #[test]
    fn parse_too_few_words_returns_none() {
        assert!(parse_itunsmpb("00000000 00000840").is_none());
    }

    #[test]
    fn parse_invalid_hex_returns_none() {
        assert!(parse_itunsmpb("00000000 GGGGGGGG 000003C8").is_none());
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_itunsmpb("").is_none());
    }

    #[test]
    fn total_pcm_matches_afconvert_contract() {
        // Authoritative: word3 (total_pcm_samples) must equal afconvert output length.
        // For the dbpoweramp fixture: afconvert outputs 11408376 frames.
        let tag = "00000000 00000840 000003C8 0000000000AE13F8";
        let smpb = parse_itunsmpb(tag).unwrap();
        assert_eq!(smpb.total_pcm_samples, Some(11408376));
    }
}
