//! Impulse Tracker (`.it`) module parser.
//!
//! Layout source: `docs/audio/trackers/it/ImpulseTracker-it.txt` (the
//! tracker author's own format text; the `.html` mirror in the same
//! directory is the identical document). Every offset, flag bit and
//! table in this module is transcribed from that text — section names
//! below quote its headings ("Impulse Header Layout", "Impulse Sample
//! Format", "Impulse Instrument Format", "Impulse Pattern Format",
//! "Internal Tables").
//!
//! File shape (§"Impulse Header Layout"):
//!
//! ```text
//! 0000  'IMPM' + song name (26, NUL-included)
//! 001E  PHiligt (pattern-row highlight, editor only)
//! 0020  OrdNum InsNum SmpNum PatNum Cwt Cmwt Flags Special   (8 × u16)
//! 0030  GV MV IS IT Sep PWD  MsgLgth(u16)  MsgOffset(u32)  reserved(u32)
//! 0040  channel pan, 64 bytes
//! 0080  channel volume, 64 bytes
//! 00C0  orders[OrdNum]
//! ....  u32 instrument offsets[InsNum]
//! ....  u32 sample-header offsets[SmpNum]
//! ....  u32 pattern offsets[PatNum]   (0 = empty 64-row pattern)
//! ```
//!
//! Instruments, sample headers and patterns are then reached through
//! the offset tables; sample bodies through each sample header's
//! `SamplePointer`.

use oxideav_core::{Error, Result};

/// The four magic bytes at offset 0 of every Impulse Tracker module.
pub const IT_MAGIC: &[u8; 4] = b"IMPM";
/// Magic at offset 0 of every instrument header.
pub const IT_INSTRUMENT_MAGIC: &[u8; 4] = b"IMPI";
/// Magic at offset 0 of every sample header.
pub const IT_SAMPLE_MAGIC: &[u8; 4] = b"IMPS";

/// Fixed header size before the variable-length order table
/// (`00C0h`).
pub const IT_HEADER_FIXED_SIZE: usize = 0xC0;
/// Number of per-channel pan / volume slots in the header.
pub const IT_MAX_CHANNELS: usize = 64;
/// Size of one sample header (`IMPS` … `ViT`).
pub const IT_SAMPLE_HEADER_SIZE: usize = 0x50;
/// Bytes written per instrument (both the old and the new layout:
/// "Total length of an instrument is 547 bytes, but 554 bytes are
/// written, just to simplify the loading of the old format").
pub const IT_INSTRUMENT_SIZE: usize = 554;
/// Pattern header: `Length(u16) Rows(u16) x x x x`.
pub const IT_PATTERN_HEADER_SIZE: usize = 8;
/// Row count of a pattern whose offset is 0 ("assumed to be a 64 row
/// empty pattern").
pub const IT_EMPTY_PATTERN_ROWS: u16 = 64;
/// Highest playable note (`0->119 (C-0 -> B-9)`).
pub const IT_MAX_NOTE: u8 = 119;
/// Pattern note value for "note off" (`255 = note off`).
pub const IT_NOTE_OFF: u8 = 255;
/// Pattern note value for "note cut" (`254 = notecut`).
pub const IT_NOTE_CUT: u8 = 254;
/// Order-list entry: end of song (`255 = "---"`).
pub const IT_ORDER_END: u8 = 255;
/// Order-list entry: skip (`254 = "+++"`).
pub const IT_ORDER_SKIP: u8 = 254;
/// Instrument-format threshold: `Cmwt < 200h` means the old (1.x)
/// instrument layout.
pub const IT_CMWT_NEW_INSTRUMENTS: u16 = 0x200;

// ---- Header `Flags` bits ---------------------------------------------------

/// Bit 0: On = Stereo, Off = Mono.
pub const IT_FLAG_STEREO: u16 = 1 << 0;
/// Bit 1: Vol0MixOptimizations (redundant v1.04+).
pub const IT_FLAG_VOL0_OPTIMISATIONS: u16 = 1 << 1;
/// Bit 2: On = Use instruments, Off = Use samples.
pub const IT_FLAG_INSTRUMENTS: u16 = 1 << 2;
/// Bit 3: On = Linear slides, Off = Amiga slides.
pub const IT_FLAG_LINEAR_SLIDES: u16 = 1 << 3;
/// Bit 4: On = Old Effects, Off = IT Effects.
pub const IT_FLAG_OLD_EFFECTS: u16 = 1 << 4;
/// Bit 5: On = Link Effect G's memory with Effect E/F (+ Gxx with an
/// instrument retriggers envelopes).
pub const IT_FLAG_COMPAT_GXX: u16 = 1 << 5;
/// Bit 6: Use MIDI pitch controller, pitch depth given by PWD.
pub const IT_FLAG_MIDI_PITCH: u16 = 1 << 6;
/// Bit 7: Request embedded MIDI configuration.
pub const IT_FLAG_MIDI_CONFIG_REQUEST: u16 = 1 << 7;

// ---- Header `Special` bits -------------------------------------------------

/// Bit 0: On = song message attached.
pub const IT_SPECIAL_MESSAGE: u16 = 1 << 0;
/// Bit 3: MIDI configuration embedded.
pub const IT_SPECIAL_MIDI_CONFIG: u16 = 1 << 3;
/// Upper bound on the song message ("v1.04+ of IT may have song
/// messages of up to 8000 bytes included").
pub const IT_MESSAGE_MAX_LEN: usize = 8000;

/// Channel-pan byte value meaning "Surround sound".
pub const IT_PAN_SURROUND: u8 = 100;
/// Channel-pan bit meaning "disabled channel (notes will not be played,
/// but note that effects in muted channels are still processed)".
pub const IT_PAN_DISABLED: u8 = 128;

