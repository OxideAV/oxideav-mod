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
//! Offset 1080     4 bytes       Signature / format tag (see
//!                                `channels_from_signature`): "M.K." /
//!                                "M!K!" / "M&K!" / "FLT4" / "FLT8" /
//!                                "OCTA" / "OKTA" / "CD81" / "dCHN"
//!                                (d=1..9) / "xxCH" / "xxCN" (10..32) /
//!                                "TDZx" (x=1..3)
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

/// Offset of the SoundTracker 2.6 / IceTracker 128×4 track-index table
/// (`Soundtracker-v2.6-IceTracker-st26.txt`: "0952 128*4 Track indices
/// for each pattern" — one byte per channel per pattern-list position).
pub const ST26_TRACK_TABLE_OFFSET: usize = 952;
/// Offset of the ST2.6 / IceTracker magic ID (`MTN\0` / `IT10`), per
/// `Soundtracker-v2.6-IceTracker-st26.txt` ("1464 4 Magic ID").
pub const ST26_MAGIC_OFFSET: usize = 1464;
/// Offset where ST2.6 track data begins ("1468 ? Track data (stored
/// like Protracker)").
pub const ST26_TRACK_DATA_OFFSET: usize = 1468;
/// Bytes per stored ST2.6 track: 64 rows × one 4-byte channel cell.
pub const ST26_TRACK_BYTES: usize = PATTERN_ROWS * 4;

