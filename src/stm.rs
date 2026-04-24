//! Scream Tracker v1.0 ("STM") module parsing.
//!
//! STM is the pre-S3M module format used by Scream Tracker 1.x. It's a
//! smaller, 4-channel-fixed tracker format with Intel (little-endian)
//! byte order, 31 instruments, and 64-row patterns stored as
//! `4 rows * 4 channels * 4 bytes` fixed-size cells.
//!
//! Layout summary (see `docs/audio/trackers/stm/ScreamTracker-v1.0-stm.txt`):
//!
//! ```text
//! Offset 0x000    20 bytes   ASCIIZ song name
//! Offset 0x014     8 bytes   Tracker name ("!Scream!" typically)
//! Offset 0x01C     1 byte    ID = 0x1A
//! Offset 0x01D     1 byte    File type: 1 = song (no samples), 2 = module
//! Offset 0x01E     1 byte    Major version
//! Offset 0x01F     1 byte    Minor version
//! Offset 0x020     1 byte    Playback tempo
//! Offset 0x021     1 byte    Number of patterns ("PAT" in the doc)
//! Offset 0x022     1 byte    Global playback volume
//! Offset 0x023    13 bytes   reserved
//! Offset 0x030    31 * 32    Instrument records (32 bytes each):
//!                              12 char ASCIIZ instrument name
//!                               1 byte ID = 0
//!                               1 byte instrument disk
//!                               2 bytes reserved
//!                               2 bytes sample length (bytes, LE)
//!                               2 bytes sample loop start (LE)
//!                               2 bytes sample loop end   (LE)
//!                               1 byte  default volume
//!                               1 byte  reserved
//!                               2 bytes C3 frequency (LE)
//!                               4 bytes reserved
//!                               2 bytes length in paragraphs (modules only)
//! Offset 0x3D0    64 bytes   Pattern order table (entries 0..=63)
//! Offset 0x410    PAT*1024   Pattern data (64 rows × 4 ch × 4 bytes each)
//! Then            …          Raw sample bodies (16-byte padded).
//! ```
//!
//! This module only parses structural metadata + sample extraction;
//! playback is not currently wired to the MOD mixer (STM uses C3
//! frequencies rather than Amiga periods, so the pitch math differs).
//! The Demuxer emits a single packet carrying the entire file; a decoder
//! looking to play STM can parse the tables exposed here and render.

use oxideav_core::{Error, Result};

/// STM fixed header size before instrument table.
pub const HEADER_PREFIX_SIZE: usize = 0x30;
/// Size of one instrument record, in bytes.
pub const INSTRUMENT_RECORD_SIZE: usize = 32;
/// Number of instruments in an STM file.
pub const INSTRUMENT_COUNT: usize = 31;
/// Offset of the pattern order table.
pub const ORDER_TABLE_OFFSET: usize = 0x3D0;
/// Pattern order table size in bytes.
pub const ORDER_TABLE_SIZE: usize = 64;
/// Offset where pattern data begins.
pub const PATTERN_DATA_OFFSET: usize = 0x410;
/// STM patterns always have 64 rows.
pub const PATTERN_ROWS: usize = 64;
/// STM is always 4 channels.
pub const STM_CHANNELS: usize = 4;
/// Bytes per row-cell (1 note + 1 volume/sample + 1 command + 1 cmd param).
pub const CELL_BYTES: usize = 4;
/// Bytes per pattern: 64 rows * 4 channels * 4 bytes.
pub const BYTES_PER_PATTERN: usize = PATTERN_ROWS * STM_CHANNELS * CELL_BYTES;

/// STM file type field (offset 0x1D).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StmFileType {
    /// 1 — song data only, no samples.
    Song,
    /// 2 — module with samples.
    Module,
    Other(u8),
}

impl From<u8> for StmFileType {
    fn from(v: u8) -> Self {
        match v {
            1 => StmFileType::Song,
            2 => StmFileType::Module,
            x => StmFileType::Other(x),
        }
    }
}

/// Decoded STM instrument record (pre-sample-extraction).
#[derive(Clone, Debug, Default)]
pub struct StmInstrument {
    pub name: String,
    /// "Instrument disk" — legacy field, kept for fidelity.
    pub disk: u8,
    /// Sample length in bytes (raw 16-bit LE from the file).
    pub length: u16,
    /// Loop start, in samples (byte offset for 8-bit PCM).
    pub loop_start: u16,
    /// Loop end, in samples. 0xFFFF commonly indicates "no loop".
    pub loop_end: u16,
    /// Default volume 0..=64.
    pub volume: u8,
    /// C3 frequency in Hz.
    pub c3_hz: u16,
    /// Length in paragraphs (module files only).
    pub paragraphs: u16,
}

