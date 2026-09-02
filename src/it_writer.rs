//! Minimal Impulse Tracker file assembler.
//!
//! Test / fixture support: builds byte-exact `.it` files from the
//! layout in `docs/audio/trackers/it/ImpulseTracker-it.txt` so the
//! crate's own tests, the black-box oracle harness and the fuzz corpus
//! seeds can synthesise modules without shipping downloaded fixtures.
//! Not an encoder in the framework sense (there is no IT *codec*
//! encoder, by design) — hence hidden from the public documentation.

use crate::it::{
    ItCell, IT_CELL_COMMAND, IT_CELL_INSTRUMENT, IT_CELL_NOTE, IT_CELL_VOLPAN, IT_CVT_SIGNED,
    IT_HEADER_FIXED_SIZE, IT_INSTRUMENT_MAGIC, IT_INSTRUMENT_SIZE, IT_KEYMAP_ENTRIES, IT_MAGIC,
    IT_MAX_CHANNELS, IT_PATTERN_HEADER_SIZE, IT_SAMPLE_HEADER_SIZE, IT_SAMPLE_MAGIC, IT_SMP_16BIT,
    IT_SMP_HAS_SAMPLE,
};

/// One envelope for [`ItWriterInstrument`]: `flags`, node points,
/// loop / sustain-loop node indices.
#[derive(Clone, Debug, Default)]
#[doc(hidden)]
pub struct ItWriterEnvelope {
    pub flags: u8,
    pub nodes: Vec<(i8, u16)>,
    pub loop_begin: u8,
    pub loop_end: u8,
    pub sustain_begin: u8,
    pub sustain_end: u8,
}

/// A 2.x instrument to serialise.
#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct ItWriterInstrument {
    pub name: String,
    pub nna: u8,
    pub dct: u8,
    pub dca: u8,
    pub fadeout: u16,
    pub pps: i8,
    pub ppc: u8,
    pub global_volume: u8,
    /// Raw `DfP` (bit 7 = don't use).
    pub default_pan: u8,
    pub random_volume: u8,
    pub random_pan: u8,
    /// `(note, sample)` per input note; `None` = identity note with
    /// `default_sample`.
    pub keymap: Option<Vec<(u8, u8)>>,
    pub default_sample: u8,
    pub volume_envelope: ItWriterEnvelope,
    pub panning_envelope: ItWriterEnvelope,
    pub pitch_envelope: ItWriterEnvelope,
}

impl Default for ItWriterInstrument {
    fn default() -> Self {
        ItWriterInstrument {
            name: String::new(),
            nna: 0,
            dct: 0,
            dca: 0,
            fadeout: 0,
            pps: 0,
            ppc: 60,
            global_volume: 128,
            default_pan: 0x80,
            random_volume: 0,
            random_pan: 0,
            keymap: None,
            default_sample: 1,
            volume_envelope: ItWriterEnvelope::default(),
            panning_envelope: ItWriterEnvelope::default(),
            pitch_envelope: ItWriterEnvelope::default(),
        }
    }
}

/// A sample to serialise. `pcm` is signed 16-bit; `sixteen_bit` picks
/// the stored width (8-bit stores `pcm >> 8`).
#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct ItWriterSample {
    pub name: String,
    pub pcm: Vec<i16>,
    pub sixteen_bit: bool,
    pub global_volume: u8,
    pub default_volume: u8,
    /// Raw `DfP` (bit 7 = use).
    pub default_pan: u8,
    pub c5_speed: u32,
    /// Extra `Flg` bits (loop / sustain / ping-pong).
    pub flags: u8,
    pub loop_begin: u32,
    pub loop_end: u32,
    pub sustain_begin: u32,
    pub sustain_end: u32,
    pub vibrato_speed: u8,
    pub vibrato_depth: u8,
    pub vibrato_rate: u8,
    pub vibrato_wave: u8,
}

impl Default for ItWriterSample {
    fn default() -> Self {
        ItWriterSample {
            name: String::new(),
            pcm: Vec::new(),
            sixteen_bit: false,
            global_volume: 64,
            default_volume: 64,
            default_pan: 32,
            c5_speed: 8363,
            flags: 0,
            loop_begin: 0,
            loop_end: 0,
            sustain_begin: 0,
            sustain_end: 0,
            vibrato_speed: 0,
            vibrato_depth: 0,
            vibrato_rate: 0,
            vibrato_wave: 0,
        }
    }
}

