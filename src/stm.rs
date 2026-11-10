//! Scream Tracker 2 ("STM" module / "STS" song) parsing.
//!
//! STM is the pre-S3M format written by Scream Tracker 1.x / 2.x: four
//! channels, 31 instruments, 64-row patterns, Intel byte order. The
//! layout implemented here is the one measured on real Scream Tracker
//! 2.21 files in
//! `docs/audio/trackers/stm/stm-pattern-packing-and-order-table.md`,
//! with the field meanings from the tracker author's own
//! `ScreamTracker-v2.3-TECH.DOC.txt` ("Song/Module file structure") and
//! user manual (`ScreamTracker-v2.3-ST.DOC.txt`):
//!
//! ```text
//! 0x000    20 bytes   song name, ASCIIZ
//! 0x014     8 bytes   tracker name, no NUL ("!Scream!")
//! 0x01C     1 byte    0x1A
//! 0x01D     1 byte    file type: 1 = song (no samples), 2 = module
//! 0x01E     2 bytes   version major, minor (2, 21 for ST 2.21)
//! 0x020     1 byte    tempo (high nibble = ticks per row)
//! 0x021     1 byte    number of patterns saved
//! 0x022     1 byte    global volume
//! 0x023    13 bytes   reserved
//! 0x030    31 * 32    instrument records:
//!                        0  12 bytes  file name, ASCIIZ
//!                       12   1 byte   0
//!                       13   1 byte   instrument disk
//!                       14   2 bytes  reserved
//!                       16   2 bytes  length in bytes
//!                       18   2 bytes  loop start
//!                       20   2 bytes  loop end (0 / 65535 = no loop)
//!                       22   1 byte   default volume
//!                       23   1 byte   reserved
//!                       24   2 bytes  C2SPD — Hz that sounds as C-2
//!                       26   4 bytes  reserved
//!                       30   2 bytes  internal segment (0 on disk)
//! 0x410   128 bytes   order table (99 = unused / end)
//! 0x490   patterns    64 rows x 4 channels
//! then    samples     modules only: 8-bit UNSIGNED bodies, each one
//!                     padded to a 16-byte boundary
//! ```
//!
//! A pattern cell is four bytes — note byte, `instrument << 3 | volume
//! low bits`, `volume high bits << 4 | command`, command info — except
//! that a note byte of 251, 252 or 253 is a one-byte cell ("last 3
//! bytes NOT in file"). Modules written by ST 2.21 never use the
//! one-byte forms (every pattern is 1 024 bytes); songs (file type 1)
//! do, and the reader honours them for both file types.
//!
//! The third-party `ScreamTracker-v1.0-stm.txt` places the order table
//! and the patterns 64 bytes early (a 64-entry order table); the
//! measured ST 2.21 layout above supersedes it.

use oxideav_core::{Error, Result};

/// STM fixed header size before instrument table.
pub const HEADER_PREFIX_SIZE: usize = 0x30;
/// Size of one instrument record, in bytes.
pub const INSTRUMENT_RECORD_SIZE: usize = 32;
/// Number of instruments in an STM file.
pub const INSTRUMENT_COUNT: usize = 31;
/// Offset of the pattern order table (directly after the instruments).
pub const ORDER_TABLE_OFFSET: usize = 0x410;
/// Pattern order table size in bytes.
pub const ORDER_TABLE_SIZE: usize = 128;
/// Offset where pattern data begins.
pub const PATTERN_DATA_OFFSET: usize = 0x490;
/// STM patterns always have 64 rows.
pub const PATTERN_ROWS: usize = 64;
/// STM is always 4 channels.
pub const STM_CHANNELS: usize = 4;
/// Bytes per unpacked row-cell (note, instrument / volume, volume /
/// command, command info).
pub const CELL_BYTES: usize = 4;
/// Bytes per unpacked pattern: 64 rows * 4 channels * 4 bytes.
pub const BYTES_PER_PATTERN: usize = PATTERN_ROWS * STM_CHANNELS * CELL_BYTES;
/// Order-table value Scream Tracker writes into unused entries; the
/// first one ends the song.
pub const ORDER_UNUSED: u8 = 99;
/// Cell volume value meaning "no volume given" (the tracker writes 65;
/// anything above 64 reads as none).
pub const VOLUME_NONE: u8 = 65;
/// Sample bodies are padded to this many bytes in modules.
pub const SAMPLE_ALIGN: usize = 16;

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
#[non_exhaustive]
pub struct StmInstrument {
    pub name: String,
    /// "Instrument disk" — legacy field, kept for fidelity.
    pub disk: u8,
    /// Sample length in bytes (raw 16-bit LE from the file).
    pub length: u16,
    /// Loop start byte.
    pub loop_start: u16,
    /// Loop end byte. `loop_start == 0 && loop_end == 65535` is the
    /// tracker's "no looping" pair.
    pub loop_end: u16,
    /// Default volume 0..=64.
    pub volume: u8,
    /// C2SPD — "the speed (in Hz) at which the sample should be played
    /// so that it would sound like C2 (mid C)" (user manual §2.3). The
    /// field keeps its historical name.
    pub c3_hz: u16,
    /// The record's last word: an in-memory segment while the tracker
    /// plays, 0 in every measured ST 2.21 file. Not used for placement.
    pub paragraphs: u16,
}