/// Top-level STM header.
#[derive(Clone, Debug)]
pub struct StmHeader {
    pub title: String,
    pub tracker_name: String,
    pub file_type: StmFileType,
    pub version_major: u8,
    pub version_minor: u8,
    pub tempo: u8,
    pub n_patterns: u8,
    pub global_volume: u8,
    pub instruments: Vec<StmInstrument>,
    pub order: Vec<u8>,
}

/// One decoded pattern cell.
#[derive(Clone, Copy, Debug, Default)]
pub struct StmCell {
    /// Raw note byte (251..=255 are flag markers; otherwise
    /// high nibble = octave, low nibble = semitone within octave).
    pub note_raw: u8,
    /// Instrument index 0..=31 (0 = no instrument change).
    pub instrument: u8,
    /// Volume 0..=64 (may be a combined nibble/bits field per spec).
    pub volume: u8,
    /// Effect command nibble (0..=0xF).
    pub command: u8,
    /// Command parameter byte.
    pub command_param: u8,
}

impl StmCell {
    /// Classify the note byte.
    pub fn kind(&self) -> StmNoteKind {
        match self.note_raw {
            251 => StmNoteKind::Empty,
            252 => StmNoteKind::DashNote,
            253 => StmNoteKind::Dots,
            254 | 255 => StmNoteKind::Reserved,
            _ => StmNoteKind::Note {
                octave: self.note_raw >> 4,
                semitone: self.note_raw & 0x0F,
            },
        }
    }
}

/// Semantic interpretation of the STM note byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StmNoteKind {
    Note { octave: u8, semitone: u8 },
    Empty,
    DashNote,
    Dots,
    Reserved,
}

#[derive(Clone, Debug)]
pub struct StmPattern {
    /// `rows[row_index][channel_index]`.
    pub rows: Vec<Vec<StmCell>>,
}

/// Per-instrument sample body after extraction.
#[derive(Clone, Debug, Default)]
pub struct StmSampleBody {
    /// Raw signed 8-bit PCM. Empty if the instrument has no samples or
    /// the file was truncated.
    pub pcm: Vec<i8>,
    pub loop_start: u16,
    pub loop_end: u16,
    pub volume: u8,
    pub c3_hz: u16,
}

impl StmSampleBody {
    /// True if this body carries a valid forward loop. STM signals
    /// "no loop" either with `loop_end == 0xFFFF` or by having
    /// `loop_end <= loop_start`.
    pub fn is_looped(&self) -> bool {
        self.loop_end != 0xFFFF && (self.loop_end as usize) > (self.loop_start as usize)
    }
}

impl crate::mixer::SampleSource for StmSampleBody {
    fn len(&self) -> usize {
        self.pcm.len()
    }
    fn loop_start(&self) -> usize {
        if self.is_looped() {
            (self.loop_start as usize).min(self.pcm.len())
        } else {
            0
        }
    }
    fn loop_end(&self) -> usize {
        if self.is_looped() {
            (self.loop_end as usize).min(self.pcm.len())
        } else {
            self.pcm.len()
        }
    }
    fn loop_kind(&self) -> crate::mixer::LoopKind {
        if self.is_looped() {
            crate::mixer::LoopKind::Forward
        } else {
            crate::mixer::LoopKind::None
        }
    }
    fn at(&self, idx: usize) -> f32 {
        self.pcm.get(idx).copied().unwrap_or(0) as f32 / 128.0
    }
}

/// Test whether a byte slice looks like an STM file. Returns `true` iff
/// the mandatory ID byte at offset 0x1C is 0x1A and the file type byte
/// at 0x1D is 1 or 2 (Song / Module). The 8-byte tracker-name field is
/// informational — typical values are `"!Scream!"`, `"!Scrn123"`, etc.
pub fn is_stm(bytes: &[u8]) -> bool {
    if bytes.len() < HEADER_PREFIX_SIZE {
        return false;
    }
    // Tracker name must be printable ASCII or spaces.
    if !bytes[0x14..0x1C]
        .iter()
        .all(|&b| b.is_ascii_graphic() || b == b' ')
    {
        return false;
    }
    if bytes[0x1C] != 0x1A {
        return false;
    }
    let file_type = bytes[0x1D];
    file_type == 1 || file_type == 2
}

