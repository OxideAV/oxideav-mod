#![no_main]

//! Drive arbitrary fuzz-supplied bytes through the full STM (Scream
//! Tracker 2) parsing + playback pipeline.
//!
//! Stages exercised, in order:
//!
//!   1. `stm::parse_header` — the 48-byte header, the 31 × 32-byte
//!      instrument records and the 128-byte order table at 0x410.
//!      Danger spots: a zero C2SPD must not divide by zero in the
//!      pitch model; loop pairs must be clamped against the body.
//!
//!   2. `stm::parse_patterns` — 64 rows × 4 channels per pattern, each
//!      cell four bytes unless its note byte is 251..=253 (a one-byte
//!      cell), so the walk is data-dependent and must stop cleanly at
//!      end of file.
//!
//!   3. `stm::extract_samples` — re-walks the pattern data to find the
//!      sample area, then takes each 16-byte-padded unsigned body,
//!      clamping declared lengths against the remaining file.
//!
//!   4. `stm_player::StmPlayerState::new` + `render` — a short PCM
//!      burst through the row / tick engine: tempo bytes with a zero
//!      high nibble, order entries that are unused (99) or name
//!      unstored patterns, the deferred `B` jump / `C` break, and every
//!      command letter's slide arithmetic.
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
