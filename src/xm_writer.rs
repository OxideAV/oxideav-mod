//! Minimal FastTracker 2 module assembler.
//!
//! Test / fixture support: builds byte-exact `.xm` files from the
//! layout in `docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt`
//! (header at offset 0, patterns, instruments with the extended
//! header, 40-byte sample headers, delta-coded PCM bodies) so the
//! crate's own tests, the black-box oracle harness and the fuzz corpus
//! seeds can synthesise modules without shipping downloaded fixtures.
//! Not an encoder in the framework sense (there is no XM *codec*
//! encoder, by design) — hence hidden from the public documentation.

use crate::xm::{
    XmCell, XM_BANNER, XM_HEADER_SIZE_OFFSET, XM_ID_BYTE_OFFSET,
    XM_INSTRUMENT_HEADER_SIZE_WITH_SAMPLES, XM_MIN_HEADER_LEN, XM_ORDER_TABLE_OFFSET,
    XM_ORDER_TABLE_SIZE, XM_PATTERN_HEADER_SIZE, XM_SAMPLE_HEADER_SIZE, XM_VERSION_0104,
};

/// Envelope type bit 0: envelope on.
pub const XM_ENV_ON: u8 = 0x01;
/// Envelope type bit 1: sustain point enabled.
pub const XM_ENV_SUSTAIN: u8 = 0x02;
/// Envelope type bit 2: loop enabled.
pub const XM_ENV_LOOP: u8 = 0x04;

/// One envelope for [`XmWriterInstrument`]: up to 12 `(tick, value)`
/// points, the sustain / loop point indices and the type bits.
#[derive(Clone, Debug, Default)]
#[doc(hidden)]
pub struct XmWriterEnvelope {
    pub points: Vec<(u16, u16)>,
    pub sustain_point: u8,
    pub loop_start_point: u8,
    pub loop_end_point: u8,
    pub type_bits: u8,
}

/// One sample for [`XmWriterInstrument`]. `pcm` is always given in the
/// 16-bit domain; when `sixteen_bit` is false the body is written as
/// the high byte of each frame. Loop points are in frames.
#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct XmWriterSample {
    pub name: String,
    pub pcm: Vec<i16>,
    pub sixteen_bit: bool,
    pub volume: u8,
    pub finetune: i8,
    /// 0 = none, 1 = forward, 2 = ping-pong.
    pub loop_mode: u8,
    pub loop_start: u32,
    pub loop_length: u32,
    pub panning: u8,
    pub relative_note: i8,
}

impl Default for XmWriterSample {
    fn default() -> Self {
        XmWriterSample {
            name: String::new(),
            pcm: Vec::new(),
            sixteen_bit: false,
            volume: 64,
            finetune: 0,
            loop_mode: 0,
            loop_start: 0,
            loop_length: 0,
            panning: 128,
            relative_note: 0,
        }
    }
}

/// One instrument for [`XmWriter`].
#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct XmWriterInstrument {
    pub name: String,
    /// Note → sample index map (96 entries; missing entries are 0).
    pub sample_map: Vec<u8>,
    pub volume_envelope: XmWriterEnvelope,
    pub panning_envelope: XmWriterEnvelope,
    pub vibrato_type: u8,
    pub vibrato_sweep: u8,
    pub vibrato_depth: u8,
    pub vibrato_rate: u8,
    pub volume_fadeout: u16,
    pub samples: Vec<XmWriterSample>,
}

impl Default for XmWriterInstrument {
    fn default() -> Self {
        XmWriterInstrument {
            name: String::new(),
            sample_map: vec![0; 96],
            volume_envelope: XmWriterEnvelope::default(),
            panning_envelope: XmWriterEnvelope::default(),
            vibrato_type: 0,
            vibrato_sweep: 0,
            vibrato_depth: 0,
            vibrato_rate: 0,
            volume_fadeout: 0,
            samples: Vec::new(),
        }
    }
}

/// One pattern for [`XmWriter`]: `num_rows` rows, sparse cells.
#[derive(Clone, Debug, Default)]
#[doc(hidden)]
pub struct XmWriterPattern {
    pub num_rows: u16,
    pub cells: Vec<(u16, u8, XmCell)>,
}

