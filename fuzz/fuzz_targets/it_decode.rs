#![no_main]

//! Drive arbitrary fuzz-supplied bytes through the full Impulse
//! Tracker (`.it`) parsing + playback pipeline.
//!
//! Stages exercised, in order:
//!
//!   1. `it::parse_module` — `IMPM` header (counts, flags, per-channel
//!      pan / volume, order list, three u32 offset tables), song
//!      message, instruments (old 1.x and 2.x layouts, keymap, three
//!      envelopes), sample headers + bodies (8/16-bit, signed /
//!      unsigned, byte order, delta, both loop pairs) and packed
//!      patterns (channel-marker / mask walk with per-channel
//!      memories).
//!
//!   2. `it_player::ItPlayerState::new` — wires the module into the
//!      engine (host channels from the highest used channel, voice
//!      pool, flags).
//!
//!   3. `ItPlayerState::render` — a short PCM burst so the row / tick
//!      / effect / envelope / NNA / mixer pipeline runs on every input
//!      that survives stage 1.
//!
//! Danger spots the layout invites, all of which must return rather
//! than panic / abort / OOM:
//!
//! * **Offset tables** — every instrument / sample / pattern offset is
//!   attacker-controlled and may point anywhere (past EOF, into the
//!   header, at itself); each must be bounds-checked before the block
//!   is read, and a bad offset must yield a placeholder so 1-based
//!   numbering stays aligned.
//! * **Sample bodies** — `Length` is in frames, times 1 or 2 bytes, and
//!   `SamplePointer` is free; the decoder must clamp to the file and
//!   never allocate `Length` up front.
//! * **Loops** — `LoopEnd` / `SusLoopEnd` past the body or below the
//!   begin must collapse to no-loop, or the mixer's wrap arithmetic
//!   would index past the PCM.
//! * **Envelopes** — `Num` up to 255 with 25 slots; out-of-order ticks
//!   must end the node list, and loop / sustain node indices must
//!   clamp, or the segment walker would never terminate.
//! * **Patterns** — `Rows` outside `32..=200`, `Length` past the file,
//!   a channel marker with bit 7 clear on a channel that never had a
//!   mask, and a mask requiring more bytes than remain.
//! * **Playback** — a `C5Speed` of 0 (silent, not divide-by-zero), an
//!   order list of only `+++`, `Bxx` past the order list, `SBx` loops,
//!   `Axx` speed 0 (ignored), `Txx` below 32, 64-channel modules with
//!   NNA continue filling the voice pool, and every effect letter with
//!   every parameter nibble.
//!
//! Render budget cap stays the same as the other targets: 2048 frames
//! stereo ≈ 46 ms at 44.1 kHz.

use libfuzzer_sys::fuzz_target;
use oxideav_mod::it::parse_module;
use oxideav_mod::it_player::ItPlayerState;

const RENDER_FRAMES: usize = 2048;

fuzz_target!(|data: &[u8]| {
    let module = match parse_module(data) {
        Ok(m) => m,
        Err(_) => return,
    };
    let mut player = ItPlayerState::new(module, 44_100);
    let mut buf = vec![0i16; RENDER_FRAMES * 2];
    let _ = player.render(&mut buf);
});