impl StmInstrument {
    /// True when the record's loop pair asks for looping: anything but
    /// the documented `0` / `65535` "no looping" pair, with the end
    /// after the start.
    pub fn is_looped(&self) -> bool {
        loop_pair_is_live(self.loop_start, self.loop_end)
    }
}

fn loop_pair_is_live(start: u16, end: u16) -> bool {
    !(start == 0 && end == 0xFFFF) && end > start
}

/// Top-level STM header.
#[derive(Clone, Debug)]
#[non_exhaustive]
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
    /// The raw 128-entry order table; unused entries hold
    /// [`ORDER_UNUSED`].
    pub order: Vec<u8>,
}

impl StmHeader {
    /// Number of leading order entries that name a stored pattern — the
    /// song proper. The first entry that is [`ORDER_UNUSED`] or names a
    /// pattern the file does not store ends it.
    pub fn song_length(&self) -> usize {
        self.order
            .iter()
            .position(|&p| p >= self.n_patterns || p >= ORDER_UNUSED)
            .unwrap_or(self.order.len())
    }

    /// True for the header Scream Tracker 2.21 itself writes: tracker
    /// name `!Scream!`, version 2.21.
    pub fn is_st221(&self) -> bool {
        self.tracker_name == "!Scream!" && self.version_major == 2 && self.version_minor == 21
    }
}

/// One decoded pattern cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StmCell {
    /// Raw note byte (251..=255 are the special forms; otherwise
    /// high nibble = octave, low nibble = note within the octave).
    pub note_raw: u8,
    /// Instrument number 1..=31 (0 = none given).
    pub instrument: u8,
    /// Volume 0..=64, or [`VOLUME_NONE`] when the cell gives none.
    pub volume: u8,
    /// Special-command number: 0 = none, 1 = `A` … 15 = `O`.
    pub command: u8,
    /// Command info byte.
    pub command_param: u8,
}

impl Default for StmCell {
    /// The empty cell: no note, no instrument, no volume, no command.
    fn default() -> Self {
        StmCell {
            note_raw: 255,
            instrument: 0,
            volume: VOLUME_NONE,
            command: 0,
            command_param: 0,
        }
    }
}

impl StmCell {
    /// Classify the note byte.
    pub fn kind(&self) -> StmNoteKind {
        match self.note_raw {
            251 => StmNoteKind::Empty,
            252 | 254 => StmNoteKind::DashNote,
            253 | 255 => StmNoteKind::Dots,
            _ => StmNoteKind::Note {
                octave: self.note_raw >> 4,
                semitone: self.note_raw & 0x0F,
            },
        }
    }

    /// True when the cell carries a volume (0..=64).
    pub fn has_volume(&self) -> bool {
        self.volume <= 64
    }

    /// The four on-disk bytes of this cell (the unpacked form).
    pub fn to_bytes(&self) -> [u8; 4] {
        let vol = self.volume.min(127);
        [
            self.note_raw,
            (self.instrument << 3) | (vol & 0x07),
            ((vol >> 3) << 4) | (self.command & 0x0F),
            self.command_param,
        ]
    }

    /// Decode a four-byte cell.
    pub fn from_bytes(b: [u8; 4]) -> Self {
        StmCell {
            note_raw: b[0],
            instrument: b[1] >> 3,
            volume: (b[1] & 0x07) | ((b[2] >> 4) << 3),
            command: b[2] & 0x0F,
            command_param: b[3],
        }
    }
}

