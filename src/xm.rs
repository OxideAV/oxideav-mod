//! FastTracker 2 Extended Module ("XM") structural parser.
//!
//! XM is a descendant of the Amiga MOD / ScreamTracker lineage, introduced
//! with FastTracker 2 in 1994. Compared to MOD / STM it adds:
//!
//! - Up to 32 channels (even counts, 2..=32).
//! - A 256-entry pattern order table with a "restart position".
//! - Up to 256 patterns, 1..=256 rows each, with variable-length
//!   bit-packed cells (note / instrument / volume / effect / effect param
//!   columns are individually optional).
//! - Up to 128 instruments, each with a volume envelope (up to 12 points),
//!   panning envelope (up to 12 points), per-note sample mapping (96
//!   entries), vibrato table, fadeout, and multiple samples (sample
//!   header size 0x28) whose data is stored as 8- or 16-bit delta values.
//! - Two frequency tables (Amiga and Linear), selectable per-file.
//!
//! Layout summary, derived from
//! `docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt` and corrected
//! against `FastTracker-2-xm-alt.txt`:
//!
//! ```text
//! Offset  Len  Field
//! -------------------
//!   0     17   ASCII banner "Extended Module: " (note: capital M, trailing colon+space)
//!  17     20   Module name (space / zero padded)
//!  37      1   0x1A (tracker DOS-EOF marker)
//!  38     20   Tracker name
//!  58      2   Version (LE, typically 0x0104)
//!  60      4   Header size (usually 0x114 including these 4 bytes' worth of
//!              "size field" starting at offset 60+4=64, i.e. the size
//!              includes the song_length..order[0..256] run)
//!  64      2   Song length (in pattern order table, 1..256)
//!  66      2   Restart position
//!  68      2   Number of channels (2..=32, always even)
//!  70      2   Number of patterns (max 256)
//!  72      2   Number of instruments (max 128)
//!  74      2   Flags: bit 0 = 0 Amiga / 1 Linear frequency table
//!  76      2   Default tempo (speed, ticks/row)
//!  78      2   Default BPM
//!  80    256   Pattern order table
//! 336 = 60 + 0x114  (first pattern header begins here for a standard file)
//! ```
//!
//! The structural parser here mirrors the STM parser's policy:
//! spec-derived offsets + length-tolerant readers (short packets =
//! `Error::NeedMore`, malformed marker bytes = `Error::invalid`). No
//! playback; callers that want to render XM music can walk the
//! [`XmHeader`] / [`XmPattern`] / [`XmInstrument`] outputs themselves.

use oxideav_core::{Error, Result};

/// XM magic banner. Note the capital `M` and trailing `": "` —
/// FT2 rejects lowercase `"Extended module: "` as corrupt.
pub const XM_BANNER: &[u8; 17] = b"Extended Module: ";

/// Offset of the `0x1A` tracker-DOS-EOF marker.
pub const XM_ID_BYTE_OFFSET: usize = 37;

/// Expected version word at offset 58 (little-endian).
pub const XM_VERSION_0104: u16 = 0x0104;

/// Offset of the 4-byte `header_size` dword.
pub const XM_HEADER_SIZE_OFFSET: usize = 60;

/// Offset of the 256-byte pattern order table. The header size dword at
/// offset 60 spans from offset 64 through to the end of the order table
/// (see `FastTracker-2-xm-alt.txt`: the typical value is 0x114 =
/// 2+2+2+2+2+2+2+2+256 = 276).
pub const XM_ORDER_TABLE_OFFSET: usize = 80;

/// Size of the XM pattern order table.
pub const XM_ORDER_TABLE_SIZE: usize = 256;

/// Minimum plausible file size: banner + header prefix + order table.
pub const XM_MIN_HEADER_LEN: usize = XM_ORDER_TABLE_OFFSET + XM_ORDER_TABLE_SIZE;

/// Pattern header length field: always 9 bytes including the 4-byte
/// length field itself (per `FastTracker-2-xm-alt.txt`). The 9 bytes
/// decompose as: 4 length + 1 packing + 2 rows + 2 packed-size.
pub const XM_PATTERN_HEADER_SIZE: u32 = 9;

/// Standard sample header size (0x28 = 40 bytes, per the alt doc).
pub const XM_SAMPLE_HEADER_SIZE: u32 = 0x28;

/// Standard instrument header size when the instrument carries at least
/// one sample (0x107 per the alt doc). Instruments with zero samples
/// write an instrument-size of 0x21 (29 + 4 of the size field).
pub const XM_INSTRUMENT_HEADER_SIZE_WITH_SAMPLES: u32 = 0x107;

/// Frequency-table flag bit.
pub const XM_FLAG_LINEAR_FREQ_TABLE: u16 = 0x0001;

/// Frequency table selection (XM header `flags` bit 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XmFrequencyTable {
    /// Bit 0 = 0: Amiga-style period table.
    Amiga,
    /// Bit 0 = 1: Linear period table.
    Linear,
}

/// Top-level XM header fields (everything reachable without walking
/// patterns / instruments).
#[derive(Clone, Debug)]
pub struct XmHeader {
    pub module_name: String,
    pub tracker_name: String,
    /// Raw version word; typically `0x0104`.
    pub version: u16,
    /// Header size field (bytes; counts from offset 64 onwards).
    pub header_size: u32,
    pub song_length: u16,
    pub restart_position: u16,
    pub num_channels: u16,
    pub num_patterns: u16,
    pub num_instruments: u16,
    pub flags: u16,
    pub frequency_table: XmFrequencyTable,
    pub default_tempo: u16,
    pub default_bpm: u16,
    /// 256-entry order table (full width even if `song_length < 256`).
    pub order: Vec<u8>,
}

/// One pattern header + its decoded row data.
#[derive(Clone, Debug)]
pub struct XmPattern {
    pub header_length: u32,
    /// Packing type; the spec says "always 0".
    pub packing_type: u8,
    pub num_rows: u16,
    pub packed_size: u16,
    /// Decoded rows: `rows[row][channel]` = one cell.
    pub rows: Vec<Vec<XmCell>>,
}

/// A decoded pattern cell. The raw XM packing collapses an all-empty
/// cell into a single zero byte; we unpack to this wide representation
/// so downstream consumers don't need to re-do the bit math.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct XmCell {
    /// Note: 1..=96 = C-0..B-7, 97 = KeyOff, 0 = empty.
    pub note: u8,
    /// Instrument index 1..=128; 0 = no instrument.
    pub instrument: u8,
    /// Raw volume column byte (see [`XmVolume`]).
    pub volume: u8,
    /// Standard effect type byte (0..0x22, or letter-coded G..Z).
    pub effect_type: u8,
    /// Standard effect parameter byte.
    pub effect_param: u8,
}

/// Semantic interpretation of the volume-column byte.
///
/// The raw byte's high nibble selects the effect; the low nibble is the
/// parameter (except for the "set volume" range, where the byte minus
/// 0x10 is the 0..=0x40 target volume). Values outside 0x10..=0xFF are
/// treated as "do nothing".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XmVolume {
    Empty,
    /// 0x10..=0x50 → volume 0..=0x40.
    SetVolume(u8),
    /// 0x60..=0x6F.
    VolumeSlideDown(u8),
    /// 0x70..=0x7F.
    VolumeSlideUp(u8),
    /// 0x80..=0x8F.
    FineVolumeSlideDown(u8),
    /// 0x90..=0x9F.
    FineVolumeSlideUp(u8),
    /// 0xA0..=0xAF.
    SetVibratoSpeed(u8),
    /// 0xB0..=0xBF.
    Vibrato(u8),
    /// 0xC0..=0xCF.
    SetPanning(u8),
    /// 0xD0..=0xDF.
    PanningSlideLeft(u8),
    /// 0xE0..=0xEF.
    PanningSlideRight(u8),
    /// 0xF0..=0xFF.
    TonePorta(u8),
}

impl XmCell {
    /// Decode the volume-column byte into its semantic form.
    pub fn volume_kind(&self) -> XmVolume {
        match self.volume {
            0 => XmVolume::Empty,
            v @ 0x10..=0x50 => XmVolume::SetVolume(v - 0x10),
            v @ 0x60..=0x6F => XmVolume::VolumeSlideDown(v & 0x0F),
            v @ 0x70..=0x7F => XmVolume::VolumeSlideUp(v & 0x0F),
            v @ 0x80..=0x8F => XmVolume::FineVolumeSlideDown(v & 0x0F),
            v @ 0x90..=0x9F => XmVolume::FineVolumeSlideUp(v & 0x0F),
            v @ 0xA0..=0xAF => XmVolume::SetVibratoSpeed(v & 0x0F),
            v @ 0xB0..=0xBF => XmVolume::Vibrato(v & 0x0F),
            v @ 0xC0..=0xCF => XmVolume::SetPanning(v & 0x0F),
            v @ 0xD0..=0xDF => XmVolume::PanningSlideLeft(v & 0x0F),
            v @ 0xE0..=0xEF => XmVolume::PanningSlideRight(v & 0x0F),
            v @ 0xF0..=0xFF => XmVolume::TonePorta(v & 0x0F),
            _ => XmVolume::Empty,
        }
    }