/// A pattern to serialise: `num_rows` and explicit `(row, channel,
/// cell)` entries.
#[derive(Clone, Debug, Default)]
#[doc(hidden)]
pub struct ItWriterPattern {
    pub num_rows: u16,
    pub cells: Vec<(u16, u8, ItCell)>,
}

impl ItWriterPattern {
    pub fn new(num_rows: u16) -> Self {
        ItWriterPattern {
            num_rows,
            cells: Vec::new(),
        }
    }
    /// Place `cell` at `(row, channel)`.
    pub fn put(&mut self, row: u16, channel: u8, cell: ItCell) -> &mut Self {
        self.cells.push((row, channel, cell));
        self
    }
    /// Note + instrument at `(row, channel)`.
    pub fn note(&mut self, row: u16, channel: u8, note: u8, instrument: u8) -> &mut Self {
        self.put(row, channel, cell_note(note, instrument))
    }
    /// Effect at `(row, channel)`.
    pub fn effect(&mut self, row: u16, channel: u8, letter: char, param: u8) -> &mut Self {
        self.put(row, channel, cell_effect(letter, param))
    }
}

/// Note + instrument cell.
pub fn cell_note(note: u8, instrument: u8) -> ItCell {
    ItCell {
        mask: IT_CELL_NOTE
            | if instrument != 0 {
                IT_CELL_INSTRUMENT
            } else {
                0
            },
        note,
        instrument,
        ..ItCell::default()
    }
}

/// Effect-only cell (`letter` in `'A'..='Z'`).
pub fn cell_effect(letter: char, param: u8) -> ItCell {
    ItCell {
        mask: IT_CELL_COMMAND,
        command: letter.to_ascii_uppercase() as u8 - b'@',
        param,
        ..ItCell::default()
    }
}

/// Merge an effect into an existing cell.
pub fn with_effect(mut cell: ItCell, letter: char, param: u8) -> ItCell {
    cell.mask |= IT_CELL_COMMAND;
    cell.command = letter.to_ascii_uppercase() as u8 - b'@';
    cell.param = param;
    cell
}

/// Merge a volume-column byte into an existing cell.
pub fn with_volpan(mut cell: ItCell, volpan: u8) -> ItCell {
    cell.mask |= IT_CELL_VOLPAN;
    cell.volpan = volpan;
    cell
}

/// Whole-module builder.
#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct ItWriter {
    pub song_name: String,
    pub flags: u16,
    pub created_with: u16,
    pub compatible_with: u16,
    pub global_volume: u8,
    pub mix_volume: u8,
    pub initial_speed: u8,
    pub initial_tempo: u8,
    pub pan_separation: u8,
    pub channel_pan: [u8; IT_MAX_CHANNELS],
    pub channel_volume: [u8; IT_MAX_CHANNELS],
    pub orders: Vec<u8>,
    pub message: Option<String>,
    pub instruments: Vec<ItWriterInstrument>,
    pub samples: Vec<ItWriterSample>,
    pub patterns: Vec<ItWriterPattern>,
}

impl Default for ItWriter {
    fn default() -> Self {
        ItWriter {
            song_name: "oxideav".into(),
            flags: crate::it::IT_FLAG_STEREO | crate::it::IT_FLAG_LINEAR_SLIDES,
            created_with: 0x0214,
            compatible_with: 0x0214,
            global_volume: 128,
            mix_volume: 48,
            initial_speed: 6,
            initial_tempo: 125,
            pan_separation: 128,
            channel_pan: [32; IT_MAX_CHANNELS],
            channel_volume: [64; IT_MAX_CHANNELS],
            orders: Vec::new(),
            message: None,
            instruments: Vec::new(),
            samples: Vec::new(),
            patterns: Vec::new(),
        }
    }
}

fn put_str(dst: &mut [u8], s: &str) {
    let b = s.as_bytes();
    let n = b.len().min(dst.len().saturating_sub(1));
    dst[..n].copy_from_slice(&b[..n]);
}

fn write_envelope(dst: &mut [u8], e: &ItWriterEnvelope) {
    dst[0] = e.flags;
    dst[1] = e.nodes.len().min(25) as u8;
    dst[2] = e.loop_begin;
    dst[3] = e.loop_end;
    dst[4] = e.sustain_begin;
    dst[5] = e.sustain_end;
    for (i, &(y, t)) in e.nodes.iter().take(25).enumerate() {
        dst[6 + 3 * i] = y as u8;
        dst[7 + 3 * i..9 + 3 * i].copy_from_slice(&t.to_le_bytes());
    }
}

