#![no_main]

//! Drive arbitrary fuzz-supplied bytes through the full STM (Scream
//! Tracker v1) parsing + playback pipeline.
//!
//! Stages exercised, in order:
//!
//!   1. `stm::is_stm` — cheap sniff (`"!Scream!"` banner + `"P"`
//!      magic byte at offset 20 + 0x02 type byte at offset 29).
//!
//!   2. `stm::parse_header` — reads the 1208-byte fixed header
//!      (20-byte song name + tracker tag + format-version bytes +
//!      tempo + 128-entry order table + 31 × 32 instrument
//!      descriptors). Danger spots: the instrument's C3 Hz (LE u32 at
//!      sample-header byte 24..28) drives the pitch model — a zero
//!      must not divide-by-zero in `StmC3Pitch::step_for`. Per-sample
//!      `loop_start` / `loop_end` at byte 14..18 must be clamped
//!      against the actual PCM body.
//!
//!   3. `stm::parse_patterns` — fixed 64-row × 4-channel layout, 4
//!      bytes per cell. Even with all-zero or all-0xFF cells the
//!      parse path must produce a `StmPattern` rather than panic.
//!
//!   4. `stm::extract_samples` — walks per-instrument bodies after
//!      pattern data, clamping each declared length against the
//!      remaining file.
//!
//!   5. `stm_player::StmPlayerState::new` — wires the parsed pieces
//!      into a mixable engine.
//!
//!   6. `StmPlayerState::render` — drives a short PCM burst so the
//!      tick / effect / mixer pipeline is exercised on every fuzz
//!      input that survives stages 1–5.
//!
//! Classic STM danger spots driven here:
//!
//! * **Instrument C3 Hz of zero** — must not divide-by-zero in the
//!   semitone → step conversion.
//! * **Instrument volume > 64** — must clamp at the 0..=64 limit
//!   before entering the mixer's pre-multiplied gain path.
//! * **Order table sentinel** — STM uses 254/255 as "no pattern"
//!   sentinels; the order trim in `StmPlayerState::new` must stop at
//!   the first 255 entry rather than walking the full 128-byte table.
//! * **`0xy` arpeggio param zero** — must be inert per the
//!   ProTracker contract STM inherits via "Effect command in
//!   ProTracker format".
//! * **`7xy` tremolo `[0, 64]` clamp** — the per-tick sine LFO offset
//!   added to `ch.volume` must be clamped before entering the global
//!   gain path.
//! * **`Fxx` speed/tempo split at $20** — same as MOD; `F00` (stop)
//!   must terminate the render rather than spin.
//!
//! Render budget cap stays the same as the MOD target: 2048 frames
//! stereo ≈ 46 ms at 44.1 kHz.

use libfuzzer_sys::fuzz_target;
use oxideav_mod::stm::{extract_samples, parse_header, parse_patterns};
use oxideav_mod::stm_player::StmPlayerState;

const RENDER_FRAMES: usize = 2048;

fuzz_target!(|data: &[u8]| {
    let header = match parse_header(data) {
        Ok(h) => h,
        Err(_) => return,
    };
    let patterns = parse_patterns(&header, data);
    let samples = extract_samples(&header, data);

    let mut player = StmPlayerState::new(&header, samples, patterns, 44_100);
    let mut buf = vec![0i16; RENDER_FRAMES * 2];
    let _ = player.render(&mut buf);
});