    /// True if this cell carries an XM "note off" (raw note 97).
    pub fn is_note_off(&self) -> bool {
        self.note == 97
    }

    /// True if a musical note is present (1..=96).
    pub fn has_note(&self) -> bool {
        (1..=96).contains(&self.note)
    }
}

/// Envelope shape — up to 12 points, each `(x_tick, y_value 0..=64)`.
#[derive(Clone, Debug, Default)]
pub struct XmEnvelope {
    pub points: Vec<(u16, u16)>,
    pub sustain_point: u8,
    pub loop_start_point: u8,
    pub loop_end_point: u8,
    /// Envelope type bit-field: 0: On, 1: Sustain, 2: Loop.
    pub type_bits: u8,
}

impl XmEnvelope {
    pub fn is_on(&self) -> bool {
        self.type_bits & 0x01 != 0
    }
    pub fn has_sustain(&self) -> bool {
        self.type_bits & 0x02 != 0
    }
    pub fn has_loop(&self) -> bool {
        self.type_bits & 0x04 != 0
    }
}

/// Loop mode encoded in a sample header's `type` byte.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum XmSampleLoopMode {
    #[default]
    None,
    Forward,
    PingPong,
}

impl XmSampleLoopMode {
    pub fn from_type_byte(b: u8) -> Self {
        match b & 0x03 {
            1 => XmSampleLoopMode::Forward,
            2 => XmSampleLoopMode::PingPong,
            _ => XmSampleLoopMode::None,
        }
    }
}

impl crate::mixer::SampleSource for XmSampleHeader {
    fn len(&self) -> usize {
        if self.is_16_bit {
            self.pcm16.len()
        } else {
            self.pcm8.len()
        }
    }
    fn loop_start(&self) -> usize {
        // loop_start/loop_length are byte offsets in the XM file; for
        // 16-bit samples convert to frame indices.
        let div = if self.is_16_bit { 2 } else { 1 };
        if matches!(self.loop_mode, XmSampleLoopMode::None) {
            0
        } else {
            (self.loop_start as usize / div).min(self.len())
        }
    }
    fn loop_end(&self) -> usize {
        let div = if self.is_16_bit { 2 } else { 1 };
        if matches!(self.loop_mode, XmSampleLoopMode::None) {
            self.len()
        } else {
            let end = (self.loop_start + self.loop_length) as usize / div;
            end.min(self.len())
        }
    }
    fn loop_kind(&self) -> crate::mixer::LoopKind {
        match self.loop_mode {
            XmSampleLoopMode::None => crate::mixer::LoopKind::None,
            XmSampleLoopMode::Forward => crate::mixer::LoopKind::Forward,
            XmSampleLoopMode::PingPong => crate::mixer::LoopKind::PingPong,
        }
    }
    fn at(&self, idx: usize) -> f32 {
        if self.is_16_bit {
            self.pcm16.get(idx).copied().unwrap_or(0) as f32 / 32768.0
        } else {
            self.pcm8.get(idx).copied().unwrap_or(0) as f32 / 128.0
        }
    }
}

/// Decoded sample header (40 bytes + variable-length PCM body following
/// all sample headers of the instrument).
#[derive(Clone, Debug, Default)]
pub struct XmSampleHeader {
    pub name: String,
    /// Sample length **in bytes** (not frames). For 16-bit samples the
    /// frame count is `length / 2`.
    pub length: u32,
    pub loop_start: u32,
    pub loop_length: u32,
    pub volume: u8,
    /// Signed finetune -128..+127.
    pub finetune: i8,
    pub type_byte: u8,
    pub panning: u8,
    /// Signed relative-note, -96..+95 (0 => C-4 = C-4).
    pub relative_note: i8,
    pub loop_mode: XmSampleLoopMode,
    pub is_16_bit: bool,
    /// Decoded PCM samples. Empty until `extract_samples` is called.
    /// For 16-bit samples the values are `i16` halves, one per frame.
    pub pcm16: Vec<i16>,
    /// Decoded 8-bit PCM samples (if `!is_16_bit`). Empty otherwise.
    pub pcm8: Vec<i8>,
}

impl XmSampleHeader {
    /// True when the header declares a usable loop region.
    ///
    /// Bits 0-1 of the type byte encode the loop mode per the FT2
    /// sample-header field table in
    /// `docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt` +14
    /// ("Bit 0-1: 0 = No loop, 1 = Forward loop, 2 = Ping-pong
    /// loop"). The decoded [`Self::loop_mode`] captures that
    /// classification; this accessor is the canonical "is there a
    /// loop?" question, true for both forward and ping-pong modes
    /// and false for [`XmSampleLoopMode::None`].
    ///
    /// A `loop_length` of `0` is **not** a sentinel for "no loop"
    /// in XM — unlike MOD's `repeat_length == 2`, FT2 keys loop
    /// presence on the type byte alone, so a length-zero forward
    /// loop is still classified as looped here even though the
    /// mixer-side region degenerates to an empty span (the
    /// [`crate::mixer::SampleSource::loop_end`] impl above does the
    /// PCM-aware clamp). This separation matches the MOD
    /// `header::Sample::is_looped` / `samples::SampleBody` split
    /// — header-view answers what the file declared, mixer-view
    /// answers what's safe to read.
    pub fn is_looped(&self) -> bool {
        !matches!(self.loop_mode, XmSampleLoopMode::None)
    }

    /// Header-declared loop region as a half-open `[start, start +
    /// length)` **frame-index** pair, or `None` when
    /// [`Self::is_looped`] is `false`.
    ///
    /// FT2 stores `loop_start` / `loop_length` as **byte** offsets
    /// in the on-disk sample header
    /// (`docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt` +4 / +8);
    /// for a 16-bit sample, one frame is two bytes, so this
    /// accessor divides the on-disk byte counts by 2 when
    /// [`Self::is_16_bit`] is set. The returned values are therefore
    /// directly comparable to a frame-indexed cursor (the same view
    /// the `SampleSource` impl uses), without leaking the byte-vs-
    /// frame ambiguity into the caller. Loop fields for an 8-bit
    /// sample pass through unchanged.
    ///
    /// Like MOD's [`crate::header::Sample::loop_region`], this is
    /// the **raw header view** — there is no clamp against the
    /// extracted PCM body length. The mixer's `SampleSource::
    /// loop_end` impl owns the `.min(self.len())` clamp because a
    /// truncated rip can have a body shorter than the declared
    /// `length`. Use this accessor for metadata / diagnostics /
    /// instrument UIs that want what the file authored; consult
    /// the trait for what the mixer will actually read.
    pub fn loop_region_frames(&self) -> Option<(u32, u32)> {
        if self.is_looped() {
            let div = if self.is_16_bit { 2 } else { 1 };
            Some((self.loop_start / div, self.loop_length / div))
        } else {
            None
        }
    }

    /// Frame-count view of the sample length.
    ///
    /// `length` is stored in **bytes** per the on-disk field
    /// (`docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt` +0 of
    /// the sample header — "Sample length"); for 16-bit samples
    /// the frame count is `length / 2`. This accessor folds the
    /// `is_16_bit` divisor into one canonical surface so callers
    /// reasoning in frame indices don't have to repeat the
    /// conditional. Pairs with [`Self::loop_region_frames`] —
    /// both views are frame-indexed.
    pub fn length_frames(&self) -> u32 {
        if self.is_16_bit {
            self.length / 2
        } else {
            self.length
        }
    }

    /// Typed view on the signed-byte `finetune` field as a fractional
    /// semitone offset.
    ///
    /// The on-disk field at +13 of the sample header is a signed byte
    /// (`docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt` +13 —
    /// "Finetune (signed byte)"). The Linear-mode period formula in
    /// the same document
    /// (`Period = 10*12*16*4 - Note*16*4 - FineTune/2`) makes one
    /// semitone worth 64 period units and one finetune unit worth
    /// half a period unit, so the relation simplifies to **128
    /// finetune units = 1 semitone**. This accessor folds the divide
    /// into one canonical surface so a UI / analysis caller doesn't
    /// have to re-derive the scale from the period formula.
    ///
    /// Returned values span `-1.0` (at finetune `-128`) up to just
    /// under `+0.9921875` (at finetune `+127`). A value of `0.0`
    /// means "no finetune — play this sample at its tabulated period
    /// exactly". The result composes additively with
    /// [`Self::transpose_semitones`] / [`Self::relative_note`] so the
    /// total pitch offset relative to the cell's `C-4` is
    /// `relative_note + finetune_semitones()`.
    ///
    /// Notes on doc wording: the spec's text labels the field as
    /// "signed byte -16..+15" — that's the visible range in the FT2
    /// editor UI (one tick on the +/-16 dial = 8 on the on-disk byte),
    /// not the on-disk range. The mod XM-spec aggregator
    /// `docs/audio/trackers/xm/multimedia-cx-fasttracker-2.html` +213
    /// repeats the UI wording verbatim; the period formula on +96 of
    /// `FastTracker-2-v2.04-xm.txt` is the load-bearing definition
    /// and confirms the -128..+127 byte range. Same pattern as
    /// MOD's E5x ±8 nibble vs. the 16-entry period table indexing.
    pub fn finetune_semitones(&self) -> f32 {
        self.finetune as f32 / 128.0
    }

