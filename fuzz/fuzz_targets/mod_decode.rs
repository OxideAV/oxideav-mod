#![no_main]

//! Drive arbitrary fuzz-supplied bytes through the full MOD parsing
//! pipeline.
//!
//! The decoder must always return rather than panic / abort / OOM,
//! regardless of how malformed the input is. The return values are
//! intentionally discarded — the contract under test is *the call
//! returns*, not what it returns.
//!
//! Pipeline stages exercised, in order:
//!
//!   1. `header::parse_header` — reads the 1084-byte fixed header
//!      (title + 31 × 30 sample descriptors + song-length + restart +
//!      128-byte order table + 4-byte signature). Danger spots: the
//!      signed-4-bit finetune at sample-header byte 24, the
//!      `M.K. / M!K! / 4CHN / 6CHN / 8CHN / FLT4 / FLT8 / OCTA /
//!      CD81 / xxCH` signature dispatch (with `xxCH` only valid for
//!      10..=32 channels), and the `n_patterns = 1 +
//!      max(order[0..song_length])` computation that lets a single
//!      byte ask the parser to walk megabytes of pattern data.
//!
//!   2. `player::parse_patterns` — walks `n_patterns × 64 rows ×
//!      channels × 4 bytes` of pattern data starting at offset 1084.
//!      Already tolerant of short input (truncated cells become
//!      all-zero notes), but the fuzzer still drives every legal
//!      channel count from 4 to 32, every legal pattern count from 1
//!      to 128, and every legal effect nibble 0..=0xF with arbitrary
//!      effect parameters.
//!
//!   3. `samples::extract_samples` — walks the 31 sample bodies after
//!      pattern data, clamping each declared length against the
//!      remaining file. Loop metadata (`repeat_start`,
//!      `repeat_length`) is clamped against the actual PCM length so
//!      the mixer never reads past the buffer.
//!
//!   4. `player::PlayerState::new` — wires the parsed pieces into a
//!      mixable engine. Per-channel `pan` defaults to the Amiga LRRL
//!      hard-pan layout, LED filter defaults to ON.
//!
//!   5. `PlayerState::render` — drives a short PCM burst (≤ 1 KiB of
//!      i16 stereo, ~5 ms at 44.1 kHz) so the entire tick / effect /
//!      mixer pipeline is exercised on every fuzz input that survives
//!      stages 1–4. We cap the render budget at a few thousand frames
//!      so libfuzzer's per-iteration time stays small even on
//!      pathological speed / BPM combinations.
//!
//! Classic MOD danger spots driven here:
//!
//! * **Finetune ±8 extreme period table walk** — a hostile finetune
//!   of -8 or +7 must produce a valid `PERIOD_TABLE[finetune_row][n]`
//!   index for any note 0..=35, never a panic.
//! * **`Fxx` speed/BPM split** — `< 0x20` sets speed (ticks/division),
//!   `>= 0x20` sets BPM. `F00` is "stop song" per the spec; the
//!   render must terminate cleanly rather than spinning a zero-tick
//!   mixer.
//! * **`Bxx` position jump** — must clamp the order index against
//!   `song_length`. A hostile `BFF` against a song-length-of-1 file
//!   must not index past the order table.
//! * **`Dxy` pattern break** — decimal `x*10 + y` must clamp at 63
//!   (max pattern row index).
//! * **`E6x` pattern loop** — per-channel start + count state; a
//!   hostile `E6F` loop count must not overflow.
//! * **`9xx` sample-offset out-of-range** — `9FF` against an empty or
//!   short sample must take the "no note played" path rather than
//!   wrapping a looped sample's cursor back into the loop region.
//! * **LED filter (E00 / E01) IIR step** — mid-render filter toggling
//!   must not produce non-finite samples.
//! * **`EEy` pattern delay** — repeats the current row for `y` extra
//!   divisions without re-triggering held notes; nested delays must
//!   not unbound.
//!
//! Render is sized so a single fuzz iteration stays within
//! libfuzzer's default per-input time budget (~25 ms) on every input
//! the parser accepts.

use libfuzzer_sys::fuzz_target;
use oxideav_mod::header::parse_header;
use oxideav_mod::player::{parse_patterns, PlayerState};
use oxideav_mod::samples::extract_samples;

/// Render budget per fuzz iteration. 2048 frames stereo = ~46 ms at
/// 44.1 kHz, generous enough to step through several MOD rows on a
/// default `Fxx`-untouched speed-of-6 BPM-of-125 song so the effect
/// pipeline is actually exercised, small enough that the per-input
/// time budget stays well under libfuzzer's default.
const RENDER_FRAMES: usize = 2048;

fuzz_target!(|data: &[u8]| {
    let header = match parse_header(data) {
        Ok(h) => h,
        Err(_) => return,
    };

    // parse_patterns is infallible (truncated cells zero-fill).
    let patterns = parse_patterns(&header, data);
    let samples = extract_samples(&header, data);

    let mut player = PlayerState::new(&header, samples, patterns, 44_100);
    let mut buf = vec![0i16; RENDER_FRAMES * 2];
    let _ = player.render(&mut buf);
});
