#![no_main]

//! Drive arbitrary fuzz-supplied bytes through the full XM
//! (FastTracker 2 Extended Module) parsing + playback pipeline.
//!
//! Stages exercised, in order:
//!
//!   1. `xm::is_xm` — cheap sniff for the 17-byte
//!      `"Extended Module: "` ASCII banner at offset 0.
//!
//!   2. `xm::parse_header` — reads the 336-byte file header (banner,
//!      module name, tracker name, version, header size, song length,
//!      restart position, channel / pattern / instrument counts,
//!      frequency-table flag, default tempo / BPM, 256-entry order
//!      table). Danger spots: an unbounded `header_size` slot must
//!      not produce a negative `pattern_data_offset`; song-length and
//!      pattern-count fields each cap at 256 but a hostile value
//!      requires the pattern walker to clamp.
//!
//!   3. `xm::parse_patterns` — variable-length bit-packed rows. Each
//!      cell is gated by a mask byte that picks which of note /
//!      instrument / volume-column / effect-type / effect-param are
//!      present; the unpacker walks until `packed_size` bytes have
//!      been consumed and zero-fills the rest. A hostile
//!      `packed_size` of 0 must produce an all-empty pattern, not an
//!      empty `Vec` of rows.
//!
//!   4. `xm::parse_instruments` — per-instrument: 8-byte mini-header,
//!      then if `n_samples > 0` the full 263-byte block (sample-map
//!      table, volume + panning envelope nodes, vibrato params,
//!      fadeout, reserved). Envelope point count is capped at 12 per
//!      the spec; a hostile count must clamp.
//!
//!   5. `xm::extract_sample_bodies` — walks 8- or 16-bit delta-PCM
//!      sample bodies after the instrument table. Per-sample length
//!      is in bytes, not samples, so a 16-bit sample with an odd
//!      declared length must not overflow the delta-undelta loop.
//!
//!   6. `xm_player::XmPlayerState::new` — wires the parsed pieces
//!      into a mixable engine.
//!
//!   7. `XmPlayerState::render` — drives a short PCM burst so the
//!      tick / effect / envelope / mixer pipeline is exercised on
//!      every fuzz input that survives stages 1–6.
//!
//! Classic XM danger spots driven here:
//!
//! * **Frequency-table flag** — Amiga vs. Linear pitch tables are
//!   selected by a single header bit; both must produce finite step
//!   values at any period.
//! * **Volume envelope node ordering** — envelope ticks must be
//!   monotonically non-decreasing; a hostile envelope with
//!   out-of-order ticks must not infinite-loop the segment walker.
//! * **Fadeout** — 16-bit fadeout value subtracted per tick; a
//!   hostile fadeout of 0xFFFF must underflow to 0 cleanly rather
//!   than wrap.
//! * **Note 97 = key-off** — must transition to envelope release
//!   without retriggering the sample.
//! * **`E3x` glissando** — when on, tone-porta snaps the period to
//!   the nearest semitone after each tick's linear slide step. The
//!   Amiga snap walks the entire 10-octave period table and picks
//!   the nearest entry; a hostile period must not infinite-loop the
//!   nearest-neighbour search.
//! * **`Lxy` set envelope position** — `param` directly seeks the
//!   tick cursor; a hostile param past the envelope's last node must
//!   clamp.
//! * **Volume-column kinds** — eleven distinct kinds (0..=0x5F set
//!   volume, 0x60..=0x6F volume-slide down, etc.). Every reserved
//!   nibble must be tolerated.
//!
//! Render budget cap stays the same as the MOD/STM targets: 2048
//! frames stereo ≈ 46 ms at 44.1 kHz.

use libfuzzer_sys::fuzz_target;
use oxideav_mod::xm::{extract_sample_bodies, parse_header, parse_instruments, parse_patterns};
use oxideav_mod::xm_player::XmPlayerState;

const RENDER_FRAMES: usize = 2048;

fuzz_target!(|data: &[u8]| {
    let header = match parse_header(data) {
        Ok(h) => h,
        Err(_) => return,
    };

    let (patterns, inst_offset) = match parse_patterns(&header, data) {
        Ok(pair) => pair,
        Err(_) => return,
    };

    let mut instruments = match parse_instruments(&header, data, inst_offset) {
        Ok(v) => v,
        Err(_) => return,
    };

    extract_sample_bodies(&mut instruments, data);

    let mut player = XmPlayerState::new(&header, instruments, patterns, 44_100);
    let mut buf = vec![0i16; RENDER_FRAMES * 2];
    let _ = player.render(&mut buf);
});
