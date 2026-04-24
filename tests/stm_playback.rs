//! STM playback integration test.
//!
//! Round-4 milestone: the shared tracker mixer core (pitch trait,
//! sample-source trait, generic voice) is wired through STM's row/tick
//! engine. We build a tiny hand-crafted STM file that:
//!   - Declares one instrument with a 64-sample square-wave body.
//!   - Sets its C3 reference to 8363 Hz (the canonical sampler rate).
//!   - Puts a single C-4 / instrument-1 note at row 0, channel 0.
//!
//! Then we drive the new `StmPlayerState` for ~0.1s and assert:
//!   - The run produces exactly the frames we asked for.
//!   - There are many non-zero samples (the square wave is audible).
//!   - The peak stays within a reasonable range (the mixer clamps to
//!     ±1.0 before the int16 conversion, so we should never see
//!     saturation throughout — but some hefty energy must be present).

use oxideav_mod::{
    stm::{extract_samples, parse_header, parse_patterns},
    stm_player::StmPlayerState,
};

fn build_c4_square_stm() -> Vec<u8> {
    const HEADER_PREFIX: usize = 0x30;
    const ORDER_OFF: usize = 0x3D0;
    const ORDER_SIZE: usize = 64;
    const PATTERN_OFF: usize = 0x410;
    const BYTES_PER_PATTERN: usize = 64 * 4 * 4;

    let n_patterns = 1u8;
    let mut out = vec![0u8; PATTERN_OFF];
    out[0..4].copy_from_slice(b"pbck");
    out[0x14..0x1C].copy_from_slice(b"!Scream!");
    out[0x1C] = 0x1A;
    out[0x1D] = 2; // module
    out[0x1E] = 2;
    out[0x1F] = 0;
    out[0x20] = 0x60; // tempo
    out[0x21] = n_patterns;
    out[0x22] = 64; // global volume

    // Instrument 0.
    let inst_off = HEADER_PREFIX;
    out[inst_off..inst_off + 3].copy_from_slice(b"sqr");
    // Sample length = 64 bytes. Forward loop 0..64 so the square
    // wave keeps going for the entire render window (without this the
    // note's audible tail is only ~337 frames).
    out[inst_off + 16..inst_off + 18].copy_from_slice(&64u16.to_le_bytes());
    out[inst_off + 18..inst_off + 20].copy_from_slice(&0u16.to_le_bytes());
    out[inst_off + 20..inst_off + 22].copy_from_slice(&64u16.to_le_bytes());
    // Default volume + C3 frequency.
    out[inst_off + 22] = 64;
    out[inst_off + 24..inst_off + 26].copy_from_slice(&8363u16.to_le_bytes());

    // Order table.
    for i in 0..ORDER_SIZE {
        out[ORDER_OFF + i] = if i == 0 { 0 } else { 255 };
    }

    // Pattern 0.
    let mut pattern = vec![0u8; BYTES_PER_PATTERN];
    // Row 0 / channel 0: note byte 0x40 (octave 4, semitone 0 → C-4).
    pattern[0] = 0x40;
    // Byte 1: instrument 1 (high 5 bits) | vol_lo 0 = 0x08.
    pattern[1] = (1u8) << 3;
    pattern[2] = 0;
    pattern[3] = 0;
    out.extend(pattern);

    // Square-wave body: 32 samples at +100, 32 samples at -100.
    for i in 0..64 {
        let v: i8 = if i < 32 { 100 } else { -100 };
        out.push(v as u8);
    }
    out
}

#[test]
fn stm_player_renders_audible_square_wave() {
    let bytes = build_c4_square_stm();
    let hdr = parse_header(&bytes).expect("stm header");
    let pats = parse_patterns(&hdr, &bytes);
    let samples = extract_samples(&hdr, &bytes);

    let mut p = StmPlayerState::new(&hdr, samples, pats, 44_100);
    let n_frames = 4410; // 0.1 s
    let mut buf = vec![0i16; n_frames * 2];
    let produced = p.render(&mut buf);
    assert_eq!(produced, n_frames, "must fill the requested frames");

    // The sample is a 64-point one-shot played at C-4 (≈ 8363 Hz),
    // output at 44_100 Hz → ~337 output frames worth of audible signal
    // before the one-shot ends. We assert at least ~half of that in
    // non-zero samples (interpolation introduces some zero crossings at
    // the square-wave transitions).
    let nonzero = buf.iter().filter(|&&x| x != 0).count();
    assert!(
        nonzero > 100,
        "expected audible square wave (>~100 non-zero samples), got {nonzero}"
    );

    // Peak must lie in a sensible post-volume range: with headroom
    // scaling dividing by 2 and a square ±100/128, we expect |peak| ≈
    // 0.39 → ~12800 in i16. Give a wide tolerance.
    let peak = buf.iter().map(|&s| s.unsigned_abs() as u32).max().unwrap();
    assert!(
        peak > 1000 && peak < 32767,
        "expected peak within (1000, 32767), got {peak}"
    );
}

#[test]
fn stm_player_silent_on_empty_pattern() {
    // Same header as above but without the note trigger. No note, no
    // instrument — the voice never starts, so the buffer must be all
    // zeros.
    let mut bytes = build_c4_square_stm();
    // Clear the single live cell to "empty" (note byte 251 per spec).
    // Pattern starts at offset 0x410 + row0/ch0 = 0x410.
    bytes[0x410] = 251;
    bytes[0x411] = 0;
    bytes[0x412] = 0;
    bytes[0x413] = 0;

    let hdr = parse_header(&bytes).unwrap();
    let pats = parse_patterns(&hdr, &bytes);
    let samples = extract_samples(&hdr, &bytes);
    let mut p = StmPlayerState::new(&hdr, samples, pats, 44_100);

    let mut buf = vec![0i16; 4410 * 2];
    let produced = p.render(&mut buf);
    assert_eq!(produced, 4410);
    assert!(
        buf.iter().all(|&x| x == 0),
        "expected pure silence on no-trigger pattern"
    );
}