/// Semantic interpretation of the STM note byte, in the tracker's own
/// terms (`TECH.DOC` "Special [BYTE0] contents").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StmNoteKind {
    Note {
        octave: u8,
        semitone: u8,
    },
    /// 251 — one-byte cell, "all bytes 0".
    Empty,
    /// 252 (one-byte cell) / 254 — "note: -0-". The staged example song
    /// uses 252 as its ordinary filler under sustained notes, so the
    /// player treats it as "no note event".
    DashNote,
    /// 253 (one-byte cell) / 255 — "note: ...", no note.
    Dots,
    /// Not produced any more (254 / 255 classify as [`Self::DashNote`] /
    /// [`Self::Dots`]); kept so existing matches stay valid.
    Reserved,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct StmPattern {
    /// `rows[row_index][channel_index]`.
    pub rows: Vec<Vec<StmCell>>,
}

/// Per-instrument sample body after extraction.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct StmSampleBody {
    /// Signed 8-bit PCM (the on-disk unsigned bytes re-centred). Empty
    /// if the instrument has no samples or the file was truncated.
    pub pcm: Vec<i8>,
    pub loop_start: u16,
    pub loop_end: u16,
    pub volume: u8,
    /// C2SPD (see [`StmInstrument::c3_hz`]).
    pub c3_hz: u16,
}

impl StmSampleBody {
    /// Build a body from an instrument record and its raw on-disk bytes
    /// (8-bit unsigned — `TECH.DOC` "Sample storage on disk"). This is
    /// also the route for attaching the external instrument files a
    /// type-1 song refers to.
    pub fn from_unsigned(inst: &StmInstrument, raw: &[u8]) -> Self {
        let take = raw.len().min(inst.length as usize);
        StmSampleBody {
            pcm: raw[..take].iter().map(|&b| (b ^ 0x80) as i8).collect(),
            loop_start: inst.loop_start,
            loop_end: inst.loop_end,
            volume: inst.volume,
            c3_hz: inst.c3_hz,
        }
    }

