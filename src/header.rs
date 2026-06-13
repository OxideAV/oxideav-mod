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

/// Ultimate SoundTracker (15-sample) header constants.
///
/// Per `docs/audio/trackers/mod/Ultimate-Soundtracker-mod.txt` the UST
/// layout differs from the 31-sample ProTracker/SoundTracker layout in
/// the number of sample slots (15 vs 31) and therefore in the offsets of
/// every field after the sample table:
///
/// ```text
/// + 0    20 bytes      song/module working title
/// + 20   15 * 30 bytes 15 sample headers
/// + 470  1 byte        song length (number of steps in pattern table)
/// + 471  1 byte        song speed in BPM (NOT a restart byte)
/// + 472  128 bytes     pattern step (order) table
/// + 600  …             pattern data (1024 bytes / 4-channel pattern)
/// ```
///
/// 15 samples × 30 bytes = 450 bytes, so the fixed header preceding the
/// pattern data is 600 bytes (vs 1084 for the 31-sample layout). There
/// is no 4-byte signature at offset 1080 — UST predates the `M.K.`
/// format ID, so the variant must be selected by the caller rather than
/// detected from a magic.
pub const UST_SAMPLE_COUNT: usize = 15;
/// Fixed header size for the 15-sample UST layout (pattern data starts here).
pub const UST_HEADER_FIXED_SIZE: usize = 600;
/// Offset of the song-length byte in the UST layout.
pub const UST_SONG_LENGTH_OFFSET: usize = 470;
/// Offset of the BPM byte in the UST layout (`AMIGA Timer-IRQ = (240-bpm)*122`).
pub const UST_BPM_OFFSET: usize = 471;
/// Offset of the 128-entry pattern order table in the UST layout.
pub const UST_ORDER_TABLE_OFFSET: usize = 472;

/// Which on-disk header layout a [`ModHeader`] was parsed from.
///
/// The two variants share the [`ModHeader`] / [`Sample`] / pattern-cell
/// shapes after parsing (the UST parser normalises its fields into the
/// same units the 31-sample parser uses), but they differ in the byte
/// offsets of the order table / pattern data / sample bodies and in how
/// the pattern-cell effect column is interpreted. Downstream code keys
/// off this enum via [`ModHeader::pattern_data_offset`] /
/// [`ModHeader::sample_data_offset`] and the variant-aware effect
/// translation in `player::parse_patterns`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModVariant {
    /// The 31-sample ProTracker / SoundTracker-2.x family (signature at +1080).
    Standard31,
    /// The 15-sample Ultimate SoundTracker layout (no signature).
    UltimateSoundTracker15,
}

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
    /// Which on-disk layout this header was parsed from.
    pub variant: ModVariant,
}

impl ModHeader {
    /// True for the 15-sample Ultimate SoundTracker layout.
    ///
    /// Cited in `docs/audio/trackers/mod/Ultimate-Soundtracker-mod.txt`
    /// ("15 sample headers", pattern data at +600). UST modules carry no
    /// signature, so the variant is selected by the parser entry point
    /// ([`parse_ust_header`] vs [`parse_header`]) rather than detected
    /// from a magic.
    pub fn is_ust(&self) -> bool {
        self.variant == ModVariant::UltimateSoundTracker15
    }

    /// Total size of the header block preceding pattern data (in bytes).
    ///
    /// The 31-sample layout reserves a fixed 1084-byte header; the UST
    /// 15-sample layout reserves only 600 bytes because it has 16 fewer
    /// 30-byte sample slots and no 4-byte signature word.
    pub fn pattern_data_offset(&self) -> usize {
        match self.variant {
            ModVariant::Standard31 => HEADER_FIXED_SIZE,
            ModVariant::UltimateSoundTracker15 => UST_HEADER_FIXED_SIZE,
        }
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
        self.pattern_data_offset() + self.pattern_data_size()
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
        variant: ModVariant::Standard31,
    })
}