/// True when `bytes` carries the SoundTracker 2.6 / IceTracker magic at
/// offset 1464 (`MTN\0` for SoundTracker 2.6, `IT10` for IceTracker
/// 1.0/1.1, per `Soundtracker-v2.6-IceTracker-st26.txt`).
pub fn is_st26_magic(bytes: &[u8]) -> bool {
    bytes.len() >= ST26_MAGIC_OFFSET + 4
        && (&bytes[ST26_MAGIC_OFFSET..ST26_MAGIC_OFFSET + 4] == b"MTN\0"
            || &bytes[ST26_MAGIC_OFFSET..ST26_MAGIC_OFFSET + 4] == b"IT10")
}

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
    /// SoundTracker 2.6 / IceTracker (`MTN\0` / `IT10` magic at +1464).
    ///
    /// Per `Soundtracker-v2.6-IceTracker-st26.txt`: each 64-row track is
    /// stored independently, and the 128-position pattern list holds
    /// FOUR track indices per position (one per channel) instead of one
    /// pattern number. The parser normalises the list to an identity
    /// order table (position `i` plays synthesized logical pattern `i`),
    /// and `player::parse_patterns` assembles each logical pattern from
    /// its four indexed tracks. `n_tracks` is the "Number of stored
    /// tracks" header byte (+951), needed to size the track-data region
    /// that precedes the sample bodies.
    SoundTracker26 {
        /// Number of stored 256-byte tracks (header byte +951).
        n_tracks: u8,
    },
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

    /// True for the SoundTracker 2.6 / IceTracker per-track layout.
    pub fn is_st26(&self) -> bool {
        matches!(self.variant, ModVariant::SoundTracker26 { .. })
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
            // ST2.6 track data starts right after the +1464 magic
            // (`Soundtracker-v2.6-IceTracker-st26.txt`: "1468 ? Track
            // data (stored like Protracker)").
            ModVariant::SoundTracker26 { .. } => ST26_TRACK_DATA_OFFSET,
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
        match self.variant {
            // ST2.6 stores each 64-row single-channel track once; the
            // region before the sample bodies is `n_tracks` tracks of
            // 256 bytes, NOT `n_patterns` interleaved patterns
            // (`Soundtracker-v2.6-IceTracker-st26.txt`).
            ModVariant::SoundTracker26 { n_tracks } => n_tracks as usize * ST26_TRACK_BYTES,
            _ => self.n_patterns as usize * PATTERN_ROWS * self.channels as usize * 4,
        }
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

    // SoundTracker 2.6 / IceTracker dispatch. These files place their
    // magic at +1464 and hold track-index bytes at +1080 where the
    // 31-sample family keeps its signature, so the tag check below
    // would reject them. A recognised +1080 tag always wins (the
    // catalogue is the primary discriminator); only when the tag is
    // unknown do we look for the ST2.6 magic.
    {
        let mut sig = [0u8; 4];
        sig.copy_from_slice(&bytes[1080..1084]);
        if !is_known_signature(&sig) && is_st26_magic(bytes) {
            return parse_st26_header(bytes);
        }
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

    let n_patterns = derive_n_patterns(&order);

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

/// Parse the SoundTracker 2.6 / IceTracker header layout
/// (`Soundtracker-v2.6-IceTracker-st26.txt`).
///
/// Layout: title (20 bytes), then 31 × 30-byte instruments (the
/// finetune byte is "Unused (finetune not available in ST2.6)" and is
/// forced to 0 here), pattern-list size (+950), number of stored tracks
/// (+951), the 128×4 track-index table (+952), the magic `MTN\0`/`IT10`
/// (+1464), track data (+1468), and finally sample data. Event cells
/// and sample bodies are "stored like Protracker".
///
/// Normalisation: the pattern list is exposed as an identity order
/// table (`order[i] = i` for the live window), one synthesized logical
/// pattern per list position; `player::parse_patterns` assembles each
/// logical pattern from the four tracks its list entry names. The
/// order-flow effects (`Bxx`/`Dxy` — both in the ST2.6 effect table)
/// therefore run unchanged against the list positions.
pub fn parse_st26_header(bytes: &[u8]) -> Result<ModHeader> {
    if bytes.len() < ST26_TRACK_DATA_OFFSET {
        return Err(Error::NeedMore);
    }
    if !is_st26_magic(bytes) {
        return Err(Error::invalid("MOD: not a SoundTracker 2.6 module"));
    }
    let title = read_padded_ascii(&bytes[0..20]);

    let mut samples = Vec::with_capacity(SAMPLE_COUNT);
    for i in 0..SAMPLE_COUNT {
        let off = 20 + i * 30;
        let name = read_padded_ascii(&bytes[off..off + 22]);
        let len_words = u16::from_be_bytes([bytes[off + 22], bytes[off + 23]]) as u32;
        let volume = bytes[off + 25].min(64);
        let repeat_start_words = u16::from_be_bytes([bytes[off + 26], bytes[off + 27]]) as u32;
        let repeat_length_words = u16::from_be_bytes([bytes[off + 28], bytes[off + 29]]) as u32;
        samples.push(Sample {
            name,
            length: len_words.saturating_mul(2),
            // "+ 0024 1 Unused (finetune not available in ST2.6)".
            finetune: 0,
            volume,
            repeat_start: repeat_start_words.saturating_mul(2),
            repeat_length: repeat_length_words.saturating_mul(2),
        });
    }

    // "0950 1 Size of the pattern list" — the playable position count,
    // bounded by the 128-entry table.
    let song_length = bytes[950].min(ORDER_TABLE_SIZE as u8);
    let n_tracks = bytes[951];

    // Identity order table: list position i plays synthesized logical
    // pattern i. Residue past the live window is zero-filled (there is
    // no on-disk order table to carry residue — the list itself is the
    // 4-wide track-index table).
    let order: Vec<u8> = (0..ORDER_TABLE_SIZE)
        .map(|i| if (i as u8) < song_length { i as u8 } else { 0 })
        .collect();
    let n_patterns = derive_n_patterns(&order);

    let mut signature = [0u8; 4];
    signature.copy_from_slice(&bytes[ST26_MAGIC_OFFSET..ST26_MAGIC_OFFSET + 4]);

    Ok(ModHeader {
        title,
        samples,
        song_length,
        // ST2.6 has no restart byte (+951 is the track count); surface
        // the conventional 0x7F filler.
        restart: 0x7F,
        order,
        signature,
        channels: 4,
        n_patterns,
        variant: ModVariant::SoundTracker26 { n_tracks },
    })
}

/// Stored-pattern count: highest pattern number anywhere in the 128-byte
/// order table, + 1.
///
/// The scan covers the ENTIRE table, not just the live `song_length`
/// window — three staged sources pin this: the
/// `FireLight-MOD-Player-Tutorial.txt` §2.5 loader pseudocode ("loop 128
/// times … the highest value found is stored as the number of patterns"),
/// the `Protracker-effects-MODFIL12.txt` §2.7 annotation ("Be sure to
/// scan ALL the values (128 of them) and to increment the highest pattern
/// nr once"), and `Ultimate-Soundtracker-mod.txt` ("pattern data (1024
/// bytes) for each pattern number that can be found in entire pattern
/// table"). Trackers do not clear the order-table residue past the song
/// length, so a pattern referenced only by residue is still physically
/// stored in the file — deriving the count from the live window alone
/// shifts the sample-data offset of every such module.
///
/// Hardening on top of the documented rule: the `+ 1` saturates in the
/// `u8` domain (a hostile 0xFF entry must not panic/wrap), and the result
/// is capped at 128 patterns per the `Protracker-effects-MODFIL12.txt`
/// §2.7 annotation "The nr of patterns is limited to 128 (from 0 to
/// 127)" — corrupt tables cannot demand more pattern data than the
/// format can address.
fn derive_n_patterns(order: &[u8]) -> u8 {
    order
        .iter()
        .max()
        .copied()
        .unwrap_or(0)
        .saturating_add(1)
        .min(128)
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

    // Same entire-table scan as the 31-sample path:
    // `Ultimate-Soundtracker-mod.txt` places "pattern data (1024 bytes)
    // for each pattern number that can be found in entire pattern table".
    let n_patterns = derive_n_patterns(&order);

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

/// Map the offset-1080 format tag to a channel count.
///
/// The full tag catalogue is documented in
/// `docs/audio/trackers/mod/multimedia-cx-protracker.html` (the "File
/// Format" tag list) and corroborated by
/// `docs/audio/trackers/mod/archiveteam-amiga-module.html`
/// ("File format tags"):
///
/// | Tag    | Channels | Origin                          |
/// | ------ | -------- | ------------------------------- |
/// | `M.K.` / `M!K!` / `M&K!` | 4 | ProTracker (`M!K!` = >64 patterns; `M&K!` a one-off variant tag) |
/// | `FLT4` / `FLT8` | 4 / 8 | Startrekker (`FLT8` paired)  |
/// | `OCTA` / `OKTA` / `CD81` | 8 | OctaMED / Oktalyzer / Falcon |
/// | `dCHN` | d | FastTracker (2/6/8) / TakeTracker (5/7/9) |
/// | `xxCH` / `xxCN` | xx (10..=32) | FastTracker (`CH`) / TakeTracker (`CN`) |
/// | `TDZx` | x (1/2/3) | TakeTracker                     |
///
/// `4CHN` is accepted as the explicit 4-channel spelling alongside the
/// canonical `M.K.`. The doc notes `xxCN` is the TakeTracker spelling of
/// the same 10+-channel layout `xxCH` carries; both decode identically.
fn channels_from_signature(sig: &[u8; 4]) -> Result<u8> {
    match sig {
        // `M&K!` is documented as "just a standard MOD, but with a weird
        // tag" — 4 channels, same layout as `M.K.`.
        b"M.K." | b"M!K!" | b"M&K!" | b"FLT4" => Ok(4),
        b"OCTA" | b"OKTA" | b"CD81" | b"FLT8" => Ok(8),
        // "dCHN" — a single ASCII digit followed by "CHN". FastTracker
        // emits 2/6/8; TakeTracker emits 5/7/9. We accept any 1..=9 the
        // doc's two producers can write (4CHN is the explicit 4-channel
        // spelling, also valid here).
        [d, b'C', b'H', b'N'] => match (*d as char).to_digit(10) {
            Some(n @ 1..=9) => Ok(n as u8),
            _ => unknown_signature(sig),
        },
        // "xxCH" / "xxCN" — two ASCII digits followed by "CH" (FastTracker)
        // or "CN" (TakeTracker), count in 10..=32. The two spellings name
        // the same layout, so they decode identically.
        [t, o, b'C', b'H'] | [t, o, b'C', b'N'] => {
            match ((*t as char).to_digit(10), (*o as char).to_digit(10)) {
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
                _ => unknown_signature(sig),
            }
        }
        // "TDZx" — TakeTracker 1/2/3-channel module (x is the channel
        // count as an ASCII digit).
        [b'T', b'D', b'Z', x] => match (*x as char).to_digit(10) {
            Some(n @ 1..=3) => Ok(n as u8),
            _ => unknown_signature(sig),
        },
        _ => unknown_signature(sig),
    }
}

/// True when `sig` is a recognised offset-1080 MOD format tag, i.e.
/// when [`channels_from_signature`] would resolve it to a channel count.
/// The container probe uses this so probe acceptance and parser
/// acceptance stay in lockstep — the tag catalogue lives in exactly one
/// place. Tags whose digits fall outside the documented 10..=32 range
/// (e.g. `99CH`) are rejected here too, matching the parser.
pub fn is_known_signature(sig: &[u8; 4]) -> bool {
    channels_from_signature(sig).is_ok()
}

fn unknown_signature(sig: &[u8; 4]) -> Result<u8> {
    Err(Error::invalid(format!(
        "MOD: unknown signature {:?}",
        std::str::from_utf8(sig).unwrap_or("????")
    )))
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

    /// Minimal SoundTracker 2.6 / IceTracker file: 3 pattern-list
    /// positions over 2 stored tracks, garbage in sample 1's unused
    /// finetune byte, `magic` at +1464.
    fn make_fake_st26(magic: &[u8; 4]) -> Vec<u8> {
        let mut out = vec![0u8; ST26_TRACK_DATA_OFFSET + 2 * ST26_TRACK_BYTES];
        out[0..4].copy_from_slice(b"st26");
        // Sample 1: unused finetune byte holds garbage the parser must
        // ignore ("+ 0024 1 Unused (finetune not available in ST2.6)").
        out[20 + 24] = 0x05;
        out[950] = 3; // pattern-list size
        out[951] = 2; // stored tracks
                      // Position 0 plays track 1 on channels 0/2, track 0 on 1/3.
        out[952..956].copy_from_slice(&[1, 0, 1, 0]);
        out[ST26_MAGIC_OFFSET..ST26_MAGIC_OFFSET + 4].copy_from_slice(magic);
        out
    }

    #[test]
    fn st26_header_parses_and_normalises() {
        // `Soundtracker-v2.6-IceTracker-st26.txt`: magic at +1464,
        // pattern-list size at +950, stored-track count at +951, track
        // data at +1468. The parser exposes an identity order table so
        // the standard order-flow engine walks list positions directly.
        let h = parse_header(&make_fake_st26(b"MTN\0")).unwrap();
        assert!(h.is_st26());
        assert!(!h.is_ust());
        assert!(!h.is_flt8());
        assert_eq!(h.channels, 4);
        assert_eq!(h.song_length, 3);
        assert_eq!(h.variant, ModVariant::SoundTracker26 { n_tracks: 2 });
        assert_eq!(h.signature, *b"MTN\0");
        assert_eq!(&h.order[..4], &[0, 1, 2, 0], "identity live window");
        assert_eq!(h.n_patterns, 3, "one logical pattern per list position");
        assert_eq!(h.pattern_data_offset(), ST26_TRACK_DATA_OFFSET);
        assert_eq!(h.pattern_data_size(), 2 * ST26_TRACK_BYTES);
        assert_eq!(
            h.sample_data_offset(),
            ST26_TRACK_DATA_OFFSET + 2 * ST26_TRACK_BYTES,
            "sample bodies follow the stored tracks, not n_patterns * 1024"
        );
        assert_eq!(
            h.samples[0].finetune, 0,
            "the unused ST2.6 finetune byte must be ignored"
        );
        assert_eq!(h.restart, 0x7F);
    }

    #[test]
    fn st26_it10_magic_dispatches_like_mtn() {
        // IceTracker 1.0/1.1 writes `IT10` at the same offset.
        let h = parse_header(&make_fake_st26(b"IT10")).unwrap();
        assert!(h.is_st26());
        assert_eq!(h.signature, *b"IT10");
    }

    #[test]
    fn st26_known_1080_tag_wins_over_st26_magic() {
        // A recognised +1080 signature is the primary discriminator; a
        // coincidental `MTN\0` inside a standard module's pattern data
        // must not reroute the parse.
        let mut bytes = make_fake_mod(b"M.K.", 1);
        bytes.resize(ST26_TRACK_DATA_OFFSET, 0);
        bytes[ST26_MAGIC_OFFSET..ST26_MAGIC_OFFSET + 4].copy_from_slice(b"MTN\0");
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.variant, ModVariant::Standard31);
    }

    #[test]
    fn st26_truncated_needs_more() {
        let bytes = make_fake_st26(b"MTN\0");
        assert!(matches!(
            parse_st26_header(&bytes[..ST26_TRACK_DATA_OFFSET - 1]),
            Err(Error::NeedMore)
        ));
    }

    #[test]
    fn n_patterns_scans_entire_order_table_not_just_live_window() {
        // `FireLight-MOD-Player-Tutorial.txt` §2.5 ("loop 128 times …
        // the highest value found is stored as the number of patterns"),
        // `Protracker-effects-MODFIL12.txt` §2.7 annotation ("Be sure to
        // scan ALL the values (128 of them)"), and
        // `Ultimate-Soundtracker-mod.txt` ("for each pattern number that
        // can be found in entire pattern table"): editing residue past
        // the song length still names physically stored patterns, so
        // the stored-pattern count — and with it the sample-data offset
        // — must come from the whole 128-byte table.
        let mut bytes = make_fake_mod(b"M.K.", 2);
        bytes[952] = 3; // live window: patterns 3, 1
        bytes[953] = 1;
        bytes[954] = 6; // residue: highest pattern number in the table
        bytes[955] = 5;
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.song_length, 2);
        assert_eq!(
            h.n_patterns, 7,
            "residue entry 6 must raise the stored-pattern count to 7; \
             a live-window-only scan would report 4 and misplace the \
             sample data of every residue-bearing module"
        );
        assert_eq!(h.pattern_data_size(), 7 * 0x400);

        let h = parse_ust_header(&{
            let mut b = make_fake_ust(1);
            b[UST_ORDER_TABLE_OFFSET + 5] = 2; // residue on the UST path
            b
        })
        .unwrap();
        assert_eq!(h.n_patterns, 3);
    }

    #[test]
    fn n_patterns_capped_at_128() {
        // `Protracker-effects-MODFIL12.txt` §2.7 annotation: "The nr of
        // patterns is limited to 128 (from 0 to 127)." A corrupt table
        // whose entries run to 0xFF must not demand 256 patterns of
        // data — the count clamps to the format's addressable maximum.
        let mut bytes = make_fake_mod(b"M.K.", 1);
        for i in 0..128 {
            bytes[952 + i] = 0xFF;
        }
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.n_patterns, 128);
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
    fn signature_2chn_fasttracker() {
        // multimedia-cx-protracker.html: "2CHN — a 2 channel MOD.
        // This is handled by FastTracker."
        let h = parse_header(&make_fake_mod(b"2CHN", 1)).unwrap();
        assert_eq!(h.channels, 2);
    }

    #[test]
    fn signature_4chn_explicit_four_channel() {
        // The explicit 4-channel spelling resolves the same as M.K.
        let h = parse_header(&make_fake_mod(b"4CHN", 1)).unwrap();
        assert_eq!(h.channels, 4);
    }

    #[test]
    fn signature_taketracker_odd_chn() {
        // multimedia-cx-protracker.html: "5CHN, 7CHN, 9CHN — allegedly
        // this is a TakeTracker extension for 5, 7, and 9 channels."
        for (tag, n) in [(b"5CHN", 5u8), (b"7CHN", 7), (b"9CHN", 9)] {
            let h = parse_header(&make_fake_mod(tag, 1)).unwrap();
            assert_eq!(h.channels, n, "tag {:?}", std::str::from_utf8(tag));
        }
    }

    #[test]
    fn signature_taketracker_tdz() {
        // multimedia-cx-protracker.html: "TDZ1, TDZ2, TDZ3 — allegedly
        // this is a TakeTracker extension for 1, 2, and 3 channels."
        for (tag, n) in [(b"TDZ1", 1u8), (b"TDZ2", 2), (b"TDZ3", 3)] {
            let h = parse_header(&make_fake_mod(tag, 1)).unwrap();
            assert_eq!(h.channels, n, "tag {:?}", std::str::from_utf8(tag));
        }
    }

    #[test]
    fn signature_taketracker_xxcn_equals_xxch() {
        // multimedia-cx-protracker.html: "xxCN — another 10+ channel MOD
        // … Allegedly TakeTracker writes these." Same layout as xxCH.
        let h = parse_header(&make_fake_mod(b"16CN", 1)).unwrap();
        assert_eq!(h.channels, 16);
        let h = parse_header(&make_fake_mod(b"32CN", 1)).unwrap();
        assert_eq!(h.channels, 32);
    }

    #[test]
    fn signature_okta_and_mandk_variants() {
        // OKTA = 8-channel (Oktalyzer); M&K! = 4-channel ("just a
        // standard MOD, but with a weird tag") — both per
        // multimedia-cx-protracker.html.
        assert_eq!(
            parse_header(&make_fake_mod(b"OKTA", 1)).unwrap().channels,
            8
        );
        assert_eq!(
            parse_header(&make_fake_mod(b"M&K!", 1)).unwrap().channels,
            4
        );
    }

    #[test]
    fn rejects_unknown_signature() {
        let bytes = make_fake_mod(b"XXXX", 1);
        assert!(parse_header(&bytes).is_err());
    }

    #[test]
    fn rejects_out_of_range_channel_counts() {
        // Single-digit 0CHN and the documented upper bound +1 must both
        // be rejected: 0 channels is degenerate, 33+ exceeds the 32-cap.
        assert!(parse_header(&make_fake_mod(b"0CHN", 1)).is_err());
        assert!(parse_header(&make_fake_mod(b"99CH", 1)).is_err());
        assert!(parse_header(&make_fake_mod(b"TDZ4", 1)).is_err());
    }

    #[test]
    fn is_known_signature_matches_parser() {
        // The probe helper must accept exactly the tags the parser
        // resolves — no drift between probe and parse.
        for ok in [
            b"M.K.", b"M!K!", b"M&K!", b"FLT4", b"FLT8", b"OCTA", b"OKTA", b"CD81", b"4CHN",
            b"2CHN", b"6CHN", b"8CHN", b"5CHN", b"7CHN", b"9CHN", b"16CH", b"32CN", b"TDZ1",
        ] {
            assert!(
                is_known_signature(ok),
                "expected known: {:?}",
                std::str::from_utf8(ok)
            );
        }
        for bad in [b"XXXX", b"0CHN", b"99CH", b"TDZ4", b"M.K!"] {
            assert!(
                !is_known_signature(bad),
                "expected unknown: {:?}",
                std::str::from_utf8(bad)
            );
        }
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