impl XmWriterPattern {
    pub fn new(num_rows: u16) -> Self {
        XmWriterPattern {
            num_rows,
            cells: Vec::new(),
        }
    }

    pub fn put(&mut self, row: u16, channel: u8, cell: XmCell) -> &mut Self {
        self.cells.retain(|(r, c, _)| !(*r == row && *c == channel));
        self.cells.push((row, channel, cell));
        self
    }

    /// Note + instrument, no volume column, no effect.
    pub fn note(&mut self, row: u16, channel: u8, note: u8, instrument: u8) -> &mut Self {
        self.put(row, channel, cell_note(note, instrument))
    }

    /// Effect only.
    pub fn effect(&mut self, row: u16, channel: u8, effect: u8, param: u8) -> &mut Self {
        self.put(row, channel, cell_effect(effect, param))
    }
}

/// A note + instrument cell.
pub fn cell_note(note: u8, instrument: u8) -> XmCell {
    XmCell {
        note,
        instrument,
        ..XmCell::default()
    }
}

/// An effect-only cell.
pub fn cell_effect(effect: u8, param: u8) -> XmCell {
    XmCell {
        effect_type: effect,
        effect_param: param,
        ..XmCell::default()
    }
}

/// Add an effect to a cell.
pub fn with_effect(mut cell: XmCell, effect: u8, param: u8) -> XmCell {
    cell.effect_type = effect;
    cell.effect_param = param;
    cell
}

/// Add a raw volume-column byte to a cell.
pub fn with_volume(mut cell: XmCell, volume: u8) -> XmCell {
    cell.volume = volume;
    cell
}

/// A whole module.
#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct XmWriter {
    pub module_name: String,
    pub tracker_name: String,
    pub num_channels: u16,
    pub linear: bool,
    pub default_tempo: u16,
    pub default_bpm: u16,
    pub restart_position: u16,
    pub orders: Vec<u8>,
    pub patterns: Vec<XmWriterPattern>,
    pub instruments: Vec<XmWriterInstrument>,
}

impl Default for XmWriter {
    fn default() -> Self {
        XmWriter {
            module_name: "oxideav fixture".into(),
            tracker_name: "oxideav".into(),
            num_channels: 2,
            linear: true,
            default_tempo: 6,
            default_bpm: 125,
            restart_position: 0,
            orders: vec![0],
            patterns: Vec::new(),
            instruments: Vec::new(),
        }
    }
}

fn padded(s: &str, n: usize, pad: u8) -> Vec<u8> {
    let mut v: Vec<u8> = s.bytes().take(n).collect();
    v.resize(n, pad);
    v
}

impl XmWriter {
    /// Pack one pattern: every cell is written in the 5-byte unpacked
    /// form (mask byte `0x9F`, then note / instrument / volume / effect
    /// / param); empty cells are the single `0x80` mask byte.
    pub fn pattern_bytes(p: &XmWriterPattern, num_channels: u16) -> Vec<u8> {
        let mut packed = Vec::new();
        for row in 0..p.num_rows {
            for ch in 0..num_channels as u8 {
                let cell = p
                    .cells
                    .iter()
                    .find(|(r, c, _)| *r == row && *c == ch)
                    .map(|(_, _, cell)| *cell)
                    .unwrap_or_default();
                if cell == XmCell::default() {
                    packed.push(0x80);
                } else {
                    packed.push(0x9F);
                    packed.push(cell.note);
                    packed.push(cell.instrument);
                    packed.push(cell.volume);
                    packed.push(cell.effect_type);
                    packed.push(cell.effect_param);
                }
            }
        }
        let mut out = Vec::with_capacity(9 + packed.len());
        out.extend_from_slice(&XM_PATTERN_HEADER_SIZE.to_le_bytes());
        out.push(0);
        out.extend_from_slice(&p.num_rows.to_le_bytes());
        out.extend_from_slice(&(packed.len() as u16).to_le_bytes());
        out.extend(packed);
        out
    }

    fn envelope_bytes(env: &XmWriterEnvelope) -> [u8; 48] {
        let mut raw = [0u8; 48];
        for (i, (x, y)) in env.points.iter().take(12).enumerate() {
            raw[i * 4..i * 4 + 2].copy_from_slice(&x.to_le_bytes());
            raw[i * 4 + 2..i * 4 + 4].copy_from_slice(&y.to_le_bytes());
        }
        raw
    }