    /// True if this body carries a forward loop: any loop pair other
    /// than the tracker's `0` / `65535` "no looping" one whose end lies
    /// after its start (user manual §2.3).
    pub fn is_looped(&self) -> bool {
        loop_pair_is_live(self.loop_start, self.loop_end)
            && (self.loop_start as usize) < self.pcm.len()
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
/// informational — Scream Tracker 2.21 writes `"!Scream!"`.
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
/// end of the order table; `Error::invalid` if the mandatory ID byte is
/// wrong.
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
        // rec[12] = 0, rec[13] = disk, rec[14..16] reserved
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
/// unread cells are empty (`StmCell::default()`).
pub fn parse_patterns(header: &StmHeader, bytes: &[u8]) -> Vec<StmPattern> {
    parse_patterns_with_end(header, bytes).0
}

/// Like [`parse_patterns`], also returning the file offset just past
/// the pattern data — where a module's sample bodies start. The offset
/// is `PATTERN_DATA_OFFSET + n * 1024` unless one-byte cells (note byte
/// 251..=253) shortened a pattern, as they do in song files.
pub fn parse_patterns_with_end(header: &StmHeader, bytes: &[u8]) -> (Vec<StmPattern>, usize) {
    let mut patterns = Vec::with_capacity(header.n_patterns as usize);
    let mut cursor = PATTERN_DATA_OFFSET;
    for _ in 0..header.n_patterns as usize {
        let mut rows = Vec::with_capacity(PATTERN_ROWS);
        for _ in 0..PATTERN_ROWS {
            let mut row = Vec::with_capacity(STM_CHANNELS);
            for _ in 0..STM_CHANNELS {
                let cell = match bytes.get(cursor).copied() {
                    // One-byte cells: "last 3 bytes NOT in file".
                    Some(b0 @ 251..=253) => {
                        cursor += 1;
                        StmCell {
                            note_raw: b0,
                            ..StmCell::default()
                        }
                    }
                    Some(b0) if cursor + CELL_BYTES <= bytes.len() => {
                        let cell = StmCell::from_bytes([
                            b0,
                            bytes[cursor + 1],
                            bytes[cursor + 2],
                            bytes[cursor + 3],
                        ]);
                        cursor += CELL_BYTES;
                        cell
                    }
                    _ => {
                        cursor = bytes.len();
                        StmCell::default()
                    }
                };
                row.push(cell);
            }
            rows.push(row);
        }
        patterns.push(StmPattern { rows });
    }
    (patterns, cursor.min(bytes.len()))
}

/// Offset where sample bodies begin when every pattern is stored
/// unpacked (always the case for modules written by ST 2.21).
/// [`extract_samples`] itself walks the pattern data, so it also copes
/// with one-byte cells.
pub fn sample_data_offset(header: &StmHeader) -> usize {
    PATTERN_DATA_OFFSET + header.n_patterns as usize * BYTES_PER_PATTERN
}

/// Extract all 31 instrument sample bodies.
///
/// Bodies follow the pattern data in instrument order, each padded to a
/// 16-byte boundary; the instrument record's last word is not used for
/// placement (it is 0 in every measured file). A song (file type 1)
/// stores no samples, so every body comes back empty — attach external
/// instrument files with [`StmSampleBody::from_unsigned`]. Samples
/// extending past EOF are clamped.
pub fn extract_samples(header: &StmHeader, bytes: &[u8]) -> Vec<StmSampleBody> {
    let mut out = Vec::with_capacity(header.instruments.len());
    let has_bodies = header.file_type != StmFileType::Song;
    let mut cursor = if has_bodies {
        parse_patterns_with_end(header, bytes).1
    } else {
        bytes.len()
    };
    let end = bytes.len();
    for inst in &header.instruments {
        let declared = inst.length as usize;
        let available = end.saturating_sub(cursor);
        let take = declared.min(available);
        out.push(StmSampleBody::from_unsigned(
            inst,
            &bytes[cursor.min(end)..cursor.min(end) + take],
        ));
        if declared > 0 {
            cursor = cursor.saturating_add(declared.next_multiple_of(SAMPLE_ALIGN));
        }
    }
    out
}

/// Duration estimate in microseconds: the leading run of valid orders,
/// 64 rows each, at the header tempo (high nibble = ticks per row, one
/// tick = 20 ms — the staged ST 2.21 trace measures 0.12 s per row at
/// tempo `0x60`). Jumps, breaks and tempo commands are not followed, so
/// treat it as a loose bound.
pub fn estimate_duration_micros(header: &StmHeader) -> i64 {
    let orders = header.song_length().max(1) as i64;
    let ticks = ((header.tempo >> 4) as i64).max(1);
    orders * PATTERN_ROWS as i64 * ticks * 20_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stm_writer::{StmWriter, StmWriterInstrument, StmWriterPattern};

    fn minimal(n_patterns: usize) -> StmWriter {
        let mut w = StmWriter {
            title: "test".into(),
            ..StmWriter::default()
        };
        w.instruments.push(StmWriterInstrument {
            name: "bass".into(),
            pcm: vec![0x90, 0x70, 0xC0, 0x40],
            volume: 64,
            c2spd: 8363,
            ..StmWriterInstrument::default()
        });
        for _ in 0..n_patterns {
            w.patterns.push(StmWriterPattern::new());
        }
        w.orders = vec![0];
        w
    }

    #[test]
    fn is_stm_accepts_minimal_file() {
        assert!(is_stm(&minimal(1).build()));
    }

    #[test]
    fn is_stm_rejects_missing_id_byte() {
        let mut bytes = minimal(1).build();
        bytes[0x1C] = 0;
        assert!(!is_stm(&bytes));
    }

    #[test]
    fn is_stm_rejects_bad_file_type() {
        let mut bytes = minimal(1).build();
        bytes[0x1D] = 99;
        assert!(!is_stm(&bytes));
    }

    #[test]
    fn parse_header_populates_core_fields() {
        let bytes = minimal(2).build();
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.title, "test");
        assert_eq!(h.tracker_name, "!Scream!");
        assert_eq!(h.file_type, StmFileType::Module);
        assert_eq!((h.version_major, h.version_minor), (2, 21));
        assert!(h.is_st221());
        assert_eq!(h.tempo, 0x60);
        assert_eq!(h.n_patterns, 2);
        assert_eq!(h.global_volume, 64);
        assert_eq!(h.instruments.len(), INSTRUMENT_COUNT);
        assert_eq!(h.instruments[0].name, "bass");
        assert_eq!(h.instruments[0].length, 4);
        assert_eq!(h.instruments[0].c3_hz, 8363);
        assert_eq!(h.order.len(), ORDER_TABLE_SIZE);
        assert_eq!(h.order[0], 0);
        assert!(h.order[1..].iter().all(|&o| o == ORDER_UNUSED));
        assert_eq!(h.song_length(), 1);
    }

    #[test]
    fn layout_matches_the_measured_st221_offsets() {
        // `stm-pattern-packing-and-order-table.md` §3a: instruments end
        // at 0x410, a 128-byte order table follows, patterns start at
        // 0x490, and a module's samples follow n * 1024 pattern bytes.
        assert_eq!(
            HEADER_PREFIX_SIZE + INSTRUMENT_COUNT * INSTRUMENT_RECORD_SIZE,
            ORDER_TABLE_OFFSET
        );
        assert_eq!(ORDER_TABLE_OFFSET, 0x410);
        assert_eq!(ORDER_TABLE_OFFSET + ORDER_TABLE_SIZE, PATTERN_DATA_OFFSET);
        assert_eq!(PATTERN_DATA_OFFSET, 0x490);
        let bytes = minimal(3).build();
        assert_eq!(bytes.len(), 0x490 + 3 * 1024 + 4);
        let h = parse_header(&bytes).unwrap();
        assert_eq!(sample_data_offset(&h), 0x490 + 3 * 1024);
    }

    #[test]
    fn parse_header_rejects_bad_id_byte() {
        let mut bytes = minimal(1).build();
        bytes[0x1C] = 0;
        assert!(parse_header(&bytes).is_err());
    }

    #[test]
    fn parse_header_needs_full_order_table() {
        let bytes = minimal(1).build();
        assert!(parse_header(&bytes[..PATTERN_DATA_OFFSET - 1]).is_err());
        assert!(parse_header(&bytes[..PATTERN_DATA_OFFSET]).is_ok());
    }

    #[test]
    fn parse_patterns_returns_empty_rows_by_default() {
        let bytes = minimal(1).build();
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        assert_eq!(pats.len(), 1);
        assert_eq!(pats[0].rows.len(), PATTERN_ROWS);
        assert_eq!(pats[0].rows[0].len(), STM_CHANNELS);
        // The module writer stores empties as `FF 01 80 00`, the cell
        // the staged ST 2.21 probe modules use.
        assert_eq!(&bytes[0x490..0x494], &[0xFF, 0x01, 0x80, 0x00]);
        assert_eq!(pats[0].rows[0][0], StmCell::default());
        assert_eq!(pats[0].rows[0][0].kind(), StmNoteKind::Dots);
        assert!(!pats[0].rows[0][0].has_volume());
    }

    #[test]
    fn cell_bit_fields_follow_the_probe_encoding() {
        // `20 08 41 60` from the staged `STBAUD.STM`: C octave 2,
        // instrument 1, volume 32, command A (1), info 0x60.
        let c = StmCell::from_bytes([0x20, 0x08, 0x41, 0x60]);
        assert_eq!(
            c.kind(),
            StmNoteKind::Note {
                octave: 2,
                semitone: 0
            }
        );
        assert_eq!((c.instrument, c.volume), (1, 32));
        assert_eq!((c.command, c.command_param), (1, 0x60));
        assert_eq!(c.to_bytes(), [0x20, 0x08, 0x41, 0x60]);
        // The probes' `FF 01 82 02`: no note, no volume (65), `B02`.
        let b = StmCell::from_bytes([0xFF, 0x01, 0x82, 0x02]);
        assert_eq!(b.kind(), StmNoteKind::Dots);
        assert_eq!(b.volume, VOLUME_NONE);
        assert_eq!((b.command, b.command_param), (2, 0x02));
    }

    #[test]
    fn cell_kind_classifies_special_values() {
        let make = |n: u8| StmCell {
            note_raw: n,
            ..StmCell::default()
        };
        assert_eq!(make(251).kind(), StmNoteKind::Empty);
        assert_eq!(make(252).kind(), StmNoteKind::DashNote);
        assert_eq!(make(253).kind(), StmNoteKind::Dots);
        assert_eq!(make(254).kind(), StmNoteKind::DashNote);
        assert_eq!(make(255).kind(), StmNoteKind::Dots);
    }

    #[test]
    fn packed_song_cells_are_one_byte() {
        // `stm-pattern-packing-and-order-table.md` §3c: in a song a
        // note byte of 251..=253 is the whole cell.
        let mut w = minimal(2);
        w.song = true;
        w.patterns[1].put(
            63,
            3,
            StmCell {
                note_raw: 0x24,
                instrument: 1,
                volume: 40,
                command: 4,
                command_param: 0x08,
            },
        );
        let bytes = w.build();
        // Pattern 0: 256 one-byte cells; pattern 1: 255 + one 4-byte.
        assert_eq!(bytes.len(), PATTERN_DATA_OFFSET + 256 + 255 + 4);
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.file_type, StmFileType::Song);
        let (pats, end) = parse_patterns_with_end(&h, &bytes);
        assert_eq!(end, bytes.len());
        assert_eq!(pats[0].rows[0][0].kind(), StmNoteKind::DashNote);
        let c = pats[1].rows[63][3];
        assert_eq!((c.note_raw, c.instrument, c.volume), (0x24, 1, 40));
        assert_eq!((c.command, c.command_param), (4, 0x08));
        // A song stores no sample bodies.
        assert!(extract_samples(&h, &bytes).iter().all(|s| s.pcm.is_empty()));
    }

