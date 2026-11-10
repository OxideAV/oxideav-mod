//! Render-throughput measurement for the four engines.
//!
//! `cargo bench --bench throughput` renders one full pass of a module
//! per format, several times, and prints the audio duration, the best
//! wall time, the resulting "x realtime" factor and an FNV-1a hash of
//! the PCM (so a mixer change can be checked for bit-identical output).
//!
//! Inputs:
//!  - STM: the Scream Tracker 2.21 example song staged under
//!    `docs/audio/trackers/stm/fixtures/` (with its instrument files)
//!    when the docs tree sits next to the workspace, else a synthetic
//!    module;
//!  - IT: the two Impulse Tracker 2.14p5 example songs staged under
//!    `docs/audio/trackers/it/fixtures/` when present, plus a synthetic
//!    64-channel instrument-mode module;
//!  - MOD / XM: dense synthetic modules (every row of every channel
//!    carries a note or an effect), since no real module of either
//!    format is staged.
//!
//! A path given in `OXIDEAV_MOD_BENCH` (any of the four formats, picked
//! by extension) is measured as well.

use std::path::{Path, PathBuf};
use std::time::Instant;

use oxideav_mod::it::{self, ItCell};
use oxideav_mod::it_player::ItPlayerState;
use oxideav_mod::it_writer::{
    ItWriter, ItWriterEnvelope, ItWriterInstrument, ItWriterPattern, ItWriterSample,
};
use oxideav_mod::player::PlayerState;
use oxideav_mod::stm::{self, StmCell, StmSampleBody};
use oxideav_mod::stm_player::StmPlayerState;
use oxideav_mod::stm_writer::{StmWriter, StmWriterInstrument, StmWriterPattern};
use oxideav_mod::xm::{self, XmCell};
use oxideav_mod::xm_player::XmPlayerState;
use oxideav_mod::xm_writer::{
    XmWriter, XmWriterEnvelope, XmWriterInstrument, XmWriterPattern, XmWriterSample,
};

const RATE: u32 = 44_100;
/// Timed repetitions per input; the best one is reported (the machine
/// is rarely idle, and the minimum is the least disturbed run).
const RUNS: usize = 7;
/// Cap on rendered audio per run (a looping module still terminates).
const MAX_SECONDS: usize = 1200;

struct Lcg(u32);
impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0 >> 8
    }
    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

