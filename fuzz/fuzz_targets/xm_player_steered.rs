#![no_main]

//! Structure-aware XM playback fuzzing.
//!
//! `xm_decode` feeds raw bytes to the parser, which rejects almost
//! every mutation before the player runs. This target instead reads
//! the fuzz bytes as a *recipe* and assembles an always-valid module
//! with the crate's own `xm_writer`, so libFuzzer spends its budget on
//! the engine: pattern loops and jumps (`E6x` / `Bxx` / `Dxx`
//! collisions, exhausted-loop breaks, rows past the pattern length),
//! note delays at or past the speed, envelopes with inverted loop /
//! sustain points, key-off / fadeout, keymap entries out of range,
//! ping-pong loops with odd bounds, `9xx` offsets past the end, every
//! effect byte with every parameter, and the volume column's reserved
//! ranges. The render covers a few rows so per-tick state machines
//! (retrig, tremor, arpeggio, vibrato) advance.

use libfuzzer_sys::fuzz_target;
use oxideav_mod::xm::{extract_sample_bodies, parse_header, parse_instruments, parse_patterns, XmCell};
use oxideav_mod::xm_player::XmPlayerState;
use oxideav_mod::xm_writer::{
    XmWriter, XmWriterEnvelope, XmWriterInstrument, XmWriterPattern, XmWriterSample,
};

const RENDER_FRAMES: usize = 8192;

struct Bytes<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bytes<'_> {
    fn u8(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.u8(), self.u8()])
    }
    fn done(&self) -> bool {
        self.pos >= self.data.len()
    }
}

fn envelope(b: &mut Bytes) -> XmWriterEnvelope {
    let n = (b.u8() % 13) as usize;
    let mut points = Vec::with_capacity(n);
    for _ in 0..n {
        // Mostly ascending ticks, sometimes deliberately not.
        points.push((b.u16() % 400, (b.u8() % 80) as u16));
    }
    XmWriterEnvelope {
        points,
        sustain_point: b.u8() % 14,
        loop_start_point: b.u8() % 14,
        loop_end_point: b.u8() % 14,
        type_bits: b.u8() & 0x07,
    }
}

fn sample(b: &mut Bytes) -> XmWriterSample {
    let len = 16 + (b.u16() % 1024) as usize;
    let sixteen = b.u8() & 1 != 0;
    let seed = b.u8();
    let pcm: Vec<i16> = (0..len)
        .map(|i| {
            let v = ((i as u32).wrapping_mul(seed as u32 + 1) % 64) as i16;
            (v - 32) * 800
        })
        .collect();
    let loop_start = (b.u16() as usize % (len + 4)) as u32;
    let loop_length = (b.u16() as usize % (len + 4)) as u32;
    XmWriterSample {
        name: String::new(),
        pcm,
        sixteen_bit: sixteen,
        volume: b.u8() % 72,
        finetune: b.u8() as i8,
        loop_mode: b.u8() % 4,
        loop_start,
        loop_length,
        panning: b.u8(),
        relative_note: (b.u8() % 60) as i8 - 30,
    }
}

fuzz_target!(|data: &[u8]| {
    let mut b = Bytes { data, pos: 0 };
    let num_channels = 1 + (b.u8() % 8) as u16;
    let mut w = XmWriter {
        num_channels,
        linear: b.u8() & 1 != 0,
        default_tempo: 1 + (b.u8() % 12) as u16,
        default_bpm: 32 + (b.u8() % 224) as u16,
        ..XmWriter::default()
    };
    let n_inst = 1 + (b.u8() % 3) as usize;
    for _ in 0..n_inst {
        let n_samples = (b.u8() % 3) as usize;
        let mut ins = XmWriterInstrument {
            volume_envelope: envelope(&mut b),
            panning_envelope: envelope(&mut b),
            vibrato_type: b.u8(),
            vibrato_sweep: b.u8(),
            vibrato_depth: b.u8(),
            vibrato_rate: b.u8(),
            volume_fadeout: b.u16(),
            ..XmWriterInstrument::default()
        };
        for slot in ins.sample_map.iter_mut() {
            *slot = b.u8() % 4;
        }
        for _ in 0..n_samples {
            ins.samples.push(sample(&mut b));
        }
        w.instruments.push(ins);
    }
    let n_patterns = 1 + (b.u8() % 3) as usize;
    for _ in 0..n_patterns {
        let rows = 1 + (b.u8() % 32) as u16;
        let mut p = XmWriterPattern::new(rows);
        while !b.done() {
            let row = b.u8() as u16 % rows;
            let ch = b.u8() % num_channels as u8;
            let cell = XmCell {
                note: b.u8() % 100,
                instrument: b.u8() % 5,
                volume: b.u8(),
                effect_type: b.u8() % 0x24,
                effect_param: b.u8(),
            };
            p.put(row, ch, cell);
            if b.u8() & 0x0F == 0 {
                break;
            }
        }
        w.patterns.push(p);
    }
    let n_orders = 1 + (b.u8() % 6) as usize;
    w.orders = (0..n_orders).map(|_| b.u8() % (n_patterns as u8 + 1)).collect();
    w.restart_position = b.u16() % (n_orders as u16 + 2);

    let bytes = w.build();
    let Ok(header) = parse_header(&bytes) else {
        return;
    };
    let Ok((patterns, off)) = parse_patterns(&header, &bytes) else {
        return;
    };
    let Ok(mut instruments) = parse_instruments(&header, &bytes, off) else {
        return;
    };
    extract_sample_bodies(&mut instruments, &bytes);
    let mut player = XmPlayerState::new(&header, instruments, patterns, 44_100);
    let mut buf = vec![0i16; 2048 * 2];
    let mut total = 0;
    while total < RENDER_FRAMES {
        let n = player.render(&mut buf);
        if n == 0 {
            break;
        }
        total += n;
    }
});