/// Parse the 15-sample Ultimate SoundTracker header layout.
///
/// UST predates the `M.K.` format ID, so there is no signature to detect
/// the variant from — the caller selects this entry point explicitly
/// (per the doc's recommendation to "either default to UST or provide a
/// switch to toggle between UST and ST"). Fields are normalised into the
/// same units the 31-sample [`parse_header`] uses so that the resulting
/// [`ModHeader`] drives the existing pattern / sample / player path
/// unchanged (modulo the variant-aware offsets and effect translation):
///
/// - sample `length` and `repeat_length`: on-disk **words** → samples
///   (× 2), matching the 31-sample parser;
/// - sample `repeat_start`: on-disk **bytes** in UST (NOT words as in
///   PT / NT / ST-2.5) — passed straight through without the × 2 word
///   scaling. Cited in `Ultimate-Soundtracker-mod.txt`: "Sample repeat
///   offset is in bytes (unlike PT, NT, and ST 2.5, where it is
///   specified as number of words)";
/// - `finetune` is fixed to 0 — UST has no finetune nibble (the byte at
///   +24 is the high half of the volume word, documented as not carrying
///   a finetune value);
/// - a synthetic `*b"M.K."` signature and 4-channel count are filled in
///   so signature-keyed consumers see a 4-channel module (UST is always
///   4 voices), while `variant` records the true UST origin.
///
/// The byte at +471 is the song speed in BPM, **not** a restart marker
/// (`AMIGA Timer-IRQ value = (240-bpm)*122`); it is surfaced through the
/// `restart` field for callers that want it, but is not interpreted as a
/// loop position. Cited in `Ultimate-Soundtracker-mod.txt`.
pub fn parse_ust_header(bytes: &[u8]) -> Result<ModHeader> {
    if bytes.len() < UST_HEADER_FIXED_SIZE {
        return Err(Error::NeedMore);
    }
    let title = read_padded_ascii(&bytes[0..20]);

    let mut samples = Vec::with_capacity(UST_SAMPLE_COUNT);
    for i in 0..UST_SAMPLE_COUNT {
        let off = 20 + i * 30;
        let name = read_padded_ascii(&bytes[off..off + 22]);
        let len_words = u16::from_be_bytes([bytes[off + 22], bytes[off + 23]]) as u32;
        // +24 is the volume word; UST documents no finetune nibble, and
        // the high byte of the volume word does not carry one either.
        let volume = bytes[off + 25].min(64);
        // Repeat offset is in BYTES in UST (unlike the word counts of
        // PT / NT / ST-2.5), so no word→sample scaling here.
        let repeat_start_bytes = u16::from_be_bytes([bytes[off + 26], bytes[off + 27]]) as u32;
        let repeat_length_words = u16::from_be_bytes([bytes[off + 28], bytes[off + 29]]) as u32;
        samples.push(Sample {
            name,
            length: len_words.saturating_mul(2),
            finetune: 0,
            volume,
            repeat_start: repeat_start_bytes,
            repeat_length: repeat_length_words.saturating_mul(2),
        });
    }

    let song_length = bytes[UST_SONG_LENGTH_OFFSET];
    // +471 is the BPM speed byte, not a restart position. Surfaced via
    // `restart` for completeness; the player does not treat it as a loop
    // marker.
    let restart = bytes[UST_BPM_OFFSET];
    let order: Vec<u8> =
        bytes[UST_ORDER_TABLE_OFFSET..UST_ORDER_TABLE_OFFSET + ORDER_TABLE_SIZE].to_vec();

    // UST is always 4-channel; fill a synthetic signature so the rest of
    // the pipeline (channel count, metadata) behaves like a 4-channel
    // module while `variant` records the true origin.
    let signature = *b"M.K.";
    let channels = 4;

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
        variant: ModVariant::UltimateSoundTracker15,
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

    #[test]
    fn standard_header_reports_standard31_variant() {
        let h = parse_header(&make_fake_mod(b"M.K.", 1)).unwrap();
        assert_eq!(h.variant, ModVariant::Standard31);
        assert!(!h.is_ust());
        assert_eq!(h.pattern_data_offset(), HEADER_FIXED_SIZE);
    }

    /// Build a minimal valid 15-sample Ultimate SoundTracker file with
    /// `song_length` order entries (all → pattern 0) and one 1024-byte
    /// 4-channel pattern's worth of trailing space.
    fn make_fake_ust(song_length: u8) -> Vec<u8> {
        // 600-byte header + one 0x400 pattern.
        let mut out = vec![0u8; UST_HEADER_FIXED_SIZE + 0x400];
        out[0..4].copy_from_slice(b"ust\0");
        out[UST_SONG_LENGTH_OFFSET] = song_length;
        out[UST_BPM_OFFSET] = 0x78; // 120 BPM default
        for i in 0..song_length as usize {
            out[UST_ORDER_TABLE_OFFSET + i] = 0;
        }
        out
    }

    #[test]
    fn ust_header_layout_and_variant() {
        let h = parse_ust_header(&make_fake_ust(1)).unwrap();
        assert_eq!(h.variant, ModVariant::UltimateSoundTracker15);
        assert!(h.is_ust());
        assert_eq!(h.title, "ust");
        assert_eq!(h.channels, 4);
        assert_eq!(h.signature, *b"M.K.");
        assert_eq!(h.samples.len(), UST_SAMPLE_COUNT);
        assert_eq!(h.song_length, 1);
        // +471 is the BPM byte (0x78 = 120), surfaced through `restart`.
        assert_eq!(h.restart, 0x78);
        // Pattern data starts at +600, not +1084.
        assert_eq!(h.pattern_data_offset(), UST_HEADER_FIXED_SIZE);
        assert_eq!(h.n_patterns, 1);
        // One 4-channel pattern = 64 rows × 4 ch × 4 bytes = 0x400.
        assert_eq!(h.pattern_data_size(), 0x400);
        assert_eq!(h.sample_data_offset(), UST_HEADER_FIXED_SIZE + 0x400);
    }

    #[test]
    fn ust_too_short_needs_more() {
        let short = vec![0u8; UST_HEADER_FIXED_SIZE - 1];
        assert!(matches!(parse_ust_header(&short), Err(Error::NeedMore)));
    }

    #[test]
    fn ust_sample_fields_repeat_offset_in_bytes_no_finetune() {
        let mut bytes = make_fake_ust(1);
        // Sample 0 header at +20.
        let off = 20;
        bytes[off..off + 4].copy_from_slice(b"snd\0");
        // length in words = 100 → 200 samples.
        bytes[off + 22..off + 24].copy_from_slice(&100u16.to_be_bytes());
        // +24 is the high byte of the volume word (no finetune); +25 is volume.
        bytes[off + 24] = 0xFF; // would be a finetune nibble in PT — ignored in UST
        bytes[off + 25] = 40;
        // Repeat offset (+26) is in BYTES in UST — pass straight through.
        bytes[off + 26..off + 28].copy_from_slice(&50u16.to_be_bytes());
        // Repeat length (+28) is in words → ×2 = samples.
        bytes[off + 28..off + 30].copy_from_slice(&30u16.to_be_bytes());

        let h = parse_ust_header(&bytes).unwrap();
        let s = &h.samples[0];
        assert_eq!(s.name, "snd");
        assert_eq!(s.length, 200);
        // No finetune nibble in UST regardless of the +24 byte.
        assert_eq!(s.finetune, 0);
        assert_eq!(s.volume, 40);
        // Repeat start stays 50 (bytes), NOT 100 (would-be word doubling).
        assert_eq!(s.repeat_start, 50);
        // Repeat length doubled from words: 30 → 60.
        assert_eq!(s.repeat_length, 60);
        assert!(s.is_looped());
    }

    #[test]
    fn ust_volume_clamped_to_64() {
        let mut bytes = make_fake_ust(1);
        bytes[20 + 25] = 100; // > 64
        let h = parse_ust_header(&bytes).unwrap();
        assert_eq!(h.samples[0].volume, 64);
    }
}