    /// The instrument header, the sample headers and the delta-coded
    /// bodies, in file order.
    pub fn instrument_bytes(ins: &XmWriterInstrument) -> Vec<u8> {
        let mut out = Vec::new();
        let has_samples = !ins.samples.is_empty();
        let header_size = if has_samples {
            XM_INSTRUMENT_HEADER_SIZE_WITH_SAMPLES
        } else {
            29
        };
        out.extend_from_slice(&header_size.to_le_bytes());
        out.extend(padded(&ins.name, 22, 0));
        out.push(0);
        out.extend_from_slice(&(ins.samples.len() as u16).to_le_bytes());
        if !has_samples {
            return out;
        }
        out.extend_from_slice(&XM_SAMPLE_HEADER_SIZE.to_le_bytes());
        let mut map = ins.sample_map.clone();
        map.resize(96, 0);
        out.extend_from_slice(&map[..96]);
        out.extend_from_slice(&Self::envelope_bytes(&ins.volume_envelope));
        out.extend_from_slice(&Self::envelope_bytes(&ins.panning_envelope));
        out.push(ins.volume_envelope.points.len().min(12) as u8);
        out.push(ins.panning_envelope.points.len().min(12) as u8);
        out.push(ins.volume_envelope.sustain_point);
        out.push(ins.volume_envelope.loop_start_point);
        out.push(ins.volume_envelope.loop_end_point);
        out.push(ins.panning_envelope.sustain_point);
        out.push(ins.panning_envelope.loop_start_point);
        out.push(ins.panning_envelope.loop_end_point);
        out.push(ins.volume_envelope.type_bits);
        out.push(ins.panning_envelope.type_bits);
        out.push(ins.vibrato_type);
        out.push(ins.vibrato_sweep);
        out.push(ins.vibrato_depth);
        out.push(ins.vibrato_rate);
        out.extend_from_slice(&ins.volume_fadeout.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        while out.len() < header_size as usize {
            out.push(0);
        }
        for s in &ins.samples {
            let bps = if s.sixteen_bit { 2u32 } else { 1u32 };
            out.extend_from_slice(&(s.pcm.len() as u32 * bps).to_le_bytes());
            out.extend_from_slice(&(s.loop_start * bps).to_le_bytes());
            out.extend_from_slice(&(s.loop_length * bps).to_le_bytes());
            out.push(s.volume);
            out.push(s.finetune as u8);
            let mut ty = s.loop_mode & 0x03;
            if s.sixteen_bit {
                ty |= 0x10;
            }
            out.push(ty);
            out.push(s.panning);
            out.push(s.relative_note as u8);
            out.push(0);
            out.extend(padded(&s.name, 22, 0));
        }
        for s in &ins.samples {
            if s.sixteen_bit {
                let mut old: i16 = 0;
                for &v in &s.pcm {
                    out.extend_from_slice(&v.wrapping_sub(old).to_le_bytes());
                    old = v;
                }
            } else {
                let mut old: i8 = 0;
                for &v in &s.pcm {
                    let b = (v >> 8) as i8;
                    out.push(b.wrapping_sub(old) as u8);
                    old = b;
                }
            }
        }
        out
    }

    /// Assemble the whole file.
    pub fn build(&self) -> Vec<u8> {
        let mut out = vec![0u8; XM_MIN_HEADER_LEN];
        out[0..17].copy_from_slice(XM_BANNER);
        out[17..37].copy_from_slice(&padded(&self.module_name, 20, b' '));
        out[XM_ID_BYTE_OFFSET] = 0x1A;
        out[38..58].copy_from_slice(&padded(&self.tracker_name, 20, b' '));
        out[58..60].copy_from_slice(&XM_VERSION_0104.to_le_bytes());
        let hs = XM_HEADER_SIZE_OFFSET;
        out[hs..hs + 4].copy_from_slice(&0x114u32.to_le_bytes());
        out[hs + 4..hs + 6].copy_from_slice(&(self.orders.len() as u16).to_le_bytes());
        out[hs + 6..hs + 8].copy_from_slice(&self.restart_position.to_le_bytes());
        out[hs + 8..hs + 10].copy_from_slice(&self.num_channels.to_le_bytes());
        out[hs + 10..hs + 12].copy_from_slice(&(self.patterns.len() as u16).to_le_bytes());
        out[hs + 12..hs + 14].copy_from_slice(&(self.instruments.len() as u16).to_le_bytes());
        out[hs + 14..hs + 16].copy_from_slice(&(self.linear as u16).to_le_bytes());
        out[hs + 16..hs + 18].copy_from_slice(&self.default_tempo.to_le_bytes());
        out[hs + 18..hs + 20].copy_from_slice(&self.default_bpm.to_le_bytes());
        for i in 0..XM_ORDER_TABLE_SIZE {
            out[XM_ORDER_TABLE_OFFSET + i] = self.orders.get(i).copied().unwrap_or(0);
        }
        for p in &self.patterns {
            out.extend(Self::pattern_bytes(p, self.num_channels));
        }
        for ins in &self.instruments {
            out.extend(Self::instrument_bytes(ins));
        }
        out
    }
}

/// A square-wave test sample: `len` frames, half-period `half`,
/// amplitude `amp`, forward-looped over the whole body, 8-bit.
pub fn square_sample(len: usize, half: usize, amp: i16) -> XmWriterSample {
    XmWriterSample {
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
        loop_mode: 1,
        loop_length: len as u32,
        ..XmWriterSample::default()
    }
}

/// An instrument wrapping one sample, no envelopes.
pub fn single_sample_instrument(sample: XmWriterSample) -> XmWriterInstrument {
    XmWriterInstrument {
        name: "inst".into(),
        samples: vec![sample],
        ..XmWriterInstrument::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xm::{extract_sample_bodies, parse_header, parse_instruments, parse_patterns};

    #[test]
    fn writer_round_trips_through_the_parser() {
        let mut pat = XmWriterPattern::new(4);
        pat.note(0, 0, 49, 1)
            .put(1, 1, with_volume(cell_effect(0x0A, 0x0F), 0x30))
            .effect(3, 0, 0x0D, 0x00);
        let mut ins = single_sample_instrument(square_sample(64, 16, 12000));
        ins.volume_envelope = XmWriterEnvelope {
            points: vec![(0, 64), (10, 0)],
            type_bits: XM_ENV_ON,
            ..XmWriterEnvelope::default()
        };
        ins.samples.push(XmWriterSample {
            sixteen_bit: true,
            pcm: vec![100, -200, 300],
            loop_mode: 2,
            loop_start: 1,
            loop_length: 2,
            relative_note: 12,
            ..XmWriterSample::default()
        });
        ins.sample_map[48] = 1;
        let w = XmWriter {
            num_channels: 2,
            patterns: vec![pat],
            instruments: vec![ins],
            ..XmWriter::default()
        };
        let bytes = w.build();
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.num_channels, 2);
        assert_eq!(h.num_patterns, 1);
        assert_eq!(h.num_instruments, 1);
        let (pats, off) = parse_patterns(&h, &bytes).unwrap();
        assert_eq!(pats[0].num_rows, 4);
        assert_eq!(pats[0].rows[0][0].note, 49);
        assert_eq!(pats[0].rows[0][0].instrument, 1);
        assert_eq!(pats[0].rows[1][1].volume, 0x30);
        assert_eq!(pats[0].rows[1][1].effect_type, 0x0A);
        assert_eq!(pats[0].rows[3][0].effect_type, 0x0D);
        let mut inst = parse_instruments(&h, &bytes, off).unwrap();
        extract_sample_bodies(&mut inst, &bytes);
        assert_eq!(inst[0].samples.len(), 2);
        assert_eq!(inst[0].volume_envelope.points, vec![(0, 64), (10, 0)]);
        assert_eq!(inst[0].sample_map[48], 1);
        assert_eq!(inst[0].samples[0].pcm8.len(), 64);
        assert_eq!(inst[0].samples[0].pcm8[0], (12000i16 >> 8) as i8);
        assert_eq!(inst[0].samples[1].pcm16, vec![100, -200, 300]);
        assert_eq!(inst[0].samples[1].loop_start, 2);
        assert_eq!(inst[0].samples[1].loop_length, 4);
        assert_eq!(inst[0].samples[1].relative_note, 12);
    }
}