    /// Total pitch-offset of this sample relative to the
    /// `relative_note = 0, finetune = 0` baseline, in fractional
    /// semitones.
    ///
    /// FT2 splits the pitch transposition for a sample across two
    /// fields:
    /// - `relative_note` (signed byte at +16 of the sample header,
    ///   `docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt` +16) is
    ///   the **integer-semitone** offset added to the cell's note;
    ///   `0` means the cell's note plays the sample at its tabulated
    ///   period exactly.
    /// - `finetune` is the fractional-semitone offset captured by
    ///   [`Self::finetune_semitones`] above.
    ///
    /// This accessor sums the two into the single offset a tuning UI
    /// or transcription tool wants — i.e. "how many semitones above
    /// the cell's note does this sample sound?". The xm_player
    /// engine keeps the two fields separate because the period
    /// formula consumes them at different scales (16 sub-units per
    /// semitone for the `* 16 * 4` note term, 2 sub-units per period
    /// for the `/ 2` finetune term — see `note_to_period` in
    /// `xm_player.rs`), but a single floating-point offset is the
    /// canonical surface for header-side metadata callers.
    pub fn transpose_semitones(&self) -> f32 {
        self.relative_note as f32 + self.finetune_semitones()
    }
}

/// Decoded instrument: header + per-note sample mapping + envelopes +
/// sample headers. Sample PCM bodies are attached by
/// [`extract_sample_bodies`].
#[derive(Clone, Debug, Default)]
pub struct XmInstrument {
    pub name: String,
    /// Size of the instrument header block this record came from
    /// (raw bytes, includes the 4-byte size field).
    pub header_size: u32,
    /// Raw instrument-type byte (spec says "always 0").
    pub instrument_type: u8,
    pub num_samples: u16,
    /// Sample-header size dword from the instrument's extended header
    /// (typically 0x28); absent when `num_samples == 0`. Surfaced for
    /// metadata fidelity only — the parser strides sample headers at
    /// the fixed on-disk 40 bytes, because FT2 ignores this field and
    /// modules in the wild carry broken values in it (per
    /// `multimedia-cx-fasttracker-2.html` §File Format).
    pub sample_header_size: u32,
    /// `sample_map[note]` = which of this instrument's samples plays for
    /// `note` (0..=95). Only populated when `num_samples > 0`.
    pub sample_map: Vec<u8>,
    pub volume_envelope: XmEnvelope,
    pub panning_envelope: XmEnvelope,
    pub vibrato_type: u8,
    pub vibrato_sweep: u8,
    pub vibrato_depth: u8,
    pub vibrato_rate: u8,
    pub volume_fadeout: u16,
    pub samples: Vec<XmSampleHeader>,
    /// Byte offset in the source blob where this instrument's sample
    /// PCM bodies start (sum of all preceding sample-header sizes).
    pub sample_data_offset: usize,
}

// ---------- probe + header ----------

/// Test whether `bytes` starts with the 17-byte XM banner. Matches the
/// exact, case-sensitive `"Extended Module: "` string that FT2 requires.
pub fn is_xm(bytes: &[u8]) -> bool {
    bytes.len() >= XM_BANNER.len() && &bytes[..XM_BANNER.len()] == XM_BANNER.as_slice()
}

fn read_u16_le(bytes: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([bytes[off], bytes[off + 1]])
}