    #[test]
    fn extract_samples_recentres_unsigned_bodies() {
        let bytes = minimal(1).build();
        let h = parse_header(&bytes).unwrap();
        let samples = extract_samples(&h, &bytes);
        assert_eq!(samples.len(), INSTRUMENT_COUNT);
        // On disk 0x90 0x70 0xC0 0x40 (unsigned, 0x80 = zero).
        assert_eq!(samples[0].pcm, vec![0x10, -0x10, 0x40, -0x40]);
        for s in &samples[1..] {
            assert!(s.pcm.is_empty());
        }
    }

    #[test]
    fn sample_bodies_are_padded_to_sixteen_bytes() {
        let mut w = minimal(1);
        w.instruments[0].pcm = vec![0x81; 20];
        w.instruments.push(StmWriterInstrument {
            name: "two".into(),
            pcm: vec![0x7F; 5],
            volume: 64,
            c2spd: 8363,
            ..StmWriterInstrument::default()
        });
        let bytes = w.build();
        // 20 bytes pad to 32; the last body is not padded.
        assert_eq!(bytes.len(), PATTERN_DATA_OFFSET + 1024 + 32 + 5);
        let h = parse_header(&bytes).unwrap();
        let samples = extract_samples(&h, &bytes);
        assert_eq!(samples[0].pcm, vec![1i8; 20]);
        assert_eq!(samples[1].pcm, vec![-1i8; 5]);
    }

