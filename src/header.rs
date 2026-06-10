//! ProTracker / SoundTracker MOD header parser.
//!
//! Layout (little-endian is not used — all multi-byte fields are
//! big-endian):
//!
//! ```text
//! Offset 0        20 bytes      Song title (null-padded ASCII)
//! Offset 20      31 * 30 bytes  Sample definitions:
//!                                 22 bytes name
//!                                  2 bytes length (in 16-bit words, BE)
//!                                  1 byte  finetune  (low 4 bits, signed)
//!                                  1 byte  volume    (0..64)
//!                                  2 bytes repeat-start (words, BE)
//!                                  2 bytes repeat-length (words, BE)
//! Offset 950      1 byte        Song length (1..128)
//! Offset 951      1 byte        Restart byte (0x7F typical)
//! Offset 952    128 bytes       Pattern-order table
//! Offset 1080     4 bytes       Signature: "M.K.", "M!K!", "4CHN",
//!                                "6CHN", "8CHN", "xxCH" (xx 10..32)
//! Offset 1084      …            Pattern data: 64 rows × channels × 4 bytes
//! After patterns               Raw sample bodies (signed 8-bit)
//! ```

use oxideav_core::{Error, Result};

pub const HEADER_FIXED_SIZE: usize = 1084;
pub const PATTERN_ROWS: usize = 64;
pub const SAMPLE_COUNT: usize = 31;
pub const ORDER_TABLE_SIZE: usize = 128;

#[derive(Clone, Debug)]
pub struct Sample {
    pub name: String,
    /// Sample length in *samples* (spec stores words — we've doubled).
    pub length: u32,
    /// Finetune value, signed 4-bit (-8..=7).
    pub finetune: i8,
    /// Volume 0..=64.
    pub volume: u8,
    /// Loop start in samples.
    pub repeat_start: u32,
    /// Loop length in samples (0 or 2 = no loop).
    pub repeat_length: u32,
}

impl Sample {
    /// True when the header declares a usable loop region.
    ///
    /// Per `Protracker-effects-MODFIL12.txt` lines 357-365 ("A sample
    /// is only looped if this value is greater than 2 bytes"), a
    /// `repeat_length` of `0` or `2` means the sample is one-shot.
    /// PT writers commonly emit `repeat_length = 2` as the default
    /// "no loop" sentinel, so callers must check both — a plain
    /// `repeat_length > 0` test would flag non-looped samples as
    /// looped.
    pub fn is_looped(&self) -> bool {
        self.repeat_length > 2
    }

    /// Header-declared loop region as a half-open `[start, start +
    /// length)` byte-position pair, or `None` when [`is_looped`] is
    /// `false`.
    ///
    /// Positions are in **samples** (matching `length`), already
    /// doubled from the on-disk word counts. The values are taken
    /// directly from the header without clamping against the
    /// extracted PCM body — `samples::extract_samples` performs the
    /// PCM-bounded clamp when building [`SampleBody`] for the mixer,
    /// because the extracted body length can be shorter than the
    /// declared `length` on truncated rips. Use this accessor when
    /// the header is the source of truth (e.g. metadata reporters,
    /// trackers showing the authored loop), and `SampleBody` when
    /// the PCM-aware clamped region is needed.
    ///
    /// [`is_looped`]: Self::is_looped
    /// [`SampleBody`]: crate::samples::SampleBody
    pub fn loop_region(&self) -> Option<(u32, u32)> {
        if self.is_looped() {
            Some((self.repeat_start, self.repeat_length))
        } else {
            None
        }
    }
}

#[derive(Clone, Debug)]
pub struct ModHeader {
    pub title: String,
    pub samples: Vec<Sample>,
    pub song_length: u8,
    pub restart: u8,
    pub order: Vec<u8>,
    pub signature: [u8; 4],
    pub channels: u8,
    /// Number of distinct patterns referenced by the order table.
    pub n_patterns: u8,
}

impl ModHeader {
    /// Total size of the header block preceding sample data (in bytes).
    pub fn pattern_data_offset(&self) -> usize {
        HEADER_FIXED_SIZE
    }

    /// True for Startrekker 8-channel `FLT8` modules, whose pattern
    /// data is stored as **paired 4-channel patterns** rather than as
    /// flat interleaved 8-channel rows.
    ///
    /// Per `Startrekker-mod.txt` (format-author description): "the
    /// patterns are PAIRED … in a 8 track FLT8 module, patterns 00
    /// and 01 is 'really' pattern 00. Patterns 02 and 03 together is
    /// 'really' pattern 01." Each stored pattern keeps the normal
    /// 4-channel × 64-row × 4-byte (0x400) layout; stored pattern
    /// `2k` carries channels 1-4 and stored pattern `2k+1` carries
    /// channels 5-8 of logical pattern `k`. The same doc's format
    /// summary adds: "Divide all patterns in the orderlist by 2" —
    /// the on-disk order table references the even stored-pattern
    /// indices, so [`parse_header`] halves the entries up front and
    /// `order` / `n_patterns` are already in logical-pattern terms.
    pub fn is_flt8(&self) -> bool {
        self.signature == *b"FLT8"
    }

