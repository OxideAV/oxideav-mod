#![no_main]

//! Structure-aware STM playback fuzzing.
//!
//! `stm_decode` feeds raw bytes to the parser; most mutations there
//! land in sample data or leave the order table unplayable. This target
//! reads the fuzz bytes as a *recipe* and assembles an always-valid
//! Scream Tracker 2 module (or packed song) with the crate's own
//! `stm_writer`, so libFuzzer spends its budget on the engine: the
//! deferred `B` jump and the `C` break in every channel order, jumps to
//! unused / unstored / already-played orders (the one-finite-pass
//! stop), tempo bytes with a zero high nibble, every command letter
//! with every info byte, notes with out-of-range octaves and note
//! numbers, instrument numbers without a body, loop pairs past the
//! sample end, and volume values in the reserved range.
//!
//! The render runs to the end of the pass (bounded), so the
//! termination guarantee itself is under test.

use libfuzzer_sys::fuzz_target;
use oxideav_mod::stm::{StmCell, extract_samples, parse_header, parse_patterns};
use oxideav_mod::stm_player::StmPlayerState;
use oxideav_mod::stm_writer::{StmWriter, StmWriterInstrument, StmWriterPattern};

/// Upper bound on rendered frames: 128 orders x 64 rows x 15 ticks x
/// 882 frames is the longest possible single pass at 44.1 kHz; the cap
/// keeps one execution short while still crossing several patterns.
const RENDER_FRAMES: usize = 400_000;

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

fuzz_target!(|data: &[u8]| {
    let mut b = Bytes { data, pos: 0 };
    let mut w = StmWriter {
        tempo: b.u8(),
        global_volume: b.u8(),
        ..StmWriter::default()
    };
    let n_patterns = 1 + (b.u8() % 4) as usize;
    let n_inst = (b.u8() % 4) as usize;
    w.song = b.u8() & 0x0F == 0x0F;
    for _ in 0..n_inst {
        let len = (b.u16() % 600) as usize;
        let seed = b.u8();
        w.instruments.push(StmWriterInstrument {
            name: String::new(),
            pcm: (0..len)
                .map(|i| ((i as u32).wrapping_mul(seed as u32 + 3) % 251) as u8)
                .collect(),
            loop_start: b.u16() % 700,
            loop_end: if b.u8() & 3 == 0 { 0xFFFF } else { b.u16() % 700 },
            volume: b.u8() % 80,
            c2spd: b.u16(),
        });
    }
    let n_orders = 1 + (b.u8() % 8) as usize;
    w.orders = (0..n_orders).map(|_| b.u8() % (n_patterns as u8 + 2)).collect();
    for _ in 0..n_patterns {
        w.patterns.push(StmWriterPattern::new());
    }
    while !b.done() {
        let pat = b.u8() as usize % n_patterns;
        let row = (b.u8() % 64) as usize;
        let ch = (b.u8() % 4) as usize;
        let cell = StmCell {
            note_raw: b.u8(),
            instrument: b.u8() % 32,
            volume: b.u8() & 0x7F,
            command: b.u8() % 16,
            command_param: b.u8(),
        };
        w.patterns[pat].put(row, ch, cell);
    }

    let bytes = w.build();
    let Ok(header) = parse_header(&bytes) else {
        return;
    };
    let patterns = parse_patterns(&header, &bytes);
    let samples = extract_samples(&header, &bytes);
    let mut player = StmPlayerState::new(&header, samples, patterns, 44_100);
    let mut buf = vec![0i16; 4096 * 2];
    let mut total = 0;
    while total < RENDER_FRAMES {
        let n = player.render(&mut buf);
        if n == 0 {
            break;
        }
        total += n;
    }
});