    #[test]
    fn extract_samples_handles_truncated_body() {
        let mut bytes = minimal(1).build();
        bytes.truncate(bytes.len() - 2);
        let h = parse_header(&bytes).unwrap();
        let samples = extract_samples(&h, &bytes);
        assert_eq!(samples[0].pcm.len(), 2);
    }

    #[test]
    fn loop_pair_zero_and_65535_means_no_loop() {
        // User manual §2.3: "When the first value is 0 and the last
        // 65535, no looping should be performed."
        let inst = |ls: u16, le: u16| StmInstrument {
            length: 100,
            loop_start: ls,
            loop_end: le,
            ..StmInstrument::default()
        };
        assert!(!inst(0, 0xFFFF).is_looped());
        assert!(!inst(0, 0).is_looped());
        assert!(!inst(50, 50).is_looped());
        assert!(inst(10, 90).is_looped());
        let body = StmSampleBody::from_unsigned(&inst(10, 90), &[0x80; 100]);
        assert!(body.is_looped());
        use crate::mixer::SampleSource;
        assert_eq!((body.loop_start(), body.loop_end()), (10, 90));
    }

    #[test]
    fn song_file_type_parses_and_renders_without_samples() {
        let mut w = minimal(1);
        w.song = true;
        let bytes = w.build();
        assert!(is_stm(&bytes), "type-1 song files are valid STM");
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.file_type, StmFileType::Song);
        let patterns = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        assert!(samples.iter().all(|s| s.pcm.is_empty()));
        let mut player = crate::stm_player::StmPlayerState::new(&h, samples, patterns, 44_100);
        let mut buf = vec![0i16; 2048 * 2];
        let produced = player.render(&mut buf);
        assert_eq!(produced, 2048);
        assert!(buf.iter().all(|&s| s == 0), "no samples => silence");
    }

    #[test]
    fn duration_estimate_follows_the_traced_row_time() {
        // 3 orders x 64 rows x 6 ticks x 20 ms (0.12 s per row at 0x60).
        let mut w = minimal(3);
        w.orders = vec![0, 1, 2];
        let h = parse_header(&w.build()).unwrap();
        assert_eq!(estimate_duration_micros(&h), 3 * 64 * 120_000);
    }
}