    /// Size of the pattern data region in bytes.
    ///
    /// The formula also holds for the paired `FLT8` layout: each
    /// logical 8-channel pattern occupies two stored 4-channel
    /// patterns of `PATTERN_ROWS * 4 * 4` bytes each, which is
    /// exactly `PATTERN_ROWS * 8 * 4` bytes per logical pattern.
    pub fn pattern_data_size(&self) -> usize {
        self.n_patterns as usize * PATTERN_ROWS * self.channels as usize * 4
    }

    /// Absolute offset where sample bodies begin.
    pub fn sample_data_offset(&self) -> usize {
        HEADER_FIXED_SIZE + self.pattern_data_size()
    }
}

pub fn parse_header(bytes: &[u8]) -> Result<ModHeader> {
    if bytes.len() < HEADER_FIXED_SIZE {
        return Err(Error::NeedMore);
    }
    let title = read_padded_ascii(&bytes[0..20]);

    let mut samples = Vec::with_capacity(SAMPLE_COUNT);
    for i in 0..SAMPLE_COUNT {
        let off = 20 + i * 30;
        let name = read_padded_ascii(&bytes[off..off + 22]);
        let len_words = u16::from_be_bytes([bytes[off + 22], bytes[off + 23]]) as u32;
        let finetune_raw = bytes[off + 24] & 0x0F;
        let finetune = if finetune_raw & 0x08 != 0 {
            (finetune_raw as i8) - 16
        } else {
            finetune_raw as i8
        };
        let volume = bytes[off + 25].min(64);
        let repeat_start_words = u16::from_be_bytes([bytes[off + 26], bytes[off + 27]]) as u32;
        let repeat_length_words = u16::from_be_bytes([bytes[off + 28], bytes[off + 29]]) as u32;
        samples.push(Sample {
            name,
            length: len_words.saturating_mul(2),
            finetune,
            volume,
            repeat_start: repeat_start_words.saturating_mul(2),
            repeat_length: repeat_length_words.saturating_mul(2),
        });
    }

    let song_length = bytes[950];
    let restart = bytes[951];
    let mut order: Vec<u8> = bytes[952..952 + ORDER_TABLE_SIZE].to_vec();

    let mut signature = [0u8; 4];
    signature.copy_from_slice(&bytes[1080..1084]);
    let channels = channels_from_signature(&signature)?;

    // Startrekker FLT8: the on-disk order table references the even
    // stored-pattern indices ("Divide all patterns in the orderlist
    // by 2" — Startrekker-mod.txt). Halve up front so `order` and
    // `n_patterns` are in logical 8-channel-pattern terms and every
    // downstream consumer (player position walk, metadata) can stay
    // signature-agnostic. The paired stored layout is resolved by
    // `player::parse_patterns` via `ModHeader::is_flt8`.
    if signature == *b"FLT8" {
        for o in order.iter_mut() {
            *o /= 2;
        }
    }

    let n_patterns = 1 + *order.iter().take(song_length as usize).max().unwrap_or(&0);

    Ok(ModHeader {
        title,
        samples,
        song_length,
        restart,
        order,
        signature,
        channels,
        n_patterns,
    })
}

fn channels_from_signature(sig: &[u8; 4]) -> Result<u8> {
    match sig {
        b"M.K." | b"M!K!" | b"FLT4" | b"4CHN" => Ok(4),
        b"6CHN" => Ok(6),
        b"8CHN" | b"OCTA" | b"CD81" | b"FLT8" => Ok(8),
        // "xxCH" with xx in 10..=32 (Fast Tracker / TakeTracker).
        other if other[2] == b'C' && other[3] == b'H' => {
            let tens = (other[0] as char).to_digit(10);
            let ones = (other[1] as char).to_digit(10);
            match (tens, ones) {
                (Some(t), Some(o)) => {
                    let n = (t * 10 + o) as u8;
                    if (10..=32).contains(&n) {
                        Ok(n)
                    } else {
                        Err(Error::unsupported(format!(
                            "MOD: unsupported channel count {n}"
                        )))
                    }
                }
                _ => Err(Error::invalid(format!(
                    "MOD: unknown signature {:?}",
                    std::str::from_utf8(other).unwrap_or("????")
                ))),
            }
        }
        _ => Err(Error::invalid(format!(
            "MOD: unknown signature {:?}",
            std::str::from_utf8(sig).unwrap_or("????")
        ))),
    }
}