fn read_u32_le(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

fn trim_fixed_string(bytes: &[u8]) -> String {
    // XM writes zero-padded or space-padded fixed-width strings. Strip
    // trailing NULs and trailing whitespace; don't try to strip internal
    // control characters since some demoscene tracks put ANSI art there.
    let end = bytes
        .iter()
        .rposition(|&b| b != 0 && b != b' ')
        .map(|i| i + 1)
        .unwrap_or(0);
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

/// Parse the XM file header + order table. Does not parse patterns or
/// instruments — those follow `XM_HEADER_SIZE_OFFSET + 4 + header_size`
/// in the source blob; see [`parse_patterns`] / [`parse_instruments`].
///
/// Returns `Error::NeedMore` when the buffer can't reach the order
/// table, and `Error::invalid` when the banner / id byte / channel
/// count / instrument count look structurally wrong.
pub fn parse_header(bytes: &[u8]) -> Result<XmHeader> {
    if bytes.len() < XM_MIN_HEADER_LEN {
        return Err(Error::NeedMore);
    }
    if !is_xm(bytes) {
        return Err(Error::invalid(
            "XM: missing 'Extended Module: ' banner at offset 0",
        ));
    }
    if bytes[XM_ID_BYTE_OFFSET] != 0x1A {
        return Err(Error::invalid("XM: missing 0x1A marker byte at offset 37"));
    }

    let module_name = trim_fixed_string(&bytes[17..37]);
    let tracker_name = trim_fixed_string(&bytes[38..58]);
    let version = read_u16_le(bytes, 58);
    let header_size = read_u32_le(bytes, XM_HEADER_SIZE_OFFSET);
    let song_length = read_u16_le(bytes, 64);
    let restart_position = read_u16_le(bytes, 66);
    let num_channels = read_u16_le(bytes, 68);
    let num_patterns = read_u16_le(bytes, 70);
    let num_instruments = read_u16_le(bytes, 72);
    let flags = read_u16_le(bytes, 74);
    let default_tempo = read_u16_le(bytes, 76);
    let default_bpm = read_u16_le(bytes, 78);

    if !(1..=32).contains(&num_channels) {
        return Err(Error::invalid(format!(
            "XM: implausible channel count {num_channels} (expected 1..=32)"
        )));
    }
    if num_patterns > 256 {
        return Err(Error::invalid(format!(
            "XM: implausible pattern count {num_patterns} (expected <=256)"
        )));
    }
    if num_instruments > 128 {
        return Err(Error::invalid(format!(
            "XM: implausible instrument count {num_instruments} (expected <=128)"
        )));
    }

    let frequency_table = if flags & XM_FLAG_LINEAR_FREQ_TABLE != 0 {
        XmFrequencyTable::Linear
    } else {
        XmFrequencyTable::Amiga
    };

    let order = bytes[XM_ORDER_TABLE_OFFSET..XM_ORDER_TABLE_OFFSET + XM_ORDER_TABLE_SIZE].to_vec();

    Ok(XmHeader {
        module_name,
        tracker_name,
        version,
        header_size,
        song_length,
        restart_position,
        num_channels,
        num_patterns,
        num_instruments,
        flags,
        frequency_table,
        default_tempo,
        default_bpm,
        order,
    })
}

/// Absolute byte offset in the source blob where pattern data begins,
/// i.e. just past the file header + order table.
///
/// The spec's `header_size` dword at offset 60 is measured from offset
/// 60 itself (inclusive of the size dword) — so the block spans
/// `[60 .. 60 + header_size)`. For a canonical file with
/// `header_size == 0x114`, this lands at offset `0x150 = 336`, exactly
/// at the end of the 256-byte order table that starts at offset 80.
pub fn pattern_data_offset(header: &XmHeader) -> usize {
    XM_HEADER_SIZE_OFFSET + header.header_size as usize
}

// ---------- pattern unpacking ----------

/// Decode one packed pattern cell starting at `cur` in `data`. Returns
/// `(cell, bytes_consumed)`. If `cur` is out of bounds, returns an
/// empty cell consuming zero bytes.
///
/// Packing rule (XM spec, reproduced here):
/// - If the high bit of the first byte is set, the other low 5 bits form
///   a mask selecting which of {note, instrument, volume, effect_type,
///   effect_param} are present. Missing fields are implicit zero.
/// - Otherwise the first byte **is** the note, and the remaining four
///   bytes (instrument, volume, effect_type, effect_param) follow in
///   order.
fn decode_packed_cell(data: &[u8], cur: usize) -> (XmCell, usize) {
    if cur >= data.len() {
        return (XmCell::default(), 0);
    }
    let first = data[cur];
    let mut cell = XmCell::default();
    if first & 0x80 != 0 {
        let mask = first & 0x7F;
        let mut off = 1usize;
        let grab = |off: &mut usize, data: &[u8]| -> u8 {
            let p = cur + *off;
            *off += 1;
            if p < data.len() {
                data[p]
            } else {
                0
            }
        };
        if mask & 0x01 != 0 {
            cell.note = grab(&mut off, data);
        }
        if mask & 0x02 != 0 {
            cell.instrument = grab(&mut off, data);
        }
        if mask & 0x04 != 0 {
            cell.volume = grab(&mut off, data);
        }
        if mask & 0x08 != 0 {
            cell.effect_type = grab(&mut off, data);
        }
        if mask & 0x10 != 0 {
            cell.effect_param = grab(&mut off, data);
        }
        (cell, off)
    } else {
        // Unpacked 5-byte form: first byte is the note itself.
        cell.note = first;
        let grab = |rel: usize| -> u8 {
            let p = cur + rel;
            if p < data.len() {
                data[p]
            } else {
                0
            }
        };
        cell.instrument = grab(1);
        cell.volume = grab(2);
        cell.effect_type = grab(3);
        cell.effect_param = grab(4);
        (cell, 5)
    }
}

/// Parse all patterns starting from the end of the file header.
///
/// For each pattern:
///  - Reads the 9-byte pattern header.
///  - If `packed_size == 0`, the pattern is "all empty" — emits
///    `num_rows` rows of `num_channels` default cells without advancing
///    into packed data.
///  - Otherwise, decodes `num_rows * num_channels` cells from the packed
///    block, tolerating truncation by emitting default cells for any
///    leftover slots.
///
/// Returns the decoded patterns and the absolute byte offset **after**
/// the last pattern (where the instrument table begins).
pub fn parse_patterns(header: &XmHeader, bytes: &[u8]) -> Result<(Vec<XmPattern>, usize)> {
    let mut cur = pattern_data_offset(header);
    let mut out = Vec::with_capacity(header.num_patterns as usize);
    let num_channels = header.num_channels as usize;

    for pat_idx in 0..header.num_patterns as usize {
        if cur + 9 > bytes.len() {
            return Err(Error::invalid(format!(
                "XM: truncated pattern header #{pat_idx} at offset {cur}"
            )));
        }
        let header_length = read_u32_le(bytes, cur);
        let packing_type = bytes[cur + 4];
        let num_rows = read_u16_le(bytes, cur + 5);
        let packed_size = read_u16_le(bytes, cur + 7);

        // Both spec texts bound the row count: "Number of rows in
        // pattern (1..256)" (`FastTracker-2-v2.04-xm.txt` pattern
        // header, `multimedia-cx-fasttracker-2.html` §File Format).
        // Rejecting out-of-range values also closes an allocation
        // bomb: the `packed_size == 0` branch below synthesizes
        // `num_rows * num_channels` default cells with NO file bytes
        // backing them, so an unclamped u16 row count times 256
        // patterns times 32 channels turns a ~2 KB input into
        // multi-GB of allocations (found as sustained RSS creep by
        // `oxideav-mod-fuzz/xm_decode`, round 451).
        if !(1..=256).contains(&num_rows) {
            return Err(Error::invalid(format!(
                "XM: implausible row count {num_rows} in pattern #{pat_idx} (expected 1..=256)"
            )));
        }

        // Skip over the pattern header *using its declared length*, so
        // nonstandard writers (that pad extra bytes) don't desync us.
        let data_start = cur
            .checked_add(header_length as usize)
            .ok_or_else(|| Error::invalid("XM: pattern header_length overflow"))?;

        let mut rows: Vec<Vec<XmCell>> = Vec::with_capacity(num_rows as usize);

        if packed_size == 0 {
            // All-empty pattern: synthesize default cells.
            for _ in 0..num_rows {
                rows.push(vec![XmCell::default(); num_channels]);
            }
        } else {
            // Clamp BOTH ends against `bytes.len()` — a hostile
            // `header_length` can push `data_start` past EOF, and the
            // bare `&bytes[data_start..data_end]` slice index would
            // panic with "slice index out of bounds" before the
            // `min(bytes.len())` on `data_end` had a chance to
            // matter. Caught by `oxideav-mod-fuzz/xm_decode`
            // (crash-212b2111).
            let start = data_start.min(bytes.len());
            let data_end = data_start
                .saturating_add(packed_size as usize)
                .min(bytes.len());
            let slice = &bytes[start..data_end];
            let mut inner = 0usize;
            for _ in 0..num_rows {
                let mut row = Vec::with_capacity(num_channels);
                for _ in 0..num_channels {
                    let (cell, consumed) = decode_packed_cell(slice, inner);
                    inner += consumed;
                    row.push(cell);
                }
                rows.push(row);
            }
        }

        // `data_start` already sits past the pattern header. When the
        // packed block is empty, `packed_size == 0` so `cur` lands right
        // after the 9-byte header — matching FT2's "empty patterns
        // still write a header" invariant.
        cur = data_start.saturating_add(packed_size as usize);

        out.push(XmPattern {
            header_length,
            packing_type,
            num_rows,
            packed_size,
            rows,
        });
    }

    Ok((out, cur))
}

// ---------- instruments ----------

/// Parse the envelope points buffer (48 bytes = 12 × `(x:u16, y:u16)`,
/// all little-endian). `num_points` selects how many entries are valid;
/// the remainder are ignored.
fn parse_envelope_points(bytes: &[u8; 48], num_points: u8) -> Vec<(u16, u16)> {
    let n = num_points.min(12) as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = i * 4;
        let x = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
        let y = u16::from_le_bytes([bytes[off + 2], bytes[off + 3]]);
        out.push((x, y));
    }
    out
}

/// Parse one instrument starting at `cur` in `bytes`. Returns the
/// decoded instrument and the byte offset **after** all of its sample
/// headers (i.e. where its sample PCM bodies begin).
fn parse_one_instrument(bytes: &[u8], cur: usize) -> Result<(XmInstrument, usize)> {
    if cur + 29 > bytes.len() {
        return Err(Error::invalid(format!(
            "XM: truncated instrument header at offset {cur}"
        )));
    }
    let header_size = read_u32_le(bytes, cur);
    if header_size < 29 {
        return Err(Error::invalid(format!(
            "XM: nonsensical instrument header_size {header_size} at {cur}"
        )));
    }
    let name = trim_fixed_string(&bytes[cur + 4..cur + 26]);
    let instrument_type = bytes[cur + 26];
    let num_samples = read_u16_le(bytes, cur + 27);

    let mut inst = XmInstrument {
        name,
        header_size,
        instrument_type,
        num_samples,
        ..Default::default()
    };

    if num_samples == 0 {
        // Instrument has no samples: the next instrument starts
        // immediately after this block of size `header_size`.
        let next = cur.saturating_add(header_size as usize).min(bytes.len());
        inst.sample_data_offset = next;
        return Ok((inst, next));
    }

    // Extended instrument block (starts at cur+29).
    if cur + header_size as usize > bytes.len() {
        return Err(Error::invalid(format!(
            "XM: truncated extended instrument block (need {} bytes at {cur})",
            header_size
        )));
    }
    let ext_base = cur + 29;
    // Sample header size lives at extended offset +0 (file offset
    // ext_base + 0 = cur+29). The `cur + header_size` guard above is
    // NOT sufficient here: with `num_samples > 0` a hostile
    // `header_size` of 29..=32 passes that check while the file ends
    // before `ext_base + 4`, and the bare dword read panics on the
    // slice index. Found by `oxideav-mod-fuzz/xm_decode`
    // (crash-bb1e627e, round 451).
    if ext_base + 4 > bytes.len() {
        return Err(Error::invalid(
            "XM: truncated instrument extended header (sample_header_size dword past EOF)",
        ));
    }
    inst.sample_header_size = read_u32_le(bytes, ext_base);

    // Sample map (96 bytes) at ext_base+4.
    let map_start = ext_base + 4;
    if map_start + 96 > bytes.len() {
        return Err(Error::invalid("XM: truncated instrument sample-number map"));
    }
    inst.sample_map = bytes[map_start..map_start + 96].to_vec();

    // Volume envelope points: 48 bytes at ext_base + 100.
    let vol_env_start = ext_base + 100;
    // Panning envelope points: 48 bytes at ext_base + 148.
    let pan_env_start = ext_base + 148;
    if pan_env_start + 48 > bytes.len() {
        return Err(Error::invalid("XM: truncated instrument envelope tables"));
    }
    let vol_env_raw: [u8; 48] = bytes[vol_env_start..vol_env_start + 48].try_into().unwrap();
    let pan_env_raw: [u8; 48] = bytes[pan_env_start..pan_env_start + 48].try_into().unwrap();

    // Byte fields starting at ext_base + 196.
    let f = ext_base + 196;
    if f + 16 > bytes.len() {
        return Err(Error::invalid("XM: truncated instrument fixed-byte block"));
    }
    let num_vol_points = bytes[f];
    let num_pan_points = bytes[f + 1];
    let vol_sustain = bytes[f + 2];
    let vol_loop_start = bytes[f + 3];
    let vol_loop_end = bytes[f + 4];
    let pan_sustain = bytes[f + 5];
    let pan_loop_start = bytes[f + 6];
    let pan_loop_end = bytes[f + 7];
    let vol_type = bytes[f + 8];
    let pan_type = bytes[f + 9];
    inst.vibrato_type = bytes[f + 10];
    inst.vibrato_sweep = bytes[f + 11];
    inst.vibrato_depth = bytes[f + 12];
    inst.vibrato_rate = bytes[f + 13];
    inst.volume_fadeout = read_u16_le(bytes, f + 14);
    // f+16..f+18 reserved.

    inst.volume_envelope = XmEnvelope {
        points: parse_envelope_points(&vol_env_raw, num_vol_points),
        sustain_point: vol_sustain,
        loop_start_point: vol_loop_start,
        loop_end_point: vol_loop_end,
        type_bits: vol_type,
    };
    inst.panning_envelope = XmEnvelope {
        points: parse_envelope_points(&pan_env_raw, num_pan_points),
        sustain_point: pan_sustain,
        loop_start_point: pan_loop_start,
        loop_end_point: pan_loop_end,
        type_bits: pan_type,
    };

    // Sample headers follow the instrument header block (`header_size`
    // bytes from `cur`). Each sample header occupies exactly 40 bytes
    // on disk regardless of what the stored `sample_header_size` dword
    // claims: per `multimedia-cx-fasttracker-2.html` §File Format,
    // "The sample header size field is completely ignored by Fast
    // Tracker 2 ... it's best to assume the sample header size is
    // always 40 bytes and to not skip this data!" — modules in the
    // wild carry completely broken values in the field, so honouring
    // it either mis-strides every subsequent header + PCM body (value
    // > 40) or used to reject the whole file (value < 40). The raw
    // dword is still surfaced on `inst.sample_header_size` for
    // metadata fidelity; it just never steers the parse.
    const SAMPLE_HEADER_DISK_SIZE: usize = 40;

    let headers_start = cur + header_size as usize;
    let mut hcur = headers_start;
    for i in 0..num_samples as usize {
        if hcur + SAMPLE_HEADER_DISK_SIZE > bytes.len() {
            return Err(Error::invalid(format!(
                "XM: truncated sample header #{i} in instrument"
            )));
        }
        let length = read_u32_le(bytes, hcur);
        let loop_start = read_u32_le(bytes, hcur + 4);
        let loop_length = read_u32_le(bytes, hcur + 8);
        let volume = bytes[hcur + 12];
        let finetune = bytes[hcur + 13] as i8;
        let type_byte = bytes[hcur + 14];
        let panning = bytes[hcur + 15];
        let relative_note = bytes[hcur + 16] as i8;
        // hcur + 17 reserved.
        let name = if hcur + 18 + 22 <= bytes.len() {
            trim_fixed_string(&bytes[hcur + 18..hcur + 40])
        } else {
            String::new()
        };
        let loop_mode = XmSampleLoopMode::from_type_byte(type_byte);
        let is_16_bit = type_byte & 0x10 != 0;
        inst.samples.push(XmSampleHeader {
            name,
            length,
            loop_start,
            loop_length,
            volume,
            finetune,
            type_byte,
            panning,
            relative_note,
            loop_mode,
            is_16_bit,
            pcm16: Vec::new(),
            pcm8: Vec::new(),
        });
        hcur += SAMPLE_HEADER_DISK_SIZE;
    }

    inst.sample_data_offset = hcur;
    Ok((inst, hcur))
}

/// Parse all instruments starting at `instruments_offset`, returning
/// the decoded instruments and the offset where the first sample body
/// would begin (i.e. end of the last instrument's sample headers).
///
/// This is the offset immediately after all instrument blocks' sample
/// **headers**; sample **bodies** are interleaved per-instrument, so the
/// separate per-instrument `sample_data_offset` fields are the useful
/// anchors for PCM extraction.
pub fn parse_instruments(
    header: &XmHeader,
    bytes: &[u8],
    instruments_offset: usize,
) -> Result<Vec<XmInstrument>> {
    let mut out = Vec::with_capacity(header.num_instruments as usize);
    let mut cur = instruments_offset;
    for i in 0..header.num_instruments as usize {
        let (inst, _next) = parse_one_instrument(bytes, cur)
            .map_err(|e| Error::invalid(format!("XM: failed to parse instrument #{i}: {e}")))?;
        // Advance to the byte just past all sample bodies of this
        // instrument. Sample headers live between `header_size`-end and
        // `inst.sample_data_offset`; PCM bodies of length
        // `sum(sample.length)` follow and belong to this instrument.
        let pcm_bytes: usize = inst.samples.iter().map(|s| s.length as usize).sum();
        cur = inst.sample_data_offset.saturating_add(pcm_bytes);
        out.push(inst);
    }
    Ok(out)
}

/// Decode all sample PCM bodies in place on `instruments`, reading
/// their delta-encoded PCM from `bytes` and converting to absolute
/// samples per the XM spec.
///
/// Mutates each `XmSampleHeader` by populating `pcm8` or `pcm16`. The
/// decoder is tolerant of truncation: if a sample body runs past end of
/// file, we decode as many frames as are available and stop cleanly.
pub fn extract_sample_bodies(instruments: &mut [XmInstrument], bytes: &[u8]) {
    for inst in instruments.iter_mut() {
        let mut cur = inst.sample_data_offset;
        for sample in inst.samples.iter_mut() {
            let length_bytes = (sample.length as usize).min(bytes.len().saturating_sub(cur));
            let slice = &bytes[cur..cur + length_bytes];
            if sample.is_16_bit {
                let n_frames = length_bytes / 2;
                let mut out = Vec::with_capacity(n_frames);
                let mut old: i16 = 0;
                for i in 0..n_frames {
                    let delta = i16::from_le_bytes([slice[i * 2], slice[i * 2 + 1]]);
                    old = old.wrapping_add(delta);
                    out.push(old);
                }
                sample.pcm16 = out;
            } else {
                let mut out = Vec::with_capacity(length_bytes);
                let mut old: i8 = 0;
                for &b in slice {
                    let delta = b as i8;
                    old = old.wrapping_add(delta);
                    out.push(old);
                }
                sample.pcm8 = out;
            }
            cur += length_bytes;
        }
    }
}

/// Rough upper-bound duration estimate in microseconds, loosely
/// analogous to the MOD / STM helpers. XM uses `tempo * ticks/row`
/// pacing and `bpm` scales the tick rate. Real songs use Fxx effects to
/// reshape tempo mid-song, so this is a coarse envelope only.
pub fn estimate_duration_micros(header: &XmHeader, patterns: &[XmPattern]) -> i64 {
    if patterns.is_empty() {
        return 0;
    }
    let bpm = header.default_bpm.max(1) as i64;
    let tempo = header.default_tempo.max(1) as i64;
    // 2.5 * bpm = ticks per second (classic Amiga/FT2 formula).
    let ticks_per_sec = (5 * bpm) / 2;
    if ticks_per_sec < 1 {
        return 0;
    }
    let song_length = header.song_length.max(1) as usize;
    let mut total_rows: u64 = 0;
    for idx in 0..song_length {
        let pat_idx = *header.order.get(idx).unwrap_or(&0) as usize;
        let rows = patterns
            .get(pat_idx)
            .map(|p| p.num_rows as u64)
            .unwrap_or(64);
        total_rows = total_rows.saturating_add(rows);
    }
    // microseconds = rows * tempo (ticks/row) * 1e6 / ticks/sec.
    (total_rows as i64).saturating_mul(tempo) * 1_000_000 / ticks_per_sec.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- builders for hand-constructed XM blobs ----

    /// Build a minimal 336-byte XM header with `num_patterns` patterns
    /// declared, `num_instruments` instruments, and the given channel
    /// count. Caller is expected to append pattern + instrument blocks
    /// as appropriate for the test.
    fn build_header(
        num_channels: u16,
        num_patterns: u16,
        num_instruments: u16,
        linear: bool,
    ) -> Vec<u8> {
        let mut out = vec![0u8; XM_MIN_HEADER_LEN];
        out[0..17].copy_from_slice(XM_BANNER);
        let name = b"hello xm          ";
        out[17..17 + name.len()].copy_from_slice(name);
        out[XM_ID_BYTE_OFFSET] = 0x1A;
        let tracker = b"oxideav-test        ";
        out[38..38 + tracker.len()].copy_from_slice(tracker);
        // version 0x0104
        out[58..60].copy_from_slice(&XM_VERSION_0104.to_le_bytes());
        // header_size 0x114
        out[60..64].copy_from_slice(&0x114u32.to_le_bytes());
        out[64..66].copy_from_slice(&1u16.to_le_bytes()); // song_length=1
        out[66..68].copy_from_slice(&0u16.to_le_bytes()); // restart
        out[68..70].copy_from_slice(&num_channels.to_le_bytes());
        out[70..72].copy_from_slice(&num_patterns.to_le_bytes());
        out[72..74].copy_from_slice(&num_instruments.to_le_bytes());
        let flags = if linear { 1u16 } else { 0u16 };
        out[74..76].copy_from_slice(&flags.to_le_bytes());
        out[76..78].copy_from_slice(&6u16.to_le_bytes()); // default tempo
        out[78..80].copy_from_slice(&125u16.to_le_bytes()); // default BPM
                                                            // order[0] = 0, rest stays 0 or we write 255 like STM does
        for i in 1..XM_ORDER_TABLE_SIZE {
            out[XM_ORDER_TABLE_OFFSET + i] = 0xFF;
        }
        out
    }

    /// Build a pattern block with a header followed by `packed` data.
    fn build_pattern_block(num_rows: u16, packed: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&XM_PATTERN_HEADER_SIZE.to_le_bytes()); // header length = 9
        out.push(0); // packing type = 0
        out.extend_from_slice(&num_rows.to_le_bytes());
        out.extend_from_slice(&(packed.len() as u16).to_le_bytes());
        out.extend_from_slice(packed);
        out
    }

    /// Build an instrument record with zero samples (header size 0x21 =
    /// 33 bytes: 4 size + 22 name + 1 type + 2 samples + 4 trailing
    /// padding to reach 0x21).
    fn build_empty_instrument(name: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        // Pick a no-samples header size that actually writes the 29 known
        // fields (4 size + 22 name + 1 type + 2 num_samples). Many
        // writers emit 0x21; we mirror that here to exercise the "skip
        // over unknown trailing bytes" path in the parser.
        const HSIZE: u32 = 0x21;
        out.extend_from_slice(&HSIZE.to_le_bytes());
        let mut nbuf = [0u8; 22];
        let n = name.len().min(22);
        nbuf[..n].copy_from_slice(&name[..n]);
        out.extend_from_slice(&nbuf);
        out.push(0); // instrument type
        out.extend_from_slice(&0u16.to_le_bytes()); // num_samples
        while out.len() < HSIZE as usize {
            out.push(0);
        }
        out
    }

    /// Build an instrument record with a single 8-bit sample of
    /// `sample_body` bytes (delta-encoded), using the standard 0x107
    /// instrument header size.
    fn build_one_sample_instrument(name: &[u8], sample_body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        const HSIZE: u32 = XM_INSTRUMENT_HEADER_SIZE_WITH_SAMPLES; // 0x107
        out.extend_from_slice(&HSIZE.to_le_bytes());
        let mut nbuf = [0u8; 22];
        let n = name.len().min(22);
        nbuf[..n].copy_from_slice(&name[..n]);
        out.extend_from_slice(&nbuf);
        out.push(0); // instrument type
        out.extend_from_slice(&1u16.to_le_bytes()); // num_samples = 1

        // Extended instrument block starts here (file-offset +29 from
        // the 4-byte size field's start).
        // +29: sample_header_size = 0x28
        out.extend_from_slice(&XM_SAMPLE_HEADER_SIZE.to_le_bytes());
        // +33..+129: sample_map (96 bytes) — all zeros (all notes → sample 0)
        out.extend(std::iter::repeat_n(0u8, 96));
        // +129..+177: volume envelope points (48 bytes) — one (0,0) + (64,64)
        let mut vol_env = [0u8; 48];
        vol_env[0..2].copy_from_slice(&0u16.to_le_bytes()); // x=0
        vol_env[2..4].copy_from_slice(&0u16.to_le_bytes()); // y=0
        vol_env[4..6].copy_from_slice(&64u16.to_le_bytes()); // x=64
        vol_env[6..8].copy_from_slice(&64u16.to_le_bytes()); // y=64
        out.extend_from_slice(&vol_env);
        // +177..+225: panning envelope points (48 bytes) — all zero
        out.extend_from_slice(&[0u8; 48]);
        // +225: num_vol_points = 2
        out.push(2);
        // +226: num_pan_points = 0
        out.push(0);
        // +227..+229: vol sustain/loop start/loop end
        out.push(0);
        out.push(0);
        out.push(0);
        // +230..+232: pan sustain/loop start/loop end
        out.push(0);
        out.push(0);
        out.push(0);
        // +233: vol type = On (bit 0)
        out.push(0x01);
        // +234: pan type
        out.push(0);
        // +235..+238: vibrato (type/sweep/depth/rate)
        out.push(0);
        out.push(0);
        out.push(0);
        out.push(0);
        // +239..+240: vol fadeout
        out.extend_from_slice(&512u16.to_le_bytes());
        // +241..+242: reserved
        out.extend_from_slice(&0u16.to_le_bytes());
        // Pad the extended block so total = HSIZE (0x107) bytes.
        while out.len() < HSIZE as usize {
            out.push(0);
        }

        // Sample header (0x28 = 40 bytes).
        out.extend_from_slice(&(sample_body.len() as u32).to_le_bytes()); // length
        out.extend_from_slice(&0u32.to_le_bytes()); // loop_start
        out.extend_from_slice(&0u32.to_le_bytes()); // loop_length
        out.push(0x40); // volume = 64
        out.push(0); // finetune
        out.push(0); // type byte — no loop, 8-bit
        out.push(128); // panning (center)
        out.push(0); // relative note (C-4 = C-4)
        out.push(0); // reserved
        let mut sname = [0u8; 22];
        let s = b"snd";
        sname[..s.len()].copy_from_slice(s);
        out.extend_from_slice(&sname);

        // Sample body.
        out.extend_from_slice(sample_body);
        out
    }

    #[test]
    fn is_xm_accepts_canonical_banner() {
        let bytes = build_header(4, 0, 0, false);
        assert!(is_xm(&bytes));
    }

    #[test]
    fn is_xm_rejects_lowercase_banner() {
        let mut bytes = build_header(4, 0, 0, false);
        // "module" (lowercase m) — matches the original (buggy) XM spec
        // text but FT2 rejects it; we mirror FT2's behaviour.
        bytes[9] = b'm';
        assert!(!is_xm(&bytes));
    }

    #[test]
    fn is_xm_rejects_short_buffer() {
        assert!(!is_xm(b"Extended Module"));
    }

    #[test]
    fn parse_header_populates_core_fields() {
        let bytes = build_header(8, 2, 3, true);
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.module_name, "hello xm");
        assert_eq!(h.tracker_name, "oxideav-test");
        assert_eq!(h.version, XM_VERSION_0104);
        assert_eq!(h.header_size, 0x114);
        assert_eq!(h.song_length, 1);
        assert_eq!(h.num_channels, 8);
        assert_eq!(h.num_patterns, 2);
        assert_eq!(h.num_instruments, 3);
        assert_eq!(h.default_tempo, 6);
        assert_eq!(h.default_bpm, 125);
        assert_eq!(h.frequency_table, XmFrequencyTable::Linear);
        assert_eq!(h.order.len(), XM_ORDER_TABLE_SIZE);
        assert_eq!(h.order[0], 0);
        assert_eq!(h.order[1], 0xFF);
    }

    #[test]
    fn parse_header_rejects_missing_id_byte() {
        let mut bytes = build_header(4, 0, 0, false);
        bytes[XM_ID_BYTE_OFFSET] = 0;
        assert!(parse_header(&bytes).is_err());
    }

    #[test]
    fn parse_header_rejects_zero_channels() {
        let mut bytes = build_header(0, 0, 0, false);
        // We stored 0 channels — parse_header should catch this.
        bytes[68..70].copy_from_slice(&0u16.to_le_bytes());
        assert!(parse_header(&bytes).is_err());
    }

    #[test]
    fn parse_header_needs_full_order_table() {
        let bytes = build_header(4, 0, 0, false);
        let short = &bytes[..XM_ORDER_TABLE_OFFSET];
        matches!(parse_header(short), Err(Error::NeedMore));
    }

    #[test]
    fn parse_patterns_all_empty_synthesizes_defaults() {
        let mut bytes = build_header(4, 1, 0, false);
        // One empty pattern: packed_size=0, 8 rows.
        bytes.extend(build_pattern_block(8, &[]));
        let h = parse_header(&bytes).unwrap();
        let (pats, end) = parse_patterns(&h, &bytes).unwrap();
        assert_eq!(pats.len(), 1);
        assert_eq!(pats[0].num_rows, 8);
        assert_eq!(pats[0].packed_size, 0);
        assert_eq!(pats[0].rows.len(), 8);
        assert_eq!(pats[0].rows[0].len(), 4);
        for row in &pats[0].rows {
            for cell in row {
                assert_eq!(*cell, XmCell::default());
            }
        }
        // No packed data → end = header_end + pattern_header_size (9).
        assert_eq!(end, pattern_data_offset(&h) + 9);
    }

    #[test]
    fn parse_patterns_hostile_header_length_does_not_panic() {
        // Regression for fuzz find `xm_decode/crash-212b2111…`: a
        // hostile pattern `header_length` that pushes `data_start`
        // past EOF must clamp the packed-data slice rather than
        // index `&bytes[oob..…]` directly and panic with "slice
        // index out of bounds".
        //
        // We construct a 1-pattern XM whose pattern header declares
        // `header_length = 0xFFFF` (well past the end of our test
        // file) and `packed_size = 1` (non-zero so the parser takes
        // the packed-slice branch instead of the
        // `packed_size == 0` synth path).
        let mut bytes = build_header(1, 1, 0, false);
        // Pattern header: header_length=0xFFFF, packing=0, rows=1,
        // packed_size=1.
        let mut block = Vec::new();
        block.extend_from_slice(&0xFFFFu32.to_le_bytes()); // header_length
        block.push(0); // packing type
        block.extend_from_slice(&1u16.to_le_bytes()); // num_rows
        block.extend_from_slice(&1u16.to_le_bytes()); // packed_size
                                                      // No packed body bytes — the slice should be empty after
                                                      // clamping. `decode_packed_cell` against an empty slice
                                                      // returns the default cell with consumed == 0.
        bytes.extend(block);

        let h = parse_header(&bytes).unwrap();
        let (pats, _end) = parse_patterns(&h, &bytes)
            .expect("hostile header_length must clamp rather than panic on the slice index");
        assert_eq!(pats.len(), 1);
        assert_eq!(pats[0].rows.len(), 1);
        assert_eq!(pats[0].rows[0].len(), 1);
        assert_eq!(pats[0].rows[0][0], XmCell::default());
    }

    #[test]
    fn parse_patterns_unpacked_cell_form() {
        let mut bytes = build_header(2, 1, 0, false);
        // One row, two channels. First cell: unpacked (5-byte) form
        // containing (note=48, inst=1, vol=0x40, fx=0x0C, fxp=0x20).
        // Second cell: packed single-byte empty (0x80) with mask=0.
        let mut packed = Vec::new();
        packed.extend_from_slice(&[48, 1, 0x40, 0x0C, 0x20]);
        packed.push(0x80); // empty packed cell
        bytes.extend(build_pattern_block(1, &packed));

        let h = parse_header(&bytes).unwrap();
        let (pats, _end) = parse_patterns(&h, &bytes).unwrap();
        let c0 = pats[0].rows[0][0];
        assert_eq!(c0.note, 48);
        assert_eq!(c0.instrument, 1);
        assert_eq!(c0.volume, 0x40);
        assert_eq!(c0.effect_type, 0x0C);
        assert_eq!(c0.effect_param, 0x20);
        assert!(c0.has_note());
        assert!(!c0.is_note_off());
        let c1 = pats[0].rows[0][1];
        assert_eq!(c1, XmCell::default());
    }

    #[test]
    fn parse_patterns_packed_cell_selective_mask() {
        let mut bytes = build_header(1, 1, 0, false);
        // One row, one channel. Packed cell selecting note+volume only:
        // mask = 0x01 | 0x04 = 0x05 → first byte 0x85.
        let mut packed = Vec::new();
        packed.extend_from_slice(&[0x80 | 0x05, 50, 0x12]); // note=50, vol=0x12
        bytes.extend(build_pattern_block(1, &packed));

        let h = parse_header(&bytes).unwrap();
        let (pats, _end) = parse_patterns(&h, &bytes).unwrap();
        let c = pats[0].rows[0][0];
        assert_eq!(c.note, 50);
        assert_eq!(c.instrument, 0);
        assert_eq!(c.volume, 0x12);
        assert_eq!(c.effect_type, 0);
        assert_eq!(c.effect_param, 0);
    }

    #[test]
    fn xm_volume_kinds_classify_correctly() {
        let mk = |v: u8| XmCell {
            volume: v,
            ..XmCell::default()
        };
        assert_eq!(mk(0).volume_kind(), XmVolume::Empty);
        assert_eq!(mk(0x10).volume_kind(), XmVolume::SetVolume(0));
        assert_eq!(mk(0x50).volume_kind(), XmVolume::SetVolume(0x40));
        assert_eq!(mk(0x63).volume_kind(), XmVolume::VolumeSlideDown(3));
        assert_eq!(mk(0x77).volume_kind(), XmVolume::VolumeSlideUp(7));
        assert_eq!(mk(0x8F).volume_kind(), XmVolume::FineVolumeSlideDown(0x0F));
        assert_eq!(mk(0x9A).volume_kind(), XmVolume::FineVolumeSlideUp(0x0A));
        assert_eq!(mk(0xA4).volume_kind(), XmVolume::SetVibratoSpeed(4));
        assert_eq!(mk(0xB5).volume_kind(), XmVolume::Vibrato(5));
        assert_eq!(mk(0xC0).volume_kind(), XmVolume::SetPanning(0));
        assert_eq!(mk(0xD2).volume_kind(), XmVolume::PanningSlideLeft(2));
        assert_eq!(mk(0xE8).volume_kind(), XmVolume::PanningSlideRight(8));
        assert_eq!(mk(0xF9).volume_kind(), XmVolume::TonePorta(9));
    }

    #[test]
    fn parse_instruments_zero_samples() {
        let mut bytes = build_header(4, 0, 2, false);
        bytes.extend(build_empty_instrument(b"empty1"));
        bytes.extend(build_empty_instrument(b"another"));
        let h = parse_header(&bytes).unwrap();
        let offset = pattern_data_offset(&h);
        let insts = parse_instruments(&h, &bytes, offset).unwrap();
        assert_eq!(insts.len(), 2);
        assert_eq!(insts[0].name, "empty1");
        assert_eq!(insts[0].num_samples, 0);
        assert!(insts[0].samples.is_empty());
        assert_eq!(insts[1].name, "another");
    }

    #[test]
    fn parse_instrument_with_one_sample_decodes_envelope_and_sample_header() {
        let mut bytes = build_header(4, 0, 1, false);
        // A 4-byte sample body; deltas decode to [1, 3, 6, 10].
        let body = [1i8, 2, 3, 4];
        let body_bytes: Vec<u8> = body.iter().map(|&b| b as u8).collect();
        bytes.extend(build_one_sample_instrument(b"kick", &body_bytes));

        let h = parse_header(&bytes).unwrap();
        let offset = pattern_data_offset(&h);
        let mut insts = parse_instruments(&h, &bytes, offset).unwrap();
        assert_eq!(insts.len(), 1);
        let inst = &insts[0];
        assert_eq!(inst.name, "kick");
        assert_eq!(inst.num_samples, 1);
        assert_eq!(inst.sample_header_size, XM_SAMPLE_HEADER_SIZE);
        assert_eq!(inst.sample_map.len(), 96);
        assert_eq!(inst.volume_envelope.points.len(), 2);
        assert_eq!(inst.volume_envelope.points[1], (64, 64));
        assert!(inst.volume_envelope.is_on());
        assert!(!inst.volume_envelope.has_sustain());
        assert_eq!(inst.samples.len(), 1);
        let s = &inst.samples[0];
        assert_eq!(s.length, body.len() as u32);
        assert_eq!(s.volume, 0x40);
        assert_eq!(s.panning, 128);
        assert!(!s.is_16_bit);
        assert_eq!(s.loop_mode, XmSampleLoopMode::None);
        assert_eq!(s.name, "snd");

        // Now decode the delta PCM.
        extract_sample_bodies(&mut insts, &bytes);
        let decoded = &insts[0].samples[0].pcm8;
        assert_eq!(decoded, &[1, 3, 6, 10]);
    }

    #[test]
    fn truncated_instrument_extended_header_errors_cleanly() {
        // A hostile instrument header_size of 29..=32 with
        // num_samples > 0 passes the `cur + header_size` bound while
        // the file ends before the sample_header_size dword at
        // cur+29..cur+33 — the read must error, not panic
        // (fuzz crash-bb1e627e).
        let mut bytes = build_header(4, 0, 1, false);
        let h = parse_header(&bytes).unwrap();
        let inst_off = pattern_data_offset(&h);
        bytes.truncate(inst_off);
        bytes.extend_from_slice(&29u32.to_le_bytes()); // header_size 29
        let mut nbuf = [0u8; 22];
        nbuf[..4].copy_from_slice(b"trap");
        bytes.extend_from_slice(&nbuf);
        bytes.push(0); // type
        bytes.extend_from_slice(&1u16.to_le_bytes()); // num_samples = 1
                                                      // File ends exactly at cur+29.
        assert!(parse_instruments(&h, &bytes, inst_off).is_err());
    }

    #[test]
    fn hostile_row_count_is_rejected() {
        // Spec bound: "Number of rows in pattern (1..256)". An
        // unclamped u16 row count with packed_size == 0 is a
        // multi-GB allocation bomb (cells are synthesized with no
        // file bytes backing them) — see the guard in
        // `parse_patterns`.
        for hostile_rows in [0u16, 257, 0xFFFF] {
            let mut bytes = build_header(32, 1, 0, false);
            let h = parse_header(&bytes).unwrap();
            bytes.extend_from_slice(&9u32.to_le_bytes()); // header length
            bytes.push(0); // packing type
            bytes.extend_from_slice(&hostile_rows.to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes()); // packed_size 0
            assert!(
                parse_patterns(&h, &bytes).is_err(),
                "row count {hostile_rows} must be rejected"
            );
        }
        // The spec maximum itself stays accepted.
        let mut bytes = build_header(4, 1, 0, false);
        let h = parse_header(&bytes).unwrap();
        bytes.extend_from_slice(&9u32.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&256u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        let (pats, _) = parse_patterns(&h, &bytes).unwrap();
        assert_eq!(pats[0].rows.len(), 256);
    }

    #[test]
    fn broken_sample_header_size_dword_is_ignored_for_parsing() {
        // `multimedia-cx-fasttracker-2.html` §File Format: "The sample
        // header size field is completely ignored by Fast Tracker 2
        // ... it's best to assume the sample header size is always 40
        // bytes and to not skip this data!" — writers in the wild stamp
        // broken values in the dword. Both broken shapes must parse to
        // the same instrument + PCM as a well-formed file:
        //   * a too-small value (0) used to hard-reject the file;
        //   * a too-large value (100) used to mis-stride past the
        //     sample header and mis-locate the PCM body.
        for broken in [0u32, 100u32] {
            let mut bytes = build_header(4, 0, 1, false);
            let inst_off = {
                let h = parse_header(&bytes).unwrap();
                pattern_data_offset(&h)
            };
            let body = [1i8, 2, 3, 4];
            let body_bytes: Vec<u8> = body.iter().map(|&b| b as u8).collect();
            bytes.extend(build_one_sample_instrument(b"kick", &body_bytes));
            // Stamp the broken dword over the extended header's
            // sample_header_size field (+29 from the record start).
            bytes[inst_off + 29..inst_off + 33].copy_from_slice(&broken.to_le_bytes());

            let h = parse_header(&bytes).unwrap();
            let mut insts = parse_instruments(&h, &bytes, inst_off)
                .unwrap_or_else(|e| panic!("sample_header_size={broken} must parse: {e:?}"));
            assert_eq!(insts.len(), 1);
            // The raw dword is surfaced verbatim for metadata fidelity…
            assert_eq!(insts[0].sample_header_size, broken);
            // …but never steers the parse: the sample header at the
            // fixed 40-byte stride decodes intact, and the PCM body is
            // found exactly where a 40-byte header leaves it.
            let smp = &insts[0].samples[0];
            assert_eq!(smp.length, body.len() as u32);
            assert_eq!(smp.volume, 0x40);
            assert_eq!(smp.name, "snd");
            extract_sample_bodies(&mut insts, &bytes);
            assert_eq!(insts[0].samples[0].pcm8, [1, 3, 6, 10]);
        }
    }

    #[test]
    fn extract_sample_bodies_handles_truncated_body() {
        let mut bytes = build_header(4, 0, 1, false);
        let body_bytes = vec![1u8, 2, 3, 4, 5];
        bytes.extend(build_one_sample_instrument(b"s", &body_bytes));
        // Chop off the last 2 bytes of the sample body.
        let drop = 2;
        bytes.truncate(bytes.len() - drop);

        let h = parse_header(&bytes).unwrap();
        let mut insts = parse_instruments(&h, &bytes, pattern_data_offset(&h)).unwrap();
        extract_sample_bodies(&mut insts, &bytes);
        assert_eq!(insts[0].samples[0].pcm8.len(), body_bytes.len() - drop);
    }

    #[test]
    fn decode_packed_cell_empty_mask_byte() {
        // 0x80 alone = all masks clear = one-byte empty cell.
        let (cell, used) = decode_packed_cell(&[0x80], 0);
        assert_eq!(cell, XmCell::default());
        assert_eq!(used, 1);
    }

    #[test]
    fn pattern_data_offset_is_standard_for_default_header_size() {
        let bytes = build_header(4, 0, 0, false);
        let h = parse_header(&bytes).unwrap();
        // header_size is measured from offset 60 inclusive; for the
        // canonical value 0x114 this lands at 60 + 0x114 = 0x150 (336),
        // which is exactly the end of the 256-byte order table that
        // starts at offset 80.
        assert_eq!(pattern_data_offset(&h), 0x150);
        assert_eq!(0x150, 336);
    }

    #[test]
    fn estimate_duration_micros_is_positive_for_nonempty_song() {
        let mut bytes = build_header(4, 1, 0, false);
        bytes.extend(build_pattern_block(64, &[]));
        let h = parse_header(&bytes).unwrap();
        let (pats, _) = parse_patterns(&h, &bytes).unwrap();
        let us = estimate_duration_micros(&h, &pats);
        assert!(us > 0, "estimate_duration_micros returned {us}");
    }

    // ---- XmSampleHeader typed loop / length accessors ----

    fn sample_header_with(
        loop_mode: XmSampleLoopMode,
        is_16_bit: bool,
        length: u32,
        loop_start: u32,
        loop_length: u32,
    ) -> XmSampleHeader {
        XmSampleHeader {
            loop_mode,
            is_16_bit,
            length,
            loop_start,
            loop_length,
            ..XmSampleHeader::default()
        }
    }

    #[test]
    fn xm_sample_is_looped_tracks_loop_mode_enum() {
        // None → false; Forward and PingPong → true. The mode comes from
        // the type byte's bits 0-1, so the three states partition cleanly.
        assert!(!sample_header_with(XmSampleLoopMode::None, false, 100, 0, 0).is_looped());
        assert!(sample_header_with(XmSampleLoopMode::Forward, false, 100, 0, 50).is_looped());
        assert!(sample_header_with(XmSampleLoopMode::PingPong, false, 100, 10, 40).is_looped());
    }

    #[test]
    fn xm_sample_loop_region_none_when_not_looped() {
        // Even when loop_start / loop_length carry nonzero leftover
        // values, a `None` loop mode forces the typed accessor to None
        // — the type byte is authoritative.
        let h = sample_header_with(XmSampleLoopMode::None, false, 100, 32, 16);
        assert_eq!(h.loop_region_frames(), None);
    }

    #[test]
    fn xm_sample_loop_region_passes_through_8bit_bytes_as_frames() {
        // 8-bit sample: one byte = one frame, so the on-disk loop
        // fields are already frame indices and pass through unchanged.
        let h = sample_header_with(XmSampleLoopMode::Forward, false, 100, 32, 16);
        assert_eq!(h.loop_region_frames(), Some((32, 16)));
    }

    #[test]
    fn xm_sample_loop_region_halves_16bit_bytes_into_frames() {
        // 16-bit sample: two bytes per frame, so the on-disk byte
        // counts have to be divided by 2 to land in the same index
        // space as the SampleSource cursor.
        let h = sample_header_with(XmSampleLoopMode::Forward, true, 200, 64, 32);
        assert_eq!(h.loop_region_frames(), Some((32, 16)));
    }

    #[test]
    fn xm_sample_loop_region_returns_header_pair_unclamped() {
        // No PCM-side clamp — the typed view returns what the header
        // declares even when start + length exceeds the declared length.
        // The mixer's SampleSource::loop_end impl owns the
        // `.min(self.len())` clamp.
        let h = sample_header_with(XmSampleLoopMode::Forward, false, 32, 24, 64);
        assert_eq!(h.loop_region_frames(), Some((24, 64)));
    }

    #[test]
    fn xm_sample_length_frames_handles_both_widths() {
        // 8-bit: bytes == frames. 16-bit: frames = bytes / 2.
        assert_eq!(
            sample_header_with(XmSampleLoopMode::None, false, 100, 0, 0).length_frames(),
            100
        );
        assert_eq!(
            sample_header_with(XmSampleLoopMode::None, true, 200, 0, 0).length_frames(),
            100
        );
    }

    #[test]
    fn xm_sample_pingpong_is_classified_as_looped() {
        // Distinct from MOD: XM's ping-pong loop mode is a separate
        // type-byte encoding, and the typed accessor recognises it
        // alongside forward.
        let h = sample_header_with(XmSampleLoopMode::PingPong, false, 100, 0, 50);
        assert!(h.is_looped());
        assert_eq!(h.loop_region_frames(), Some((0, 50)));
    }

    // ---- XmSampleHeader typed pitch-transpose accessors ----

    fn sample_header_with_pitch(finetune: i8, relative_note: i8) -> XmSampleHeader {
        XmSampleHeader {
            finetune,
            relative_note,
            ..XmSampleHeader::default()
        }
    }

    #[test]
    fn xm_sample_finetune_semitones_is_zero_at_neutral() {
        // Neutral finetune == "play at tabulated period exactly" ==
        // zero semitone offset.
        let h = sample_header_with_pitch(0, 0);
        assert_eq!(h.finetune_semitones(), 0.0);
    }

    #[test]
    fn xm_sample_finetune_semitones_scales_at_one_over_128() {
        // The Linear period formula `Period = 10*12*16*4 - Note*16*4
        // - FineTune/2` makes one semitone = 64 period units and one
        // finetune unit = 1/2 period unit, so the conversion is
        // FineTune / 128. Check the half-semitone and one-semitone
        // anchors that the formula's grid lines up with.
        let half_up = sample_header_with_pitch(64, 0);
        assert!((half_up.finetune_semitones() - 0.5).abs() < 1e-6);
        // Full-scale negative finetune lands at exactly -1 semitone.
        let one_down = sample_header_with_pitch(-128, 0);
        assert!((one_down.finetune_semitones() - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn xm_sample_finetune_semitones_symmetric_around_zero() {
        // The signed-byte mapping is symmetric for the values that
        // round-trip; +64 == +0.5 and -64 == -0.5.
        let up = sample_header_with_pitch(64, 0);
        let down = sample_header_with_pitch(-64, 0);
        assert!((up.finetune_semitones() + down.finetune_semitones()).abs() < 1e-6);
    }

    #[test]
    fn xm_sample_transpose_semitones_sums_relative_note_and_finetune() {
        // Net transpose is the integer-semitone relative_note plus the
        // fractional-semitone finetune. Use a half-step finetune at a
        // 12-semitone offset to verify both terms land in the sum.
        let h = sample_header_with_pitch(64, 12);
        assert!((h.transpose_semitones() - 12.5).abs() < 1e-6);

        // Negative composition: -12 semitones minus 0.5 semitones.
        let h2 = sample_header_with_pitch(-64, -12);
        assert!((h2.transpose_semitones() - (-12.5)).abs() < 1e-6);
    }

    #[test]
    fn xm_sample_transpose_semitones_pure_relative_note() {
        // Zero finetune: transpose equals relative_note exactly. This
        // is the common case for typical instruments — the engine
        // configures pitch via the relative_note field with
        // finetune == 0.
        let h = sample_header_with_pitch(0, 24);
        assert_eq!(h.transpose_semitones(), 24.0);
    }
}