fn read_cstring(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end])
        .trim_end()
        .to_string()
}

fn read_u16_le(bytes: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([bytes[off], bytes[off + 1]])
}

/// Parse an STM file header + instrument table + order table.
///
/// Returns `Error::NeedMore` if the buffer is too short to reach the
/// order table; `Error::invalid` if the mandatory ID byte is wrong.
pub fn parse_header(bytes: &[u8]) -> Result<StmHeader> {
    if bytes.len() < ORDER_TABLE_OFFSET + ORDER_TABLE_SIZE {
        return Err(Error::NeedMore);
    }
    if bytes[0x1C] != 0x1A {
        return Err(Error::invalid("STM: missing 0x1A id byte at offset 0x1C"));
    }

    let title = read_cstring(&bytes[0..20]);
    let tracker_name = read_cstring(&bytes[0x14..0x1C]);
    let file_type = StmFileType::from(bytes[0x1D]);
    let version_major = bytes[0x1E];
    let version_minor = bytes[0x1F];
    let tempo = bytes[0x20];
    let n_patterns = bytes[0x21];
    let global_volume = bytes[0x22];

    // Instrument table.
    let mut instruments = Vec::with_capacity(INSTRUMENT_COUNT);
    for i in 0..INSTRUMENT_COUNT {
        let off = HEADER_PREFIX_SIZE + i * INSTRUMENT_RECORD_SIZE;
        let rec = &bytes[off..off + INSTRUMENT_RECORD_SIZE];
        let name = read_cstring(&rec[0..12]);
        // rec[12] = ID, rec[13] = disk, rec[14..16] reserved
        let disk = rec[13];
        let length = read_u16_le(rec, 16);
        let loop_start = read_u16_le(rec, 18);
        let loop_end = read_u16_le(rec, 20);
        let volume = rec[22].min(64);
        // rec[23] reserved
        let c3_hz = read_u16_le(rec, 24);
        // rec[26..30] reserved
        let paragraphs = read_u16_le(rec, 30);
        instruments.push(StmInstrument {
            name,
            disk,
            length,
            loop_start,
            loop_end,
            volume,
            c3_hz,
            paragraphs,
        });
    }

    let order: Vec<u8> = bytes[ORDER_TABLE_OFFSET..ORDER_TABLE_OFFSET + ORDER_TABLE_SIZE].to_vec();

    Ok(StmHeader {
        title,
        tracker_name,
        file_type,
        version_major,
        version_minor,
        tempo,
        n_patterns,
        global_volume,
        instruments,
        order,
    })
}

/// Parse all STM patterns from the buffer.
///
/// Any pattern whose data runs past end-of-file is silently truncated:
/// unread cells default to `StmCell::default()`. This matches the
/// robust-on-truncation policy used by the MOD parser.
pub fn parse_patterns(header: &StmHeader, bytes: &[u8]) -> Vec<StmPattern> {
    let mut patterns = Vec::with_capacity(header.n_patterns as usize);
    for p in 0..header.n_patterns as usize {
        let mut rows = Vec::with_capacity(PATTERN_ROWS);
        for r in 0..PATTERN_ROWS {
            let mut row = Vec::with_capacity(STM_CHANNELS);
            for c in 0..STM_CHANNELS {
                let off = PATTERN_DATA_OFFSET
                    + p * BYTES_PER_PATTERN
                    + r * STM_CHANNELS * CELL_BYTES
                    + c * CELL_BYTES;
                let cell = if off + CELL_BYTES <= bytes.len() {
                    // Byte 0 is the note byte.
                    // Byte 1: bits 0..=2 low of volume, bits 3..=7 instrument.
                    // Byte 2: bits 0..=3 command (ProTracker-ish),
                    //         bits 4..=6 upper bits of volume.
                    // Byte 3: command parameter.
                    let b0 = bytes[off];
                    let b1 = bytes[off + 1];
                    let b2 = bytes[off + 2];
                    let b3 = bytes[off + 3];
                    let instrument = (b1 >> 3) & 0x1F;
                    let vol_lo = b1 & 0x07;
                    let vol_hi = (b2 >> 4) & 0x07;
                    let volume = (vol_hi << 3) | vol_lo;
                    let command = b2 & 0x0F;
                    StmCell {
                        note_raw: b0,
                        instrument,
                        volume: volume.min(64),
                        command,
                        command_param: b3,
                    }
                } else {
                    StmCell::default()
                };
                row.push(cell);
            }
            rows.push(row);
        }
        patterns.push(StmPattern { rows });
    }
    patterns
}