/// Parsed `IMPM` header (everything before the instrument headers).
#[derive(Clone, Debug)]
pub struct ItHeader {
    /// Song name (26 bytes, NUL-trimmed).
    pub song_name: String,
    /// Pattern-row highlight bytes (minor / major), editor information
    /// only.
    pub pattern_highlight: [u8; 2],
    /// `Cwt`: "Created with tracker. Impulse Tracker y.xx = 0yxxh".
    pub created_with: u16,
    /// `Cmwt`: "Compatible with tracker with version greater than value
    /// (ie. format version)".
    pub compatible_with: u16,
    /// Raw `Flags` word — see the `IT_FLAG_*` constants.
    pub flags: u16,
    /// Raw `Special` word — see the `IT_SPECIAL_*` constants.
    pub special: u16,
    /// `GV`: global volume, `0->128`.
    pub global_volume: u8,
    /// `MV`: mix volume, `0->128`.
    pub mix_volume: u8,
    /// `IS`: initial speed (ticks per row).
    pub initial_speed: u8,
    /// `IT`: initial tempo.
    pub initial_tempo: u8,
    /// `Sep`: panning separation `0->128`.
    pub pan_separation: u8,
    /// `PWD`: pitch wheel depth for MIDI controllers.
    pub pitch_wheel_depth: u8,
    /// Song message length as stored (`MsgLgth`).
    pub message_length: u16,
    /// Song message file offset (`Message Offset`).
    pub message_offset: u32,
    /// Raw per-channel pan bytes (`0..64`, `100` = surround,
    /// `+128` = disabled).
    pub channel_pan: [u8; IT_MAX_CHANNELS],
    /// Per-channel volume `0..64`.
    pub channel_volume: [u8; IT_MAX_CHANNELS],
    /// Order list, `OrdNum` entries (`0..199` patterns, `254` skip,
    /// `255` end).
    pub orders: Vec<u8>,
    /// Instrument header offsets (`InsNum` entries).
    pub instrument_offsets: Vec<u32>,
    /// Sample header offsets (`SmpNum` entries).
    pub sample_offsets: Vec<u32>,
    /// Pattern offsets (`PatNum` entries; `0` = empty pattern).
    pub pattern_offsets: Vec<u32>,
}

impl Default for ItHeader {
    fn default() -> Self {
        ItHeader {
            song_name: String::new(),
            pattern_highlight: [0; 2],
            created_with: 0,
            compatible_with: 0,
            flags: 0,
            special: 0,
            global_volume: 128,
            mix_volume: 128,
            initial_speed: 6,
            initial_tempo: 125,
            pan_separation: 128,
            pitch_wheel_depth: 0,
            message_length: 0,
            message_offset: 0,
            channel_pan: [32; IT_MAX_CHANNELS],
            channel_volume: [64; IT_MAX_CHANNELS],
            orders: Vec::new(),
            instrument_offsets: Vec::new(),
            sample_offsets: Vec::new(),
            pattern_offsets: Vec::new(),
        }
    }
}

impl ItHeader {
    /// Bit 0 of `Flags`: stereo output requested.
    pub fn is_stereo(&self) -> bool {
        self.flags & IT_FLAG_STEREO != 0
    }
    /// Bit 2 of `Flags`: the song uses instruments rather than samples.
    pub fn uses_instruments(&self) -> bool {
        self.flags & IT_FLAG_INSTRUMENTS != 0
    }
    /// Bit 3 of `Flags`: linear (vs Amiga) frequency slides.
    pub fn linear_slides(&self) -> bool {
        self.flags & IT_FLAG_LINEAR_SLIDES != 0
    }
    /// Bit 4 of `Flags`: "Old Effects" mode.
    pub fn old_effects(&self) -> bool {
        self.flags & IT_FLAG_OLD_EFFECTS != 0
    }
    /// Bit 5 of `Flags`: `Gxx` shares memory with `Exx`/`Fxx`.
    pub fn compatible_gxx(&self) -> bool {
        self.flags & IT_FLAG_COMPAT_GXX != 0
    }
    /// Bit 0 of `Special`: a song message is attached.
    pub fn has_message(&self) -> bool {
        self.special & IT_SPECIAL_MESSAGE != 0
    }
    /// True when the instrument headers use the old (1.x) layout
    /// (`cmwt < 200h`).
    pub fn old_instrument_format(&self) -> bool {
        self.compatible_with < IT_CMWT_NEW_INSTRUMENTS
    }
    /// Channel `ch` is muted (`+128` in the pan byte).
    pub fn channel_disabled(&self, ch: usize) -> bool {
        self.channel_pan
            .get(ch)
            .is_some_and(|&p| p & IT_PAN_DISABLED != 0)
    }
    /// Initial pan for channel `ch` with the disabled bit stripped:
    /// `0..=64`, or [`IT_PAN_SURROUND`].
    pub fn channel_initial_pan(&self, ch: usize) -> u8 {
        self.channel_pan.get(ch).map_or(32, |&p| p & 0x7F)
    }
    /// Number of "real" pattern slots in the order list (entries below
    /// [`IT_ORDER_SKIP`]).
    pub fn playable_order_count(&self) -> usize {
        self.orders.iter().filter(|&&o| o < IT_ORDER_SKIP).count()
    }
    /// Decoded "Created with" version as `(major, minor)` per
    /// "Impulse Tracker y.xx = 0yxxh".
    pub fn created_with_version(&self) -> (u8, u8) {
        (
            (self.created_with >> 8) as u8,
            (self.created_with & 0xFF) as u8,
        )
    }
    /// Byte offset of the end of the pattern-offset table = the first
    /// byte past the fixed-plus-tables header.
    pub fn tables_end(&self) -> usize {
        IT_HEADER_FIXED_SIZE
            + self.orders.len()
            + 4 * (self.instrument_offsets.len()
                + self.sample_offsets.len()
                + self.pattern_offsets.len())
    }
}

/// Cheap sniff: does `bytes` start with the `IMPM` magic?
pub fn is_it(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[0..4] == IT_MAGIC
}

pub(crate) fn read_u16_le(bytes: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([bytes[off], bytes[off + 1]])
}

pub(crate) fn read_u32_le(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

/// NUL-terminated fixed-width string → owned `String` (lossy, trimmed).
pub(crate) fn trim_fixed_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end])
        .trim_end()
        .to_string()
}