impl ItWriter {
    /// Serialise one instrument block (554 bytes).
    pub fn instrument_bytes(ins: &ItWriterInstrument) -> Vec<u8> {
        let mut b = vec![0u8; IT_INSTRUMENT_SIZE];
        b[0..4].copy_from_slice(IT_INSTRUMENT_MAGIC);
        b[0x11] = ins.nna;
        b[0x12] = ins.dct;
        b[0x13] = ins.dca;
        b[0x14..0x16].copy_from_slice(&ins.fadeout.to_le_bytes());
        b[0x16] = ins.pps as u8;
        b[0x17] = ins.ppc;
        b[0x18] = ins.global_volume;
        b[0x19] = ins.default_pan;
        b[0x1A] = ins.random_volume;
        b[0x1B] = ins.random_pan;
        b[0x1C..0x1E].copy_from_slice(&0x0214u16.to_le_bytes());
        put_str(&mut b[0x20..0x3A], &ins.name);
        for i in 0..IT_KEYMAP_ENTRIES {
            let (note, smp) = ins
                .keymap
                .as_ref()
                .and_then(|k| k.get(i).copied())
                .unwrap_or((i as u8, ins.default_sample));
            b[0x40 + 2 * i] = note;
            b[0x41 + 2 * i] = smp;
        }
        write_envelope(&mut b[0x130..0x182], &ins.volume_envelope);
        write_envelope(&mut b[0x182..0x1D4], &ins.panning_envelope);
        write_envelope(&mut b[0x1D4..0x226], &ins.pitch_envelope);
        b
    }

    /// Serialise one sample header (80 bytes) with `pointer` as the
    /// body offset.
    pub fn sample_header_bytes(s: &ItWriterSample, pointer: u32) -> Vec<u8> {
        let mut h = vec![0u8; IT_SAMPLE_HEADER_SIZE];
        h[0..4].copy_from_slice(IT_SAMPLE_MAGIC);
        h[0x11] = s.global_volume;
        let mut flags = s.flags;
        if !s.pcm.is_empty() {
            flags |= IT_SMP_HAS_SAMPLE;
        }
        if s.sixteen_bit {
            flags |= IT_SMP_16BIT;
        }
        h[0x12] = flags;
        h[0x13] = s.default_volume;
        put_str(&mut h[0x14..0x2E], &s.name);
        h[0x2E] = IT_CVT_SIGNED;
        h[0x2F] = s.default_pan;
        h[0x30..0x34].copy_from_slice(&(s.pcm.len() as u32).to_le_bytes());
        h[0x34..0x38].copy_from_slice(&s.loop_begin.to_le_bytes());
        h[0x38..0x3C].copy_from_slice(&s.loop_end.to_le_bytes());
        h[0x3C..0x40].copy_from_slice(&s.c5_speed.to_le_bytes());
        h[0x40..0x44].copy_from_slice(&s.sustain_begin.to_le_bytes());
        h[0x44..0x48].copy_from_slice(&s.sustain_end.to_le_bytes());
        h[0x48..0x4C].copy_from_slice(&pointer.to_le_bytes());
        h[0x4C] = s.vibrato_speed;
        h[0x4D] = s.vibrato_depth;
        h[0x4E] = s.vibrato_rate;
        h[0x4F] = s.vibrato_wave;
        h
    }

    /// Serialise a sample body in the header's width.
    pub fn sample_body_bytes(s: &ItWriterSample) -> Vec<u8> {
        if s.sixteen_bit {
            s.pcm.iter().flat_map(|v| v.to_le_bytes()).collect()
        } else {
            s.pcm.iter().map(|&v| (v >> 8) as i8 as u8).collect()
        }
    }