/// Absolute offset in the file where sample bodies begin.
pub fn sample_data_offset(header: &StmHeader) -> usize {
    PATTERN_DATA_OFFSET + header.n_patterns as usize * BYTES_PER_PATTERN
}

/// Extract all 31 instrument sample bodies.
///
/// Samples extending past EOF are clamped; if any sample straddles the
/// 16-byte padding boundary mentioned in the spec we ignore the padding
/// (sample bodies are laid out contiguously by length, per the spec).
pub fn extract_samples(header: &StmHeader, bytes: &[u8]) -> Vec<StmSampleBody> {
    let mut out = Vec::with_capacity(header.instruments.len());
    let mut cursor = sample_data_offset(header);
    let end = bytes.len();
    for inst in &header.instruments {
        let declared = inst.length as usize;
        let available = end.saturating_sub(cursor);
        let take = declared.min(available);
        let pcm: Vec<i8> = if take == 0 {
            Vec::new()
        } else {
            bytes[cursor..cursor + take]
                .iter()
                .map(|&b| b as i8)
                .collect()
        };
        cursor += take;
        out.push(StmSampleBody {
            pcm,
            loop_start: inst.loop_start,
            loop_end: inst.loop_end,
            volume: inst.volume,
            c3_hz: inst.c3_hz,
        });
    }
    out
}