fn read_padded_ascii(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end])
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fake_mod(channels: &[u8; 4], song_length: u8) -> Vec<u8> {
        let mut out = vec![0u8; HEADER_FIXED_SIZE];
        out[0..8].copy_from_slice(b"test\0\0\0\0");
        // sample 0: empty
        // song length + order table
        out[950] = song_length;
        out[951] = 0x7F;
        for i in 0..song_length as usize {
            out[952 + i] = 0;
        }
        out[1080..1084].copy_from_slice(channels);
        out
    }

    #[test]
    fn signature_mk() {
        let h = parse_header(&make_fake_mod(b"M.K.", 1)).unwrap();
        assert_eq!(h.channels, 4);
        assert_eq!(h.signature, *b"M.K.");
        assert_eq!(h.song_length, 1);
        assert_eq!(h.samples.len(), 31);
    }

    #[test]
    fn signature_6chn() {
        let h = parse_header(&make_fake_mod(b"6CHN", 2)).unwrap();
        assert_eq!(h.channels, 6);
    }

    #[test]
    fn signature_14ch() {
        let h = parse_header(&make_fake_mod(b"14CH", 1)).unwrap();
        assert_eq!(h.channels, 14);
    }

    #[test]
    fn rejects_unknown_signature() {
        let bytes = make_fake_mod(b"XXXX", 1);
        assert!(parse_header(&bytes).is_err());
    }

    #[test]
    fn flt8_order_entries_are_halved_to_logical_patterns() {
        // Startrekker-mod.txt: "Divide all patterns in the orderlist
        // by 2" — the on-disk table references the even stored
        // 4-channel pattern indices; the parsed header exposes
        // logical 8-channel pattern numbers.
        let mut bytes = make_fake_mod(b"FLT8", 3);
        bytes[952] = 0;
        bytes[953] = 2;
        bytes[954] = 4;
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.channels, 8);
        assert!(h.is_flt8());
        assert_eq!(&h.order[..3], &[0, 1, 2]);
        assert_eq!(h.n_patterns, 3);
        // 3 logical patterns = 6 stored 0x400-byte 4-channel
        // patterns, so the byte-count formula is unchanged.
        assert_eq!(h.pattern_data_size(), 6 * 0x400);
        assert_eq!(h.sample_data_offset(), HEADER_FIXED_SIZE + 6 * 0x400);
    }

    #[test]
    fn non_flt8_eight_channel_signature_is_not_paired() {
        // 8CHN / OCTA / CD81 use the flat interleaved 8-channel rows;
        // only FLT8 gets the paired-stored-pattern treatment and the
        // order-table halving.
        let mut bytes = make_fake_mod(b"8CHN", 2);
        bytes[952] = 0;
        bytes[953] = 2;
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.channels, 8);
        assert!(!h.is_flt8());
        assert_eq!(&h.order[..2], &[0, 2]);
        assert_eq!(h.n_patterns, 3);
    }

    fn sample_with_repeat(start: u32, length: u32) -> Sample {
        Sample {
            name: String::new(),
            length: 1024,
            finetune: 0,
            volume: 64,
            repeat_start: start,
            repeat_length: length,
        }
    }

    #[test]
    fn sample_is_looped_rejects_repeat_length_zero_and_two() {
        // The two canonical "no loop" sentinels per
        // Protracker-effects-MODFIL12.txt lines 357-365.
        assert!(!sample_with_repeat(0, 0).is_looped());
        assert!(!sample_with_repeat(0, 2).is_looped());
        assert!(!sample_with_repeat(64, 2).is_looped());
    }

    #[test]
    fn sample_is_looped_accepts_repeat_length_above_two() {
        // The smallest legitimate loop region is length 4 (PT writes
        // word-aligned values, so the next step above the no-loop
        // sentinel is 4). length 3 still satisfies the strict ">2"
        // spec wording.
        assert!(sample_with_repeat(0, 3).is_looped());
        assert!(sample_with_repeat(0, 4).is_looped());
        assert!(sample_with_repeat(128, 256).is_looped());
    }

    #[test]
    fn sample_loop_region_none_when_not_looped() {
        assert_eq!(sample_with_repeat(0, 0).loop_region(), None);
        assert_eq!(sample_with_repeat(0, 2).loop_region(), None);
        // A non-zero repeat_start with the no-loop length sentinel is
        // still "no loop" — PT consults the length, not the start.
        assert_eq!(sample_with_repeat(64, 2).loop_region(), None);
    }

    #[test]
    fn sample_loop_region_returns_header_pair_unclamped() {
        // The accessor reflects what the header declares. PCM-bounded
        // clamping happens in samples::extract_samples; the header-side
        // view stays raw so metadata reporters see authored values.
        assert_eq!(sample_with_repeat(128, 256).loop_region(), Some((128, 256)));
        // Out-of-bounds-relative-to-length values pass through here;
        // it's the PCM-aware path that clamps them.
        let mut s = sample_with_repeat(2000, 4000);
        s.length = 1024;
        assert_eq!(s.loop_region(), Some((2000, 4000)));
    }
}