fn fnv(hash: &mut u64, pcm: &[i16]) {
    for &s in pcm {
        for b in s.to_le_bytes() {
            *hash ^= b as u64;
            *hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
}

/// Render to the end of the pass; returns (frames, hash).
fn drain(mut render: impl FnMut(&mut [i16]) -> usize) -> (usize, u64) {
    let mut buf = vec![0i16; 4096 * 2];
    let mut frames = 0usize;
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    while frames < MAX_SECONDS * RATE as usize {
        let n = render(&mut buf);
        if n == 0 {
            break;
        }
        fnv(&mut hash, &buf[..n * 2]);
        frames += n;
    }
    (frames, hash)
}

fn measure(label: &str, mut run: impl FnMut() -> (usize, u64)) {
    let mut best = f64::MAX;
    let mut result = (0usize, 0u64);
    for _ in 0..RUNS {
        let t = Instant::now();
        result = run();
        best = best.min(t.elapsed().as_secs_f64());
    }
    let audio = result.0 as f64 / RATE as f64;
    println!(
        "{label:<34} {audio:>8.1} s audio  {:>8.1} ms  {:>7.0} x realtime  pcm {:016x}",
        best * 1e3,
        audio / best,
        result.1
    );
}

// ---------------------------------------------------------------- MOD

fn synthetic_mod(channels: usize, patterns: usize) -> Vec<u8> {
    let tag: &[u8; 4] = match channels {
        4 => b"M.K.",
        8 => b"8CHN",
        _ => panic!("4 or 8 channels"),
    };
    let mut rng = Lcg(0x1234_5678);
    let mut out = vec![0u8; 1084];
    out[..10].copy_from_slice(b"throughput");
    // Four samples: 8 000 bytes each, the first two looped.
    let sample_len = 8000usize;
    for i in 0..4 {
        let off = 20 + i * 30;
        out[off + 22..off + 24].copy_from_slice(&((sample_len / 2) as u16).to_be_bytes());
        out[off + 24] = (i as u8) & 0x0F;
        out[off + 25] = 64;
        if i < 2 {
            out[off + 26..off + 28].copy_from_slice(&1000u16.to_be_bytes());
            out[off + 28..off + 30].copy_from_slice(&2000u16.to_be_bytes());
        } else {
            out[off + 28..off + 30].copy_from_slice(&1u16.to_be_bytes());
        }
    }
    out[950] = patterns as u8;
    out[951] = 0x7F;
    for i in 0..patterns {
        out[952 + i] = i as u8;
    }
    out[1080..1084].copy_from_slice(tag);
    const PERIODS: [u16; 12] = [856, 808, 762, 720, 678, 640, 604, 570, 538, 508, 480, 453];
    const EFFECTS: [(u8, u8); 10] = [
        (0x0, 0x37),
        (0x1, 0x04),
        (0x2, 0x04),
        (0x4, 0x46),
        (0xA, 0x02),
        (0xC, 0x30),
        (0x7, 0x34),
        (0x9, 0x08),
        (0xE, 0x93),
        (0x0, 0x00),
    ];
    for _ in 0..patterns {
        for _row in 0..64 {
            for _ch in 0..channels {
                let roll = rng.below(4);
                let (period, sample) = if roll == 0 {
                    let octave = rng.below(3);
                    (
                        PERIODS[rng.below(12) as usize] >> octave,
                        1 + rng.below(4) as u8,
                    )
                } else {
                    (0, 0)
                };
                let (fx, param) = EFFECTS[rng.below(EFFECTS.len() as u32) as usize];
                out.push((sample & 0xF0) | (period >> 8) as u8);
                out.push(period as u8);
                out.push((sample << 4) | fx);
                out.push(param);
            }
        }
    }
    for i in 0..4 {
        for n in 0..sample_len {
            let phase = (n * (i + 2)) as f32 * 0.05;
            out.push((phase.sin() * 90.0) as i8 as u8);
        }
    }
    out
}

fn run_mod(bytes: &[u8]) -> (usize, u64) {
    let header = oxideav_mod::header::parse_header(bytes).expect("mod header");
    let patterns = oxideav_mod::player::parse_patterns(&header, bytes);
    let samples = oxideav_mod::samples::extract_samples(&header, bytes);
    let mut p = PlayerState::new(&header, samples, patterns, RATE);
    drain(|buf| p.render(buf))
}

// ----------------------------------------------------------------- XM

fn synthetic_xm(channels: u16, patterns: usize) -> Vec<u8> {
    let mut rng = Lcg(0x0BAD_CAFE);
    let mut w = XmWriter {
        num_channels: channels,
        linear: true,
        default_tempo: 6,
        default_bpm: 125,
        ..XmWriter::default()
    };
    for i in 0..4usize {
        let pcm: Vec<i16> = (0..6000)
            .map(|n| (((n * (i + 2)) as f32 * 0.04).sin() * 9000.0) as i16)
            .collect();
        let mut ins = XmWriterInstrument {
            volume_envelope: XmWriterEnvelope {
                points: vec![(0, 64), (20, 40), (60, 50), (120, 0)],
                sustain_point: 2,
                loop_start_point: 1,
                loop_end_point: 2,
                type_bits: 0x01 | ((i as u8 & 1) << 2),
            },
            panning_envelope: XmWriterEnvelope {
                points: vec![(0, 16), (40, 48), (80, 32)],
                sustain_point: 1,
                loop_start_point: 0,
                loop_end_point: 2,
                type_bits: (i as u8 >> 1) & 1,
            },
            vibrato_depth: (i * 3) as u8,
            vibrato_rate: 10,
            volume_fadeout: 400,
            ..XmWriterInstrument::default()
        };
        ins.samples.push(XmWriterSample {
            name: String::new(),
            pcm,
            sixteen_bit: i % 2 == 0,
            volume: 56,
            finetune: 0,
            loop_mode: (i % 3) as u8,
            loop_start: 1000,
            loop_length: 3000,
            panning: (40 + i * 50) as u8,
            relative_note: 0,
        });
        w.instruments.push(ins);
    }
    const EFFECTS: [(u8, u8); 10] = [
        (0x00, 0x47),
        (0x01, 0x08),
        (0x02, 0x08),
        (0x04, 0x46),
        (0x0A, 0x02),
        (0x0C, 0x30),
        (0x07, 0x34),
        (0x19, 0x04),
        (0x1B, 0x23),
        (0x00, 0x00),
    ];
    for _ in 0..patterns {
        let mut p = XmWriterPattern::new(64);
        for row in 0..64u16 {
            for ch in 0..channels as u8 {
                let roll = rng.below(4);
                let (fx, param) = EFFECTS[rng.below(EFFECTS.len() as u32) as usize];
                p.put(
                    row,
                    ch,
                    XmCell {
                        note: if roll == 0 {
                            37 + rng.below(36) as u8
                        } else {
                            0
                        },
                        instrument: if roll == 0 { 1 + rng.below(4) as u8 } else { 0 },
                        volume: if roll == 1 {
                            0x10 + rng.below(0x40) as u8
                        } else {
                            0
                        },
                        effect_type: fx,
                        effect_param: param,
                    },
                );
            }
        }
        w.patterns.push(p);
    }
    w.orders = (0..patterns as u8).collect();
    w.build()
}

fn run_xm(bytes: &[u8]) -> (usize, u64) {
    let header = xm::parse_header(bytes).expect("xm header");
    let (patterns, after) = xm::parse_patterns(&header, bytes).expect("xm patterns");
    let mut instruments = xm::parse_instruments(&header, bytes, after).expect("xm instruments");
    xm::extract_sample_bodies(&mut instruments, bytes);
    let mut p = XmPlayerState::new(&header, instruments, patterns, RATE);
    drain(|buf| p.render(buf))
}

// ---------------------------------------------------------------- STM

fn synthetic_stm(patterns: usize) -> Vec<u8> {
    let mut rng = Lcg(0x5EED_0001);
    let mut w = StmWriter::default();
    for i in 0..4usize {
        w.instruments.push(StmWriterInstrument {
            name: format!("S{i}.SMP"),
            pcm: (0..6000)
                .map(|n| (128.0 + ((n * (i + 2)) as f32 * 0.05).sin() * 100.0) as u8)
                .collect(),
            loop_start: 500,
            loop_end: if i < 2 { 5500 } else { 0xFFFF },
            volume: 64,
            c2spd: 8363,
        });
    }
    w.instruments[2].loop_start = 0;
    w.instruments[3].loop_start = 0;
    const COMMANDS: [(u8, u8); 8] = [
        (0, 0),
        (4, 0x02),
        (5, 0x04),
        (6, 0x04),
        (8, 0x46),
        (9, 0x21),
        (10, 0x47),
        (11, 0x02),
    ];
    for _ in 0..patterns {
        let mut p = StmWriterPattern::new();
        for row in 0..64 {
            for ch in 0..4 {
                let roll = rng.below(4);
                let (command, command_param) = COMMANDS[rng.below(COMMANDS.len() as u32) as usize];
                p.put(
                    row,
                    ch,
                    StmCell {
                        note_raw: if roll == 0 {
                            ((1 + rng.below(3) as u8) << 4) | rng.below(12) as u8
                        } else {
                            255
                        },
                        instrument: if roll == 0 { 1 + rng.below(4) as u8 } else { 0 },
                        volume: 65,
                        command,
                        command_param,
                    },
                );
            }
        }
        w.patterns.push(p);
    }
    w.orders = (0..patterns as u8).collect();
    w.build()
}

fn stm_player(bytes: &[u8], instrument_dir: Option<&Path>) -> StmPlayerState {
    let header = stm::parse_header(bytes).expect("stm header");
    let patterns = stm::parse_patterns(&header, bytes);
    let mut samples = stm::extract_samples(&header, bytes);
    if let Some(dir) = instrument_dir {
        for (i, inst) in header.instruments.iter().enumerate() {
            if let Ok(raw) = std::fs::read(dir.join(inst.name.to_uppercase())) {
                samples[i] = StmSampleBody::from_unsigned(inst, &raw);
            }
        }
    }
    StmPlayerState::new(&header, samples, patterns, RATE)
}

// ----------------------------------------------------------------- IT

fn synthetic_it(channels: u8, patterns: usize) -> Vec<u8> {
    let mut rng = Lcg(0x1717_4242);
    let mut w = ItWriter {
        flags: 0x000D, // stereo, instruments, linear slides
        ..ItWriter::default()
    };
    for i in 0..4usize {
        w.samples.push(ItWriterSample {
            pcm: (0..6000)
                .map(|n| (((n * (i + 2)) as f32 * 0.04).sin() * 9000.0) as i16)
                .collect(),
            sixteen_bit: i % 2 == 0,
            flags: 0x10 | if i == 3 { 0x40 } else { 0 },
            loop_begin: 1000,
            loop_end: 4000,
            ..ItWriterSample::default()
        });
        w.instruments.push(ItWriterInstrument {
            nna: (i % 4) as u8, // cut / continue / off / fade
            fadeout: 64,
            default_sample: i as u8 + 1,
            volume_envelope: ItWriterEnvelope {
                flags: 0x01 | if i % 2 == 0 { 0x04 } else { 0 },
                nodes: vec![(64, 0), (40, 20), (56, 60), (0, 160)],
                sustain_begin: 1,
                sustain_end: 2,
                ..ItWriterEnvelope::default()
            },
            panning_envelope: ItWriterEnvelope {
                flags: (i as u8 >> 1) & 1,
                nodes: vec![(-20, 0), (20, 40), (0, 80)],
                ..ItWriterEnvelope::default()
            },
            ..ItWriterInstrument::default()
        });
    }
    const EFFECTS: [(char, u8); 10] = [
        ('J', 0x47),
        ('E', 0x08),
        ('F', 0x08),
        ('H', 0x46),
        ('D', 0x02),
        ('R', 0x34),
        ('Y', 0x44),
        ('Q', 0x23),
        ('Z', 0x50),
        ('A', 0x06),
    ];
    for _ in 0..patterns {
        let mut p = ItWriterPattern::new(64);
        for row in 0..64u16 {
            for ch in 0..channels {
                match rng.below(4) {
                    0 => {
                        p.put(
                            row,
                            ch,
                            oxideav_mod::it_writer::cell_note(
                                36 + rng.below(48) as u8,
                                1 + rng.below(4) as u8,
                            ),
                        );
                    }
                    1 => {
                        let (letter, param) = EFFECTS[rng.below(EFFECTS.len() as u32) as usize];
                        p.effect(row, ch, letter, param);
                    }
                    _ => {}
                }
            }
        }
        w.patterns.push(p);
    }
    w.orders = (0..patterns as u8).chain([255]).collect();
    let _: Option<ItCell> = None;
    w.build()
}

fn run_it(bytes: &[u8]) -> (usize, u64) {
    let module = it::parse_module(bytes).expect("it module");
    let mut p = ItPlayerState::new(module, RATE);
    drain(|buf| p.render(buf))
}

// --------------------------------------------------------------- main

fn docs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/audio/trackers")
}

fn main() {
    println!("oxideav-mod render throughput ({RATE} Hz stereo S16, one pass, best of {RUNS})");

    let m4 = synthetic_mod(4, 32);
    measure("MOD synthetic 4ch x 32 patterns", || run_mod(&m4));
    let m8 = synthetic_mod(8, 32);
    measure("MOD synthetic 8ch x 32 patterns", || run_mod(&m8));

    let stm_dir = docs_dir().join("stm/fixtures/st221-shareware-example");
    if let Ok(bytes) = std::fs::read(stm_dir.join("SONG.STS")) {
        let ins = stm_dir.join("INS");
        measure("STM ST 2.21 example song", || {
            let mut p = stm_player(&bytes, Some(&ins));
            drain(|buf| p.render(buf))
        });
    }
    let s = synthetic_stm(32);
    measure("STM synthetic 4ch x 32 patterns", || {
        let mut p = stm_player(&s, None);
        drain(|buf| p.render(buf))
    });

    let x16 = synthetic_xm(16, 24);
    measure("XM synthetic 16ch x 24 patterns", || run_xm(&x16));
    let x32 = synthetic_xm(32, 24);
    measure("XM synthetic 32ch x 24 patterns", || run_xm(&x32));

    let it_dir = docs_dir().join("it/fixtures/it214v5-distribution");
    for name in ["CHRIS31B.IT", "JEFF93.IT"] {
        if let Ok(bytes) = std::fs::read(it_dir.join(name)) {
            measure(&format!("IT {name} (IT 2.14p5 example)"), || run_it(&bytes));
        }
    }
    let i64ch = synthetic_it(64, 16);
    measure("IT synthetic 64ch NNA x 16 patterns", || run_it(&i64ch));

    if let Ok(path) = std::env::var("OXIDEAV_MOD_BENCH") {
        let bytes = std::fs::read(&path).expect("OXIDEAV_MOD_BENCH file");
        let ext = Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let label = format!("{ext} {path}");
        match ext.as_str() {
            "xm" => measure(&label, || run_xm(&bytes)),
            "it" => measure(&label, || run_it(&bytes)),
            "stm" | "sts" => measure(&label, || {
                let mut p = stm_player(&bytes, None);
                drain(|buf| p.render(buf))
            }),
            _ => measure(&label, || run_mod(&bytes)),
        }
    }
}