/// Rough upper-bound duration estimate in microseconds, derived from
/// `song_length * rows * 6 ticks / tempo` analogous to the MOD estimate.
/// STM's tempo field is *not* ProTracker BPM — it's a per-row tick count
/// scaled by a hardware factor. For a rough estimate we treat the tempo
/// field like a BPM-ish value; real songs change tempo via effects so
/// callers should treat this as a loose upper bound only.
pub fn estimate_duration_micros(header: &StmHeader) -> i64 {
    let orders = (header.n_patterns as i64).max(1);
    let tempo = header.tempo.max(1) as i64;
    // Fallback heuristic: ~125 BPM equivalent at tempo=0x60 (96).
    let bpm_equiv = (tempo * 125 / 0x60).max(30);
    orders.saturating_mul(64 * 6 * 1_000_000) / (bpm_equiv * 2 / 5).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal STM with `n_patterns` patterns and one instrument
    /// carrying a 4-byte body.
    fn build_minimal_stm(n_patterns: u8) -> Vec<u8> {
        let mut out = vec![0u8; PATTERN_DATA_OFFSET];
        out[0..4].copy_from_slice(b"test");
        out[0x14..0x1C].copy_from_slice(b"!Scream!");
        out[0x1C] = 0x1A;
        out[0x1D] = 2; // module
        out[0x1E] = 2;
        out[0x1F] = 0;
        out[0x20] = 0x60; // tempo
        out[0x21] = n_patterns;
        out[0x22] = 64; // global volume

        // Instrument 0 at HEADER_PREFIX_SIZE (0x30).
        let inst_off = HEADER_PREFIX_SIZE;
        out[inst_off..inst_off + 4].copy_from_slice(b"bass");
        // length (bytes) at rec offset 16 → file offset inst_off + 16
        out[inst_off + 16..inst_off + 18].copy_from_slice(&4u16.to_le_bytes());
        out[inst_off + 22] = 64; // volume
        out[inst_off + 24..inst_off + 26].copy_from_slice(&8363u16.to_le_bytes()); // C3 hz

        // Order: only pattern 0 at index 0.
        out[ORDER_TABLE_OFFSET] = 0;
        for i in 1..ORDER_TABLE_SIZE {
            out[ORDER_TABLE_OFFSET + i] = 255;
        }

        // Pattern block filled with zeros.
        out.extend(std::iter::repeat_n(0u8, n_patterns as usize * BYTES_PER_PATTERN));

        // Instrument 0 body: 4 signed bytes.
        out.extend([0x10u8, 0xF0, 0x40, 0xC0]);
        out
    }

    #[test]
    fn is_stm_accepts_minimal_file() {
        let bytes = build_minimal_stm(1);
        assert!(is_stm(&bytes));
    }

    #[test]
    fn is_stm_rejects_missing_id_byte() {
        let mut bytes = build_minimal_stm(1);
        bytes[0x1C] = 0;
        assert!(!is_stm(&bytes));
    }

    #[test]
    fn is_stm_rejects_bad_file_type() {
        let mut bytes = build_minimal_stm(1);
        bytes[0x1D] = 99;
        assert!(!is_stm(&bytes));
    }

    #[test]
    fn parse_header_populates_core_fields() {
        let bytes = build_minimal_stm(2);
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.title, "test");
        assert_eq!(h.tracker_name, "!Scream!");
        assert_eq!(h.file_type, StmFileType::Module);
        assert_eq!(h.version_major, 2);
        assert_eq!(h.tempo, 0x60);
        assert_eq!(h.n_patterns, 2);
        assert_eq!(h.global_volume, 64);
        assert_eq!(h.instruments.len(), INSTRUMENT_COUNT);
        assert_eq!(h.instruments[0].name, "bass");
        assert_eq!(h.instruments[0].length, 4);
        assert_eq!(h.instruments[0].c3_hz, 8363);
        assert_eq!(h.order.len(), ORDER_TABLE_SIZE);
        assert_eq!(h.order[0], 0);
    }

    #[test]
    fn parse_header_rejects_bad_id_byte() {
        let mut bytes = build_minimal_stm(1);
        bytes[0x1C] = 0;
        assert!(parse_header(&bytes).is_err());
    }

    #[test]
    fn parse_header_needs_full_order_table() {
        let bytes = build_minimal_stm(1);
        let short = &bytes[..0x3D0];
        assert!(parse_header(short).is_err());
    }

    #[test]
    fn parse_patterns_returns_empty_rows_by_default() {
        let bytes = build_minimal_stm(1);
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        assert_eq!(pats.len(), 1);
        assert_eq!(pats[0].rows.len(), PATTERN_ROWS);
        assert_eq!(pats[0].rows[0].len(), STM_CHANNELS);
        assert_eq!(pats[0].rows[0][0].note_raw, 0);
    }

    #[test]
    fn parse_patterns_decodes_cell_bit_fields() {
        let mut bytes = build_minimal_stm(1);
        // Row 0, channel 0: octave 4, semitone 0 → note_raw = 0x40 (64).
        // b1 = instrument=3 (<< 3), vol_lo=4 → 0x1C
        // b2 = vol_hi=2 (<< 4), command=5 → 0x25
        // b3 = command param 0x0A
        let cell_off = PATTERN_DATA_OFFSET;
        bytes[cell_off] = 0x40;
        bytes[cell_off + 1] = (3 << 3) | 4;
        bytes[cell_off + 2] = (2 << 4) | 5;
        bytes[cell_off + 3] = 0x0A;

        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let c = pats[0].rows[0][0];
        assert_eq!(c.note_raw, 0x40);
        assert_eq!(c.kind(), StmNoteKind::Note { octave: 4, semitone: 0 });
        assert_eq!(c.instrument, 3);
        assert_eq!(c.volume, (2 << 3) | 4);
        assert_eq!(c.command, 5);
        assert_eq!(c.command_param, 0x0A);
    }

    #[test]
    fn cell_kind_classifies_reserved_values() {
        let make = |n: u8| StmCell {
            note_raw: n,
            ..StmCell::default()
        };
        assert_eq!(make(251).kind(), StmNoteKind::Empty);
        assert_eq!(make(252).kind(), StmNoteKind::DashNote);
        assert_eq!(make(253).kind(), StmNoteKind::Dots);
        assert_eq!(make(254).kind(), StmNoteKind::Reserved);
    }

    #[test]
    fn extract_samples_reads_instrument_body() {
        let bytes = build_minimal_stm(1);
        let h = parse_header(&bytes).unwrap();
        let samples = extract_samples(&h, &bytes);
        assert_eq!(samples.len(), INSTRUMENT_COUNT);
        assert_eq!(samples[0].pcm.len(), 4);
        assert_eq!(samples[0].pcm[0], 0x10);
        // 0xF0 as signed i8 is -16.
        assert_eq!(samples[0].pcm[1], -16);
        for s in &samples[1..] {
            assert!(s.pcm.is_empty());
        }
    }

    #[test]
    fn extract_samples_handles_truncated_body() {
        let mut bytes = build_minimal_stm(1);
        // Cut the last 2 bytes of the sample body.
        bytes.truncate(bytes.len() - 2);
        let h = parse_header(&bytes).unwrap();
        let samples = extract_samples(&h, &bytes);
        assert_eq!(samples[0].pcm.len(), 2);
    }
}