/// Parse the `IMPM` header + order list + the three offset tables.
///
/// Fails with `InvalidData` on a missing magic or a file too short to
/// hold the counts it declares.
pub fn parse_header(bytes: &[u8]) -> Result<ItHeader> {
    if !is_it(bytes) {
        return Err(Error::invalid("IT: missing 'IMPM' magic at offset 0"));
    }
    if bytes.len() < IT_HEADER_FIXED_SIZE {
        return Err(Error::invalid("IT: file shorter than the 0xC0-byte header"));
    }
    let ord_num = read_u16_le(bytes, 0x20) as usize;
    let ins_num = read_u16_le(bytes, 0x22) as usize;
    let smp_num = read_u16_le(bytes, 0x24) as usize;
    let pat_num = read_u16_le(bytes, 0x26) as usize;
    let tables_len = ord_num + 4 * (ins_num + smp_num + pat_num);
    if bytes.len() < IT_HEADER_FIXED_SIZE + tables_len {
        return Err(Error::invalid(
            "IT: file too short for the declared order / offset tables",
        ));
    }

    let mut channel_pan = [0u8; IT_MAX_CHANNELS];
    channel_pan.copy_from_slice(&bytes[0x40..0x80]);
    let mut channel_volume = [0u8; IT_MAX_CHANNELS];
    channel_volume.copy_from_slice(&bytes[0x80..0xC0]);

    let mut cur = IT_HEADER_FIXED_SIZE;
    let orders = bytes[cur..cur + ord_num].to_vec();
    cur += ord_num;
    let mut read_table = |n: usize| -> Vec<u32> {
        let v = (0..n).map(|i| read_u32_le(bytes, cur + 4 * i)).collect();
        cur += 4 * n;
        v
    };
    let instrument_offsets = read_table(ins_num);
    let sample_offsets = read_table(smp_num);
    let pattern_offsets = read_table(pat_num);

    Ok(ItHeader {
        song_name: trim_fixed_string(&bytes[4..0x1E]),
        pattern_highlight: [bytes[0x1E], bytes[0x1F]],
        created_with: read_u16_le(bytes, 0x28),
        compatible_with: read_u16_le(bytes, 0x2A),
        flags: read_u16_le(bytes, 0x2C),
        special: read_u16_le(bytes, 0x2E),
        global_volume: bytes[0x30],
        mix_volume: bytes[0x31],
        initial_speed: bytes[0x32],
        initial_tempo: bytes[0x33],
        pan_separation: bytes[0x34],
        pitch_wheel_depth: bytes[0x35],
        message_length: read_u16_le(bytes, 0x36),
        message_offset: read_u32_le(bytes, 0x38),
        channel_pan,
        channel_volume,
        orders,
        instrument_offsets,
        sample_offsets,
        pattern_offsets,
    })
}

/// Extract the song message, if `Special` bit 0 is set and the offset /
/// length land inside the file.
///
/// Per §"Special": "Stored at offset given by 'Message Offset' field.
/// Length = MsgLgth. NewLine = 0Dh (13 dec). EndOfMsg = 0". The `0Dh`
/// line breaks are rewritten to `\n`; the message is cut at the first
/// NUL and capped at [`IT_MESSAGE_MAX_LEN`].
pub fn extract_message(header: &ItHeader, bytes: &[u8]) -> Option<String> {
    if !header.has_message() {
        return None;
    }
    let start = header.message_offset as usize;
    let len = (header.message_length as usize).min(IT_MESSAGE_MAX_LEN);
    if len == 0 || start >= bytes.len() {
        return None;
    }
    let end = start.saturating_add(len).min(bytes.len());
    let raw = &bytes[start..end];
    let raw = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())];
    let text: String = String::from_utf8_lossy(raw)
        .chars()
        .map(|c| if c == '\r' { '\n' } else { c })
        .collect();
    Some(text)
}

// ============================================================================
// Samples — §"Impulse Sample Format"
// ============================================================================

// ---- Sample `Flg` bits -----------------------------------------------------

/// Bit 0. On = sample associated with header.
pub const IT_SMP_HAS_SAMPLE: u8 = 1 << 0;
/// Bit 1. On = 16 bit, Off = 8 bit.
pub const IT_SMP_16BIT: u8 = 1 << 1;
/// Bit 2. On = stereo, Off = mono ("Stereo samples not supported yet").
pub const IT_SMP_STEREO: u8 = 1 << 2;
/// Bit 3. On = compressed samples.
pub const IT_SMP_COMPRESSED: u8 = 1 << 3;
/// Bit 4. On = Use loop.
pub const IT_SMP_LOOP: u8 = 1 << 4;
/// Bit 5. On = Use sustain loop.
pub const IT_SMP_SUSTAIN_LOOP: u8 = 1 << 5;
/// Bit 6. On = Ping Pong loop, Off = Forwards loop.
pub const IT_SMP_PINGPONG_LOOP: u8 = 1 << 6;
/// Bit 7. On = Ping Pong Sustain loop, Off = Forwards Sustain loop.
pub const IT_SMP_PINGPONG_SUSTAIN: u8 = 1 << 7;

// ---- Sample `Cvt` (Convert) bits -------------------------------------------

/// Bit 0: Off = unsigned (IT 2.01 and below), On = signed (2.02+).
pub const IT_CVT_SIGNED: u8 = 1 << 0;
/// Bit 1: On = Motorola hi-lo byte order for 16-bit samples.
pub const IT_CVT_BIG_ENDIAN: u8 = 1 << 1;
/// Bit 2: On = samples are stored as delta values.
pub const IT_CVT_DELTA: u8 = 1 << 2;
/// Bit 3: On = byte delta values (for PTM loader).
pub const IT_CVT_BYTE_DELTA: u8 = 1 << 3;
/// Bit 4: On = TX-Wave 12-bit values.
pub const IT_CVT_TX_WAVE_12BIT: u8 = 1 << 4;
/// Bit 5: On = Left/Right/All Stereo prompt.
pub const IT_CVT_STEREO_PROMPT: u8 = 1 << 5;

/// Sample `DfP`: "Bits 0->6 = Pan value, Bit 7 ON to USE".
pub const IT_SMP_PAN_USE: u8 = 1 << 7;

/// Sample-vibrato waveform (`ViT`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ItVibratoWave {
    #[default]
    Sine = 0,
    RampDown = 1,
    Square = 2,
    /// "Random (speed is irrelevant)".
    Random = 3,
}

