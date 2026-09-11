#![no_main]

//! Structure-aware IT playback fuzzing.
//!
//! The raw-bytes `it_decode` target rarely reaches the player with
//! anything but a near-empty module. This target reads the fuzz bytes
//! as a recipe and assembles an always-valid module with the crate's
//! own `it_writer`: instrument mode with NNA / DCT / DCA combinations
//! (so the virtual-channel pool, duplicate checks and voice stealing
//! run), envelopes with loop / sustain-loop points in any order,
//! samples with normal + sustain loops of any shape, and pattern cells
//! spanning every command letter, parameter, and volume-column value.
//! The render covers a few rows so per-tick state (retrig, tremor,
//! panbrello, tempo slides, pattern delay) advances.

use libfuzzer_sys::fuzz_target;
use oxideav_mod::it::{parse_module, ItCell};
use oxideav_mod::it_player::ItPlayerState;
use oxideav_mod::it_writer::{
    ItWriter, ItWriterEnvelope, ItWriterInstrument, ItWriterPattern, ItWriterSample,
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

fn envelope(b: &mut Bytes) -> ItWriterEnvelope {
    let n = (b.u8() % 26) as usize;
    let mut nodes = Vec::with_capacity(n);
    for _ in 0..n {
        nodes.push((b.u8() as i8, b.u16() % 600));
    }
    ItWriterEnvelope {
        flags: b.u8() & 0x07,
        nodes,
        loop_begin: b.u8() % 26,
        loop_end: b.u8() % 26,
        sustain_begin: b.u8() % 26,
        sustain_end: b.u8() % 26,
    }
}

fn sample(b: &mut Bytes) -> ItWriterSample {
    let len = 16 + (b.u16() % 1024) as usize;
    let seed = b.u8();
    let pcm: Vec<i16> = (0..len)
        .map(|i| {
            let v = ((i as u32).wrapping_mul(seed as u32 + 1) % 64) as i16;
            (v - 32) * 800
        })
        .collect();
    let bound = |b: &mut Bytes| (b.u16() as usize % (len + 4)) as u32;
    ItWriterSample {
        name: String::new(),
        pcm,
        sixteen_bit: b.u8() & 1 != 0,
        global_volume: b.u8() % 72,
        default_volume: b.u8() % 72,
        default_pan: b.u8(),
        c5_speed: 1000 + (b.u16() as u32) * 2,
        flags: b.u8(),
        loop_begin: bound(b),
        loop_end: bound(b),
        sustain_begin: bound(b),
        sustain_end: bound(b),
        vibrato_speed: b.u8(),
        vibrato_depth: b.u8(),
        vibrato_rate: b.u8(),
        vibrato_wave: b.u8() % 4,
    }
}

fuzz_target!(|data: &[u8]| {
    let mut b = Bytes { data, pos: 0 };
    let mut w = ItWriter {
        flags: b.u16(),
        global_volume: b.u8(),
        mix_volume: b.u8(),
        initial_speed: b.u8(),
        initial_tempo: b.u8(),
        pan_separation: b.u8(),
        ..ItWriter::default()
    };
    let n_samples = 1 + (b.u8() % 3) as usize;
    for _ in 0..n_samples {
        w.samples.push(sample(&mut b));
    }
    let n_inst = (b.u8() % 3) as usize;
    for _ in 0..n_inst {
        let mut ins = ItWriterInstrument {
            nna: b.u8() % 4,
            dct: b.u8() % 4,
            dca: b.u8() % 3,
            fadeout: b.u16() % 1100,
            pps: b.u8() as i8,
            ppc: b.u8() % 120,
            global_volume: b.u8(),
            default_pan: b.u8(),
            random_volume: b.u8(),
            random_pan: b.u8(),
            default_sample: b.u8() % (n_samples as u8 + 2),
            volume_envelope: envelope(&mut b),
            panning_envelope: envelope(&mut b),
            pitch_envelope: envelope(&mut b),
            ..ItWriterInstrument::default()
        };
        if b.u8() & 1 != 0 {
            let n = (b.u8() % 8) as usize;
            let mut map = Vec::with_capacity(n);
            for _ in 0..n {
                map.push((b.u8() % 120, b.u8() % (n_samples as u8 + 2)));
            }
            ins.keymap = Some(map);
        }
        w.instruments.push(ins);
    }
    let n_patterns = 1 + (b.u8() % 3) as usize;
    for _ in 0..n_patterns {
        let rows = 1 + (b.u8() % 32) as u16;
        let mut p = ItWriterPattern::new(rows);
        while !b.done() {
            let row = b.u8() as u16 % rows;
            let ch = b.u8() % 8;
            let cell = ItCell {
                mask: b.u8() & 0x0F,
                note: b.u8(),
                instrument: b.u8() % 6,
                volpan: b.u8(),
                command: b.u8() % 28,
                param: b.u8(),
            };
            p.put(row, ch, cell);
            if b.u8() & 0x0F == 0 {
                break;
            }
        }
        w.patterns.push(p);
    }
    let n_orders = 1 + (b.u8() % 6) as usize;
    w.orders = (0..n_orders)
        .map(|_| match b.u8() % 8 {
            0 => 254,
            1 => 255,
            v => v % n_patterns as u8,
        })
        .collect();
    w.orders.push(255);

    let bytes = w.build();
    let Ok(module) = parse_module(&bytes) else {
        return;
    };
    let mut player = ItPlayerState::new(module, 44_100);
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