    /// Serialise one pattern (8-byte header + packed rows). Every cell
    /// is written with an explicit mask byte.
    pub fn pattern_bytes(p: &ItWriterPattern) -> Vec<u8> {
        let mut data = Vec::new();
        for row in 0..p.num_rows {
            let mut on_row: Vec<&(u16, u8, ItCell)> =
                p.cells.iter().filter(|(r, _, _)| *r == row).collect();
            on_row.sort_by_key(|(_, ch, _)| *ch);
            for &(_, ch, c) in on_row {
                let mask = c.mask & 0x0F;
                if mask == 0 {
                    continue;
                }
                data.push(0x80 | ((ch & 63) + 1));
                data.push(mask);
                if mask & IT_CELL_NOTE != 0 {
                    data.push(c.note);
                }
                if mask & IT_CELL_INSTRUMENT != 0 {
                    data.push(c.instrument);
                }
                if mask & IT_CELL_VOLPAN != 0 {
                    data.push(c.volpan);
                }
                if mask & IT_CELL_COMMAND != 0 {
                    data.push(c.command);
                    data.push(c.param);
                }
            }
            data.push(0);
        }
        let mut out = vec![0u8; IT_PATTERN_HEADER_SIZE];
        out[0..2].copy_from_slice(&(data.len().min(0xFFFF) as u16).to_le_bytes());
        out[2..4].copy_from_slice(&p.num_rows.to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    /// Assemble the complete file.
    pub fn build(&self) -> Vec<u8> {
        let n_ins = self.instruments.len();
        let n_smp = self.samples.len();
        let n_pat = self.patterns.len();
        let mut f = vec![0u8; IT_HEADER_FIXED_SIZE];
        f[0..4].copy_from_slice(IT_MAGIC);
        put_str(&mut f[4..0x1E], &self.song_name);
        f[0x1E] = 4;
        f[0x1F] = 16;
        f[0x20..0x22].copy_from_slice(&(self.orders.len() as u16).to_le_bytes());
        f[0x22..0x24].copy_from_slice(&(n_ins as u16).to_le_bytes());
        f[0x24..0x26].copy_from_slice(&(n_smp as u16).to_le_bytes());
        f[0x26..0x28].copy_from_slice(&(n_pat as u16).to_le_bytes());
        f[0x28..0x2A].copy_from_slice(&self.created_with.to_le_bytes());
        f[0x2A..0x2C].copy_from_slice(&self.compatible_with.to_le_bytes());
        f[0x2C..0x2E].copy_from_slice(&self.flags.to_le_bytes());
        let special: u16 = if self.message.is_some() { 1 } else { 0 };
        f[0x2E..0x30].copy_from_slice(&special.to_le_bytes());
        f[0x30] = self.global_volume;
        f[0x31] = self.mix_volume;
        f[0x32] = self.initial_speed;
        f[0x33] = self.initial_tempo;
        f[0x34] = self.pan_separation;
        f[0x40..0x80].copy_from_slice(&self.channel_pan);
        f[0x80..0xC0].copy_from_slice(&self.channel_volume);
        f.extend_from_slice(&self.orders);
        let ins_base = f.len();
        f.resize(f.len() + 4 * (n_ins + n_smp + n_pat), 0u8);
        let smp_base = ins_base + 4 * n_ins;
        let pat_base = smp_base + 4 * n_smp;

        if let Some(msg) = &self.message {
            let bytes: Vec<u8> = msg
                .bytes()
                .map(|b| if b == b'\n' { 0x0D } else { b })
                .collect();
            let off = f.len() as u32;
            f[0x36..0x38].copy_from_slice(&(bytes.len() as u16).to_le_bytes());
            f[0x38..0x3C].copy_from_slice(&off.to_le_bytes());
            f.extend_from_slice(&bytes);
            f.push(0);
        }

        for (i, ins) in self.instruments.iter().enumerate() {
            let off = f.len() as u32;
            f[ins_base + 4 * i..ins_base + 4 * i + 4].copy_from_slice(&off.to_le_bytes());
            f.extend_from_slice(&Self::instrument_bytes(ins));
        }
        // Sample headers first, then all bodies (IT's own layout).
        let hdr_offsets: Vec<usize> = (0..n_smp)
            .map(|i| f.len() + i * IT_SAMPLE_HEADER_SIZE)
            .collect();
        for (i, s) in self.samples.iter().enumerate() {
            f[smp_base + 4 * i..smp_base + 4 * i + 4]
                .copy_from_slice(&(hdr_offsets[i] as u32).to_le_bytes());
            f.extend_from_slice(&Self::sample_header_bytes(s, 0));
        }
        for (i, p) in self.patterns.iter().enumerate() {
            let off = f.len() as u32;
            f[pat_base + 4 * i..pat_base + 4 * i + 4].copy_from_slice(&off.to_le_bytes());
            f.extend_from_slice(&Self::pattern_bytes(p));
        }
        for (i, s) in self.samples.iter().enumerate() {
            let body_off = f.len() as u32;
            let h = hdr_offsets[i];
            f[h + 0x48..h + 0x4C].copy_from_slice(&body_off.to_le_bytes());
            f.extend_from_slice(&Self::sample_body_bytes(s));
        }
        f
    }
}

/// A square-wave test sample: `len` frames, half-period `half`,
/// amplitude `amp`, looped over the whole body.
pub fn square_sample(len: usize, half: usize, amp: i16) -> ItWriterSample {
    ItWriterSample {
        name: "square".into(),
        pcm: (0..len)
            .map(|i| {
                if (i / half.max(1)) % 2 == 0 {
                    amp
                } else {
                    -amp
                }
            })
            .collect(),
        flags: crate::it::IT_SMP_LOOP,
        loop_end: len as u32,
        ..ItWriterSample::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::it::{parse_module, IT_ENV_ON};

    #[test]
    fn round_trips_through_the_parser() {
        let mut w = ItWriter {
            message: Some("hello\nworld".into()),
            ..ItWriter::default()
        };
        w.flags |= crate::it::IT_FLAG_INSTRUMENTS;
        w.orders = vec![0, 1, 255];
        w.channel_pan[1] = 0;
        w.channel_volume[2] = 40;
        w.samples.push(square_sample(64, 8, 20000));
        w.samples.push(ItWriterSample {
            name: "wide".into(),
            pcm: vec![1000, -1000, 32767, -32768],
            sixteen_bit: true,
            c5_speed: 22050,
            ..ItWriterSample::default()
        });
        w.instruments.push(ItWriterInstrument {
            name: "lead".into(),
            nna: 3,
            fadeout: 50,
            volume_envelope: ItWriterEnvelope {
                flags: IT_ENV_ON,
                nodes: vec![(64, 0), (0, 40)],
                ..ItWriterEnvelope::default()
            },
            keymap: Some((0..120).map(|n| (n, 2)).collect()),
            ..ItWriterInstrument::default()
        });
        let mut p0 = ItWriterPattern::new(16);
        p0.note(0, 0, 60, 1).effect(0, 1, 'A', 3);
        p0.put(
            4,
            2,
            with_volpan(with_effect(cell_note(72, 1), 'D', 0x0F), 40),
        );
        let p1 = ItWriterPattern::new(8);
        w.patterns = vec![p0, p1];

        let bytes = w.build();
        let m = parse_module(&bytes).unwrap();
        assert_eq!(m.header.song_name, "oxideav");
        assert_eq!(m.header.orders, vec![0, 1, 255]);
        assert!(m.header.uses_instruments() && m.header.linear_slides());
        assert_eq!(m.message.as_deref(), Some("hello\nworld"));
        assert_eq!(m.header.channel_pan[1], 0);
        assert_eq!(m.header.channel_volume[2], 40);
        assert_eq!(m.samples.len(), 2);
        assert_eq!(m.samples[0].pcm.len(), 64);
        assert_eq!(m.samples[0].pcm[0], (20000i16 >> 8) << 8);
        assert_eq!(m.samples[0].normal_loop(), Some((0, 64, false)));
        assert_eq!(m.samples[1].pcm, vec![1000, -1000, 32767, -32768]);
        assert_eq!(m.samples[1].c5_speed, 22050);
        assert_eq!(m.instruments[0].name, "lead");
        assert_eq!(m.instruments[0].nna, crate::it::ItNna::NoteFade);
        assert_eq!(m.instruments[0].fadeout, 50);
        assert_eq!(m.instruments[0].map_note(5), (5, 2));
        assert_eq!(
            m.instruments[0].volume_envelope.nodes,
            vec![(64, 0), (0, 40)]
        );
        assert_eq!(m.patterns.len(), 2);
        assert_eq!(m.patterns[0].num_rows, 16);
        assert_eq!(m.patterns[0].cell(0, 0), cell_note(60, 1));
        assert_eq!(m.patterns[0].cell(0, 1), cell_effect('A', 3));
        let c = m.patterns[0].cell(4, 2);
        assert_eq!(
            (c.note, c.instrument, c.volpan, c.command, c.param),
            (72, 1, 40, 4, 0x0F)
        );
        assert_eq!(m.patterns[1].num_rows, 8);
        assert_eq!(m.num_channels, 3);
    }
}