impl ItVibratoWave {
    pub fn from_byte(b: u8) -> Self {
        match b & 3 {
            1 => ItVibratoWave::RampDown,
            2 => ItVibratoWave::Square,
            3 => ItVibratoWave::Random,
            _ => ItVibratoWave::Sine,
        }
    }
}

/// One `IMPS` sample header plus its decoded body.
#[derive(Clone, Debug, Default)]
pub struct ItSample {
    /// DOS filename (12 bytes).
    pub filename: String,
    /// Sample name (26 bytes).
    pub name: String,
    /// `GvL`: global volume for the sample, `0->64`.
    pub global_volume: u8,
    /// Raw `Flg` byte — see the `IT_SMP_*` constants.
    pub flags: u8,
    /// `Vol`: default volume, `0->64`.
    pub default_volume: u8,
    /// Raw `Cvt` byte — see the `IT_CVT_*` constants.
    pub convert: u8,
    /// Raw `DfP` byte: bits 0-6 pan, bit 7 = use.
    pub default_pan: u8,
    /// `Length`: number of samples (frames), not bytes.
    pub length: u32,
    /// `Loop Begin` (frames).
    pub loop_begin: u32,
    /// `Loop End`: "Sample no. AFTER end of loop".
    pub loop_end: u32,
    /// `C5Speed`: rate in Hz at C-5 (`0->9999999`).
    pub c5_speed: u32,
    /// `SusLoop Begin` (frames).
    pub sustain_begin: u32,
    /// `SusLoop End`: "Sample no. AFTER end of sustain loop".
    pub sustain_end: u32,
    /// `SamplePointer`: file offset of the body.
    pub sample_pointer: u32,
    /// `ViS`: vibrato speed `0->64`.
    pub vibrato_speed: u8,
    /// `ViD`: vibrato depth `0->64`.
    pub vibrato_depth: u8,
    /// `ViR`: vibrato rate ("rate at which vibrato is applied", the
    /// per-tick sweep increment) `0->64`.
    pub vibrato_rate: u8,
    /// `ViT`: vibrato waveform.
    pub vibrato_wave: ItVibratoWave,
    /// Decoded body, one `i16` per frame (8-bit bodies are scaled by
    /// 256). Empty when the header has no body, the body is compressed
    /// (see [`ItSample::compressed`]) or the file is truncated before
    /// the body.
    pub pcm: Vec<i16>,
    /// True when the file was cut short of the declared body (the body
    /// was clamped to what was present).
    pub truncated: bool,
}

impl ItSample {
    pub fn has_sample(&self) -> bool {
        self.flags & IT_SMP_HAS_SAMPLE != 0
    }
    pub fn is_16_bit(&self) -> bool {
        self.flags & IT_SMP_16BIT != 0
    }
    pub fn is_stereo(&self) -> bool {
        self.flags & IT_SMP_STEREO != 0
    }
    /// Bit 3 of `Flg`. The staged format text names the flag but does
    /// not describe the compression scheme, so compressed bodies are
    /// left empty (`pcm` is empty) rather than guessed at.
    pub fn compressed(&self) -> bool {
        self.flags & IT_SMP_COMPRESSED != 0
    }
    pub fn is_signed(&self) -> bool {
        self.convert & IT_CVT_SIGNED != 0
    }
    /// Sample-level default pan, `Some(0..=64)` iff bit 7 of `DfP`.
    pub fn pan(&self) -> Option<u8> {
        if self.default_pan & IT_SMP_PAN_USE != 0 {
            Some((self.default_pan & 0x7F).min(64))
        } else {
            None
        }
    }
    /// Frames available in the decoded body.
    pub fn frames(&self) -> usize {
        self.pcm.len()
    }
    /// Validated normal loop as `(begin, end, ping_pong)` — `None` when
    /// bit 4 is clear or the stored bounds do not describe a non-empty
    /// region inside the decoded body.
    pub fn normal_loop(&self) -> Option<(usize, usize, bool)> {
        if self.flags & IT_SMP_LOOP == 0 {
            return None;
        }
        self.validated_loop(
            self.loop_begin,
            self.loop_end,
            self.flags & IT_SMP_PINGPONG_LOOP != 0,
        )
    }
    /// Validated sustain loop as `(begin, end, ping_pong)` — `None` when
    /// bit 5 is clear or the bounds are unusable.
    pub fn sustain_loop(&self) -> Option<(usize, usize, bool)> {
        if self.flags & IT_SMP_SUSTAIN_LOOP == 0 {
            return None;
        }
        self.validated_loop(
            self.sustain_begin,
            self.sustain_end,
            self.flags & IT_SMP_PINGPONG_SUSTAIN != 0,
        )
    }
    fn validated_loop(&self, begin: u32, end: u32, pp: bool) -> Option<(usize, usize, bool)> {
        let len = self.pcm.len();
        let (b, e) = (begin as usize, end as usize);
        if e > len || b >= e {
            return None;
        }
        Some((b, e, pp))
    }
}

/// [`crate::mixer::SampleSource`] over an [`ItSample`] with the loop
/// selection resolved: `sustain = true` plays the sustain loop (a note
/// that has not been released), `false` the normal loop. Either falls
/// back to the other / to one-shot when its loop is absent.
#[derive(Clone, Copy, Debug)]
pub struct ItLoopView<'a> {
    pub sample: &'a ItSample,
    pub sustain: bool,
}

impl ItLoopView<'_> {
    fn resolved(&self) -> (usize, usize, crate::mixer::LoopKind) {
        use crate::mixer::LoopKind;
        let pick = if self.sustain {
            self.sample
                .sustain_loop()
                .or_else(|| self.sample.normal_loop())
        } else {
            self.sample.normal_loop()
        };
        match pick {
            Some((b, e, true)) => (b, e, LoopKind::PingPong),
            Some((b, e, false)) => (b, e, LoopKind::Forward),
            None => (0, self.sample.pcm.len(), LoopKind::None),
        }
    }
}

impl crate::mixer::SampleSource for ItLoopView<'_> {
    fn len(&self) -> usize {
        self.sample.pcm.len()
    }
    fn loop_start(&self) -> usize {
        self.resolved().0
    }
    fn loop_end(&self) -> usize {
        self.resolved().1
    }
    fn loop_kind(&self) -> crate::mixer::LoopKind {
        self.resolved().2
    }
    fn at(&self, idx: usize) -> f32 {
        self.sample.pcm.get(idx).copied().unwrap_or(0) as f32 / 32768.0
    }
}

/// Parse one sample header at `off`. Does not read the body.
pub fn parse_sample_header(bytes: &[u8], off: usize) -> Result<ItSample> {
    let end = off.checked_add(IT_SAMPLE_HEADER_SIZE);
    let Some(end) = end.filter(|&e| e <= bytes.len()) else {
        return Err(Error::invalid("IT: sample header past end of file"));
    };
    let h = &bytes[off..end];
    if &h[0..4] != IT_SAMPLE_MAGIC {
        return Err(Error::invalid("IT: sample header missing 'IMPS' magic"));
    }
    Ok(ItSample {
        filename: trim_fixed_string(&h[4..0x10]),
        global_volume: h[0x11].min(64),
        flags: h[0x12],
        default_volume: h[0x13].min(64),
        name: trim_fixed_string(&h[0x14..0x2E]),
        convert: h[0x2E],
        default_pan: h[0x2F],
        length: read_u32_le(h, 0x30),
        loop_begin: read_u32_le(h, 0x34),
        loop_end: read_u32_le(h, 0x38),
        c5_speed: read_u32_le(h, 0x3C),
        sustain_begin: read_u32_le(h, 0x40),
        sustain_end: read_u32_le(h, 0x44),
        sample_pointer: read_u32_le(h, 0x48),
        vibrato_speed: h[0x4C].min(64),
        vibrato_depth: h[0x4D].min(64),
        vibrato_rate: h[0x4E].min(64),
        vibrato_wave: ItVibratoWave::from_byte(h[0x4F]),
        pcm: Vec::new(),
        truncated: false,
    })
}

/// Decode a sample body per the `Flg` / `Cvt` bits into `sample.pcm`.
///
/// Covered: 8-bit and 16-bit, unsigned (`Cvt` bit 0 off) and signed,
/// Intel and Motorola 16-bit byte order (bit 1), delta storage (bit 2,
/// running sum in the sample's own width). Bit 3 (PTM byte-delta) and
/// bit 4 (TX-Wave 12-bit) are "used internally for the loading of
/// alternative formats" and are not IT-file storage modes; they are
/// treated as plain PCM. Stereo bodies are decoded as the first
/// `Length` frames of the stored data (the staged text declares stereo
/// "not supported yet" and gives no channel layout). Compressed bodies
/// are left empty.
pub fn decode_sample_body(sample: &mut ItSample, bytes: &[u8]) {
    sample.pcm.clear();
    sample.truncated = false;
    if !sample.has_sample() || sample.length == 0 || sample.compressed() {
        return;
    }
    let start = sample.sample_pointer as usize;
    if start >= bytes.len() {
        sample.truncated = true;
        return;
    }
    let width = if sample.is_16_bit() { 2 } else { 1 };
    let want = sample.length as usize;
    let avail = (bytes.len() - start) / width;
    let n = want.min(avail);
    if n < want {
        sample.truncated = true;
    }
    let body = &bytes[start..start + n * width];
    let signed = sample.is_signed();
    let delta = sample.convert & IT_CVT_DELTA != 0;
    let mut out = Vec::with_capacity(n);
    if width == 1 {
        let mut acc: u8 = 0;
        for &b in body {
            let v = if delta {
                acc = acc.wrapping_add(b);
                acc
            } else {
                b
            };
            let s = if signed { v as i8 } else { (v ^ 0x80) as i8 };
            out.push((s as i16) << 8);
        }
    } else {
        let be = sample.convert & IT_CVT_BIG_ENDIAN != 0;
        let mut acc: u16 = 0;
        for w in body.chunks_exact(2) {
            let raw = if be {
                u16::from_be_bytes([w[0], w[1]])
            } else {
                u16::from_le_bytes([w[0], w[1]])
            };
            let v = if delta {
                acc = acc.wrapping_add(raw);
                acc
            } else {
                raw
            };
            out.push(if signed {
                v as i16
            } else {
                (v ^ 0x8000) as i16
            });
        }
    }
    sample.pcm = out;
}

/// Parse every sample header named by the offset table and decode its
/// body. An offset of 0 or an unreadable header yields an empty
/// placeholder so that sample numbering (1-based in patterns) stays
/// aligned with the table.
pub fn parse_samples(header: &ItHeader, bytes: &[u8]) -> Vec<ItSample> {
    header
        .sample_offsets
        .iter()
        .map(|&off| {
            if off == 0 {
                return ItSample::default();
            }
            match parse_sample_header(bytes, off as usize) {
                Ok(mut s) => {
                    decode_sample_body(&mut s, bytes);
                    s
                }
                Err(_) => ItSample::default(),
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Hand-assembled minimal header: `IMPM`, name, counts, flags, an
    /// order list and offset tables. Returns the byte vector; callers
    /// append instrument / sample / pattern blocks and patch the offset
    /// tables themselves.
    pub(crate) fn build_header(
        name: &str,
        orders: &[u8],
        n_ins: usize,
        n_smp: usize,
        n_pat: usize,
        flags: u16,
    ) -> Vec<u8> {
        let mut out = vec![0u8; IT_HEADER_FIXED_SIZE];
        out[0..4].copy_from_slice(IT_MAGIC);
        let n = name.as_bytes();
        out[4..4 + n.len().min(25)].copy_from_slice(&n[..n.len().min(25)]);
        out[0x1E] = 4;
        out[0x1F] = 16;
        out[0x20..0x22].copy_from_slice(&(orders.len() as u16).to_le_bytes());
        out[0x22..0x24].copy_from_slice(&(n_ins as u16).to_le_bytes());
        out[0x24..0x26].copy_from_slice(&(n_smp as u16).to_le_bytes());
        out[0x26..0x28].copy_from_slice(&(n_pat as u16).to_le_bytes());
        out[0x28..0x2A].copy_from_slice(&0x0214u16.to_le_bytes()); // Cwt 2.14
        out[0x2A..0x2C].copy_from_slice(&0x0200u16.to_le_bytes()); // Cmwt 2.00
        out[0x2C..0x2E].copy_from_slice(&flags.to_le_bytes());
        out[0x2E..0x30].copy_from_slice(&0u16.to_le_bytes());
        out[0x30] = 128; // GV
        out[0x31] = 48; // MV
        out[0x32] = 6; // IS
        out[0x33] = 125; // IT
        out[0x34] = 128; // Sep
        for i in 0..IT_MAX_CHANNELS {
            out[0x40 + i] = 32;
            out[0x80 + i] = 64;
        }
        out.extend_from_slice(orders);
        out.extend(std::iter::repeat_n(0u8, 4 * (n_ins + n_smp + n_pat)));
        out
    }

    /// Patch the `idx`-th entry of one of the three offset tables.
    pub(crate) fn set_offset(buf: &mut [u8], table_base: usize, idx: usize, value: u32) {
        let at = table_base + 4 * idx;
        buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Serialise a sample header (body pointer patched by the caller).
    pub(crate) fn build_sample_header(
        name: &str,
        flags: u8,
        cvt: u8,
        length: u32,
        loops: (u32, u32, u32, u32),
        c5: u32,
        pointer: u32,
    ) -> Vec<u8> {
        let mut h = vec![0u8; IT_SAMPLE_HEADER_SIZE];
        h[0..4].copy_from_slice(IT_SAMPLE_MAGIC);
        h[4..12].copy_from_slice(b"SMP00000");
        h[0x11] = 64; // GvL
        h[0x12] = flags;
        h[0x13] = 64; // Vol
        let n = name.as_bytes();
        h[0x14..0x14 + n.len().min(25)].copy_from_slice(&n[..n.len().min(25)]);
        h[0x2E] = cvt;
        h[0x2F] = 32; // DfP, unused
        h[0x30..0x34].copy_from_slice(&length.to_le_bytes());
        h[0x34..0x38].copy_from_slice(&loops.0.to_le_bytes());
        h[0x38..0x3C].copy_from_slice(&loops.1.to_le_bytes());
        h[0x3C..0x40].copy_from_slice(&c5.to_le_bytes());
        h[0x40..0x44].copy_from_slice(&loops.2.to_le_bytes());
        h[0x44..0x48].copy_from_slice(&loops.3.to_le_bytes());
        h[0x48..0x4C].copy_from_slice(&pointer.to_le_bytes());
        h
    }

    fn sample_from(flags: u8, cvt: u8, length: u32, body: &[u8]) -> ItSample {
        let mut file = build_sample_header(
            "s",
            flags,
            cvt,
            length,
            (0, 0, 0, 0),
            8363,
            IT_SAMPLE_HEADER_SIZE as u32,
        );
        file.extend_from_slice(body);
        let mut s = parse_sample_header(&file, 0).unwrap();
        decode_sample_body(&mut s, &file);
        s
    }

    #[test]
    fn sample_header_fields_round_trip() {
        let mut file = build_sample_header(
            "kick",
            IT_SMP_HAS_SAMPLE
                | IT_SMP_16BIT
                | IT_SMP_LOOP
                | IT_SMP_SUSTAIN_LOOP
                | IT_SMP_PINGPONG_SUSTAIN,
            IT_CVT_SIGNED,
            100,
            (10, 20, 30, 40),
            22050,
            0x1000,
        );
        file[0x2F] = 0x80 | 10; // DfP: use, pan 10
        file[0x4C] = 5;
        file[0x4D] = 6;
        file[0x4E] = 7;
        file[0x4F] = 2;
        let s = parse_sample_header(&file, 0).unwrap();
        assert_eq!(s.name, "kick");
        assert_eq!(s.filename, "SMP00000");
        assert!(s.has_sample() && s.is_16_bit() && s.is_signed());
        assert!(!s.is_stereo() && !s.compressed());
        assert_eq!((s.length, s.loop_begin, s.loop_end), (100, 10, 20));
        assert_eq!((s.sustain_begin, s.sustain_end), (30, 40));
        assert_eq!(s.c5_speed, 22050);
        assert_eq!(s.sample_pointer, 0x1000);
        assert_eq!(s.pan(), Some(10));
        assert_eq!(
            (s.vibrato_speed, s.vibrato_depth, s.vibrato_rate),
            (5, 6, 7)
        );
        assert_eq!(s.vibrato_wave, ItVibratoWave::Square);
        // No body decoded (pointer past EOF) → loops are unusable.
        assert!(s.pcm.is_empty());
        assert_eq!(s.normal_loop(), None);
        assert_eq!(s.sustain_loop(), None);
    }

    #[test]
    fn sample_header_rejects_bad_magic_and_short_buffer() {
        let mut file = build_sample_header("s", 0, 0, 0, (0, 0, 0, 0), 8363, 0);
        file[0] = b'X';
        assert!(parse_sample_header(&file, 0).is_err());
        let file = build_sample_header("s", 0, 0, 0, (0, 0, 0, 0), 8363, 0);
        assert!(parse_sample_header(&file[..IT_SAMPLE_HEADER_SIZE - 1], 0).is_err());
        assert!(parse_sample_header(&file, usize::MAX - 3).is_err());
    }

    #[test]
    fn decodes_8bit_unsigned_and_signed() {
        let u = sample_from(IT_SMP_HAS_SAMPLE, 0, 4, &[0x80, 0xFF, 0x00, 0x7F]);
        assert_eq!(u.pcm, [0, 127 << 8, -128 << 8, -1 << 8]);
        let s = sample_from(
            IT_SMP_HAS_SAMPLE,
            IT_CVT_SIGNED,
            4,
            &[0x00, 0x7F, 0x80, 0xFF],
        );
        assert_eq!(s.pcm, [0, 127 << 8, -128 << 8, -1 << 8]);
    }

    #[test]
    fn decodes_16bit_both_byte_orders_and_unsigned() {
        let le = sample_from(
            IT_SMP_HAS_SAMPLE | IT_SMP_16BIT,
            IT_CVT_SIGNED,
            2,
            &[0x34, 0x12, 0x00, 0x80],
        );
        assert_eq!(le.pcm, [0x1234, i16::MIN]);
        let be = sample_from(
            IT_SMP_HAS_SAMPLE | IT_SMP_16BIT,
            IT_CVT_SIGNED | IT_CVT_BIG_ENDIAN,
            2,
            &[0x12, 0x34, 0x80, 0x00],
        );
        assert_eq!(be.pcm, [0x1234, i16::MIN]);
        let un = sample_from(
            IT_SMP_HAS_SAMPLE | IT_SMP_16BIT,
            0,
            2,
            &[0x00, 0x80, 0xFF, 0xFF],
        );
        assert_eq!(un.pcm, [0, i16::MAX]);
    }

    #[test]
    fn decodes_delta_storage() {
        // 8-bit signed delta: +10, +10, -5 → 10, 20, 15.
        let d8 = sample_from(
            IT_SMP_HAS_SAMPLE,
            IT_CVT_SIGNED | IT_CVT_DELTA,
            3,
            &[10, 10, 0xFB],
        );
        assert_eq!(d8.pcm, [10 << 8, 20 << 8, 15 << 8]);
        // 16-bit signed delta: +1000, +1000, -500.
        let mut body = Vec::new();
        for v in [1000i16, 1000, -500] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        let d16 = sample_from(
            IT_SMP_HAS_SAMPLE | IT_SMP_16BIT,
            IT_CVT_SIGNED | IT_CVT_DELTA,
            3,
            &body,
        );
        assert_eq!(d16.pcm, [1000, 2000, 1500]);
    }

    #[test]
    fn truncated_body_is_clamped_and_flagged() {
        let s = sample_from(
            IT_SMP_HAS_SAMPLE | IT_SMP_16BIT,
            IT_CVT_SIGNED,
            8,
            &[1, 0, 2, 0, 3],
        );
        assert_eq!(s.pcm, [1, 2], "odd trailing byte is dropped");
        assert!(s.truncated);
        let mut s2 = sample_from(IT_SMP_HAS_SAMPLE, IT_CVT_SIGNED, 4, &[1, 2, 3, 4]);
        assert!(!s2.truncated);
        s2.sample_pointer = 0xFFFF_FF00;
        decode_sample_body(&mut s2, &[0u8; 16]);
        assert!(s2.pcm.is_empty() && s2.truncated);
    }

    #[test]
    fn compressed_and_absent_bodies_stay_empty() {
        let c = sample_from(
            IT_SMP_HAS_SAMPLE | IT_SMP_COMPRESSED,
            IT_CVT_SIGNED,
            4,
            &[1, 2, 3, 4],
        );
        assert!(c.compressed());
        assert!(c.pcm.is_empty(), "compressed bodies are not guessed at");
        assert!(!c.truncated);
        let a = sample_from(0, IT_CVT_SIGNED, 4, &[1, 2, 3, 4]);
        assert!(!a.has_sample());
        assert!(a.pcm.is_empty());
    }

    #[test]
    fn loops_validate_against_decoded_length() {
        let mut s = sample_from(
            IT_SMP_HAS_SAMPLE | IT_SMP_LOOP | IT_SMP_SUSTAIN_LOOP | IT_SMP_PINGPONG_LOOP,
            IT_CVT_SIGNED,
            8,
            &[0, 1, 2, 3, 4, 5, 6, 7],
        );
        s.loop_begin = 2;
        s.loop_end = 6;
        s.sustain_begin = 1;
        s.sustain_end = 3;
        assert_eq!(s.normal_loop(), Some((2, 6, true)));
        assert_eq!(s.sustain_loop(), Some((1, 3, false)));
        // End past the body → unusable.
        s.loop_end = 9;
        assert_eq!(s.normal_loop(), None);
        // Begin >= end → unusable.
        s.sustain_begin = 3;
        assert_eq!(s.sustain_loop(), None);
        // Flag clear → no loop even with sane bounds.
        s.flags &= !IT_SMP_SUSTAIN_LOOP;
        s.sustain_begin = 1;
        assert_eq!(s.sustain_loop(), None);
    }

    #[test]
    fn loop_view_selects_sustain_then_normal_then_one_shot() {
        use crate::mixer::{LoopKind, SampleSource};
        let mut s = sample_from(
            IT_SMP_HAS_SAMPLE | IT_SMP_LOOP | IT_SMP_SUSTAIN_LOOP | IT_SMP_PINGPONG_SUSTAIN,
            IT_CVT_SIGNED,
            8,
            &[0, 64, 0, 0, 0, 0, 0, 0],
        );
        s.loop_begin = 4;
        s.loop_end = 8;
        s.sustain_begin = 0;
        s.sustain_end = 2;
        let held = ItLoopView {
            sample: &s,
            sustain: true,
        };
        assert_eq!(
            (held.loop_start(), held.loop_end(), held.loop_kind()),
            (0, 2, LoopKind::PingPong)
        );
        let released = ItLoopView {
            sample: &s,
            sustain: false,
        };
        assert_eq!(
            (
                released.loop_start(),
                released.loop_end(),
                released.loop_kind()
            ),
            (4, 8, LoopKind::Forward)
        );
        assert!((held.at(1) - 64.0 * 256.0 / 32768.0).abs() < 1e-6);
        assert_eq!(held.at(99), 0.0);
        // Sustain requested but only the normal loop exists → normal.
        s.flags &= !IT_SMP_SUSTAIN_LOOP;
        let held = ItLoopView {
            sample: &s,
            sustain: true,
        };
        assert_eq!(held.loop_kind(), LoopKind::Forward);
        // No loops at all → one-shot over the whole body.
        s.flags &= !IT_SMP_LOOP;
        let v = ItLoopView {
            sample: &s,
            sustain: false,
        };
        assert_eq!(
            (v.loop_start(), v.loop_end(), v.loop_kind()),
            (0, 8, LoopKind::None)
        );
    }

    #[test]
    fn parse_samples_follows_offset_table_with_placeholders() {
        let mut file = build_header("s", &[0], 0, 3, 1, 0);
        let smp_base = IT_HEADER_FIXED_SIZE + 1;
        // Sample 1: valid header + body appended at the end.
        let hdr_off = file.len() as u32;
        let body_off = hdr_off + IT_SAMPLE_HEADER_SIZE as u32;
        let h = build_sample_header(
            "one",
            IT_SMP_HAS_SAMPLE,
            IT_CVT_SIGNED,
            3,
            (0, 0, 0, 0),
            8363,
            body_off,
        );
        file.extend_from_slice(&h);
        file.extend_from_slice(&[1, 2, 3]);
        set_offset(&mut file, smp_base, 0, hdr_off);
        // Sample 2: offset 0 → placeholder. Sample 3: offset past EOF →
        // placeholder.
        set_offset(&mut file, smp_base, 2, 0xFFFF_0000);
        let header = parse_header(&file).unwrap();
        let samples = parse_samples(&header, &file);
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0].name, "one");
        assert_eq!(samples[0].pcm, [1 << 8, 2 << 8, 3 << 8]);
        assert!(!samples[1].has_sample() && samples[1].pcm.is_empty());
        assert!(!samples[2].has_sample() && samples[2].pcm.is_empty());
    }

    #[test]
    fn rejects_missing_magic() {
        let mut b = build_header("x", &[0], 0, 0, 1, 0);
        b[0] = b'X';
        assert!(matches!(parse_header(&b), Err(Error::InvalidData(_))));
        assert!(!is_it(&b));
    }

    #[test]
    fn rejects_short_tables() {
        let mut b = build_header("x", &[0, 1, 2], 2, 3, 4, 0);
        b.truncate(b.len() - 1);
        assert!(matches!(parse_header(&b), Err(Error::InvalidData(_))));
        // The fixed header alone, with non-zero counts, is also short.
        let short = &b[..IT_HEADER_FIXED_SIZE];
        assert!(matches!(parse_header(short), Err(Error::InvalidData(_))));
    }

    #[test]
    fn parses_fixed_fields_and_tables() {
        let orders = [0u8, 1, IT_ORDER_SKIP, 2, IT_ORDER_END];
        let mut b = build_header(
            "hello world",
            &orders,
            2,
            3,
            4,
            IT_FLAG_STEREO | IT_FLAG_INSTRUMENTS | IT_FLAG_LINEAR_SLIDES,
        );
        // Special: message attached; message at the end of the file.
        b[0x2E] = 1;
        let msg = b"line one\rline two\0junk";
        let msg_off = b.len() as u32;
        b[0x36..0x38].copy_from_slice(&(msg.len() as u16).to_le_bytes());
        b[0x38..0x3C].copy_from_slice(&msg_off.to_le_bytes());
        b[0x40] = 0; // ch0 hard left
        b[0x41] = 64 | IT_PAN_DISABLED; // ch1 right + disabled
        b[0x42] = IT_PAN_SURROUND;
        b[0x80] = 40;
        let ins_base = IT_HEADER_FIXED_SIZE + orders.len();
        let smp_base = ins_base + 4 * 2;
        let pat_base = smp_base + 4 * 3;
        set_offset(&mut b, ins_base, 1, 0x1111);
        set_offset(&mut b, smp_base, 2, 0x2222);
        set_offset(&mut b, pat_base, 3, 0x3333);
        b.extend_from_slice(msg);

        let h = parse_header(&b).unwrap();
        assert_eq!(h.song_name, "hello world");
        assert_eq!(h.pattern_highlight, [4, 16]);
        assert_eq!(h.created_with_version(), (2, 0x14));
        assert_eq!(h.compatible_with, 0x200);
        assert!(!h.old_instrument_format());
        assert!(h.is_stereo());
        assert!(h.uses_instruments());
        assert!(h.linear_slides());
        assert!(!h.old_effects());
        assert!(!h.compatible_gxx());
        assert_eq!(h.global_volume, 128);
        assert_eq!(h.mix_volume, 48);
        assert_eq!(h.initial_speed, 6);
        assert_eq!(h.initial_tempo, 125);
        assert_eq!(h.pan_separation, 128);
        assert_eq!(h.orders, orders);
        assert_eq!(h.playable_order_count(), 3);
        assert_eq!(h.instrument_offsets, vec![0, 0x1111]);
        assert_eq!(h.sample_offsets, vec![0, 0, 0x2222]);
        assert_eq!(h.pattern_offsets, vec![0, 0, 0, 0x3333]);
        assert_eq!(h.tables_end(), pat_base + 16);
        assert_eq!(h.channel_initial_pan(0), 0);
        assert!(!h.channel_disabled(0));
        assert_eq!(h.channel_initial_pan(1), 64);
        assert!(h.channel_disabled(1));
        assert_eq!(h.channel_initial_pan(2), IT_PAN_SURROUND);
        assert_eq!(h.channel_volume[0], 40);
        assert_eq!(h.channel_volume[1], 64);
        assert!(h.has_message());
        assert_eq!(
            extract_message(&h, &b).as_deref(),
            Some("line one\nline two"),
            "0Dh newlines become '\\n'; the message ends at the first NUL"
        );
    }

    #[test]
    fn message_is_none_when_flag_clear_or_out_of_range() {
        let mut b = build_header("m", &[0], 0, 0, 1, 0);
        let h = parse_header(&b).unwrap();
        assert!(!h.has_message());
        assert_eq!(extract_message(&h, &b), None);
        b[0x2E] = 1;
        b[0x36..0x38].copy_from_slice(&10u16.to_le_bytes());
        b[0x38..0x3C].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        let h = parse_header(&b).unwrap();
        assert_eq!(
            extract_message(&h, &b),
            None,
            "offset past EOF → no message"
        );
    }

    #[test]
    fn old_instrument_format_threshold_is_cmwt_0x200() {
        let mut b = build_header("o", &[0], 0, 0, 1, 0);
        b[0x2A..0x2C].copy_from_slice(&0x01FFu16.to_le_bytes());
        assert!(parse_header(&b).unwrap().old_instrument_format());
        b[0x2A..0x2C].copy_from_slice(&0x0200u16.to_le_bytes());
        assert!(!parse_header(&b).unwrap().old_instrument_format());
    }
}
