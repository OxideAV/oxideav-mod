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

// ---- Round 9: vibrato / tone porta / pattern break tests ----

const STM_HEADER_PREFIX: usize = 0x30;
const STM_ORDER_OFF: usize = 0x3D0;
const STM_ORDER_SIZE: usize = 64;
const STM_PATTERN_OFF: usize = 0x410;
const STM_BYTES_PER_PATTERN: usize = 64 * 4 * 4;

/// Write an STM cell into an in-memory pattern block at `(row, ch)`.
///
/// `note_raw` is the raw note byte (0x40 = C-4, 251 = empty, etc.).
/// `instrument` is 1-based (0 = no change).
/// `volume` is 0..=64 (0 = no override).
/// `command` + `command_param` are ProTracker-style 0..=F and 0..=FF.
#[allow(clippy::too_many_arguments)]
fn write_cell(
    block: &mut [u8],
    row: usize,
    ch: usize,
    note_raw: u8,
    instrument: u8,
    volume: u8,
    command: u8,
    command_param: u8,
) {
    let off = row * 4 * 4 + ch * 4;
    block[off] = note_raw;
    // b1 = (instrument << 3) | (vol_lo & 0x07)
    block[off + 1] = (instrument << 3) | (volume & 0x07);
    // b2 = ((vol_hi & 0x07) << 4) | (command & 0x0F)
    block[off + 2] = (((volume >> 3) & 0x07) << 4) | (command & 0x0F);
    block[off + 3] = command_param;
}

/// Build a 256-sample sine body as an STM sample (signed i8).
fn make_sine_body_256() -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    for i in 0..256 {
        let t = (i as f32) / 256.0;
        let v = (96.0 * (2.0 * std::f32::consts::PI * t).sin()) as i8;
        out.push(v as u8);
    }
    out
}

/// Build an STM with `n_patterns` patterns, a single sine instrument
/// (looped 0..=256 forward), and optional command/param on row 0 ch 0
/// alongside a note.
///
/// `build_row0_cell` receives the 1024-byte pattern block for pattern 0
/// and writes into it; later patterns / rows are zero-filled.
fn build_stm_with_pattern<F>(n_patterns: u8, build_row0_cell: F) -> Vec<u8>
where
    F: FnOnce(&mut [u8]),
{
    let mut out = vec![0u8; STM_PATTERN_OFF];
    out[0..4].copy_from_slice(b"test");
    out[0x14..0x1C].copy_from_slice(b"!Scream!");
    out[0x1C] = 0x1A;
    out[0x1D] = 2; // module
    out[0x1E] = 2;
    out[0x1F] = 0;
    out[0x20] = 0x60; // tempo
    out[0x21] = n_patterns;
    out[0x22] = 64; // global volume

    // Instrument 0: sample length = 256 bytes, forward loop full body.
    let inst_off = STM_HEADER_PREFIX;
    out[inst_off..inst_off + 3].copy_from_slice(b"sin");
    out[inst_off + 16..inst_off + 18].copy_from_slice(&256u16.to_le_bytes());
    out[inst_off + 18..inst_off + 20].copy_from_slice(&0u16.to_le_bytes());
    out[inst_off + 20..inst_off + 22].copy_from_slice(&256u16.to_le_bytes());
    out[inst_off + 22] = 64;
    out[inst_off + 24..inst_off + 26].copy_from_slice(&8363u16.to_le_bytes());

    // Order: pattern 0, 1, ... up to n_patterns; the rest 0xFF-terminated.
    for i in 0..STM_ORDER_SIZE {
        out[STM_ORDER_OFF + i] = if (i as u8) < n_patterns { i as u8 } else { 255 };
    }

    // Build patterns. Pattern 0 gets the caller-supplied row-0 setup.
    // Default-fill every cell with note_raw=251 ("empty") so any row
    // the caller doesn't overwrite doesn't accidentally retrigger the
    // note_raw=0 (= C-0) cell.
    let mut pat0 = vec![0u8; STM_BYTES_PER_PATTERN];
    for row in 0..64usize {
        for ch in 0..4usize {
            pat0[row * 16 + ch * 4] = 251;
        }
    }
    build_row0_cell(&mut pat0);
    out.extend(pat0);
    for _ in 1..n_patterns {
        let mut patn = vec![0u8; STM_BYTES_PER_PATTERN];
        for row in 0..64usize {
            for ch in 0..4usize {
                patn[row * 16 + ch * 4] = 251;
            }
        }
        out.extend(patn);
    }

    // Sample body: 256-sample sine.
    out.extend(make_sine_body_256());
    out
}

/// Approximate DFT magnitude at a target frequency. Good enough for
/// correctness checks (not a FFT — O(N) per frequency).
fn dft_mag(signal: &[f32], sample_rate: f32, freq_hz: f32) -> f32 {
    let n = signal.len() as f32;
    if n < 1.0 {
        return 0.0;
    }
    let w = 2.0 * std::f32::consts::PI * freq_hz / sample_rate;
    let mut re = 0.0f32;
    let mut im = 0.0f32;
    for (i, &s) in signal.iter().enumerate() {
        let phi = w * i as f32;
        re += s * phi.cos();
        im -= s * phi.sin();
    }
    (re * re + im * im).sqrt() / n
}

/// Extract the left channel of interleaved stereo i16 as f32.
fn left_channel(buf: &[i16]) -> Vec<f32> {
    buf.chunks_exact(2).map(|c| c[0] as f32).collect()
}

/// Vibrato (4xy) on a single sustained note must produce detectable
/// off-carrier energy compared with the same note played dry. Mirrors
/// the round-6 XM vibrato spectral probe.
#[test]
fn stm_player_vibrato_produces_spectral_sidebands() {
    // Row 0, ch 0: note C-6 (octave 6, semitone 0 → note_raw 0x60),
    // instrument 1, effect 4 (vibrato), param 0x6F (speed 6, depth F).
    // Expected carrier: 8363 Hz native, played at C-6 (+3 octaves from
    // C-3) → 66904 Hz sample step, through a 256-sample loop → 261 Hz
    // output fundamental at 44_100 Hz rendering. Aliasing in the output
    // is OK — the test measures *relative* energy at a single probe.
    let vib_bytes = build_stm_with_pattern(1, |pat| {
        // note 0x60 = octave 6, semitone 0 (C-6).
        write_cell(pat, 0, 0, 0x60, 1, 0, 0x4, 0x6F);
    });
    let hdr = parse_header(&vib_bytes).expect("stm header");
    let pats = parse_patterns(&hdr, &vib_bytes);
    let samples = extract_samples(&hdr, &vib_bytes);
    let mut p = StmPlayerState::new(&hdr, samples, pats, 44_100);

    // Render ~0.5 s.
    let n_frames = 22_050;
    let mut buf_vib = vec![0i16; n_frames * 2];
    let produced = p.render(&mut buf_vib);
    assert_eq!(produced, n_frames);
    let signal_vib = left_channel(&buf_vib);
    let rms_vib = (signal_vib.iter().map(|&x| x * x).sum::<f32>() / signal_vib.len() as f32).sqrt();
    assert!(rms_vib > 100.0, "vibrato signal RMS too low: {rms_vib}");

    // Control: same note, no effect.
    let dry_bytes = build_stm_with_pattern(1, |pat| {
        write_cell(pat, 0, 0, 0x60, 1, 0, 0, 0);
    });
    let hdr2 = parse_header(&dry_bytes).expect("stm header");
    let pats2 = parse_patterns(&hdr2, &dry_bytes);
    let samples2 = extract_samples(&hdr2, &dry_bytes);
    let mut p2 = StmPlayerState::new(&hdr2, samples2, pats2, 44_100);
    let mut buf_dry = vec![0i16; n_frames * 2];
    p2.render(&mut buf_dry);
    let signal_dry = left_channel(&buf_dry);

    // Carrier frequency for C-6 at 8363 Hz C-3 reference: 8363 * 2^3 =
    // 66904 Hz sample-rate, looped in a 256-frame table → fundamental
    // 66904 / 256 ≈ 261.3 Hz (modulo output-rate aliasing).
    let carrier: f32 = 8363.0 * 8.0 / 256.0;
    // Probe ~30 Hz below the carrier — vibrato at speed 6 / depth 15
    // should push substantial energy here.
    let probe_off: f32 = (carrier - 30.0).max(10.0);
    let sb_vib = dft_mag(&signal_vib, 44_100.0, probe_off);
    let sb_dry = dft_mag(&signal_dry, 44_100.0, probe_off);
    assert!(
        sb_vib > sb_dry * 1.5,
        "expected vibrato to boost off-carrier energy (with={sb_vib}, without={sb_dry}, carrier={carrier}, probe={probe_off})"
    );
}

/// Tone portamento: row 0 triggers C-4, row 1 fires 3FF toward G-4.
/// After a row of 6 ticks at speed 0xFF the channel's `cur_semis`
/// should have snapped to the G-4 target without retriggering the
/// voice.
#[test]
fn stm_player_tone_porta_reaches_target() {
    // Row 0 ch 0: note C-4 (octave 4, semitone 0 → 0x40), inst 1.
    // Row 1 ch 0: note G-4 (octave 4, semitone 7 → 0x47), command 3,
    // param 0xFF (very large porta speed).
    let bytes = build_stm_with_pattern(1, |pat| {
        write_cell(pat, 0, 0, 0x40, 1, 0, 0, 0);
        write_cell(pat, 1, 0, 0x47, 0, 0, 0x3, 0xFF);
    });
    let hdr = parse_header(&bytes).expect("stm header");
    let pats = parse_patterns(&hdr, &bytes);
    let samples = extract_samples(&hdr, &bytes);
    let mut p = StmPlayerState::new(&hdr, samples, pats, 44_100);

    // Render past row 1. STM samples-per-tick ≈ 44_100 * 2.5 / bpm ≈
    // 882 at tempo 0x60 (bpm_equiv 125). 12 ticks × 882 ≈ 10_584.
    let n_frames = 12_000;
    let mut buf = vec![0i16; n_frames * 2];
    p.render(&mut buf);

    // Expect the channel's current semitone to equal G-4 (4*12 + 7 = 55).
    let ch = &p.channels[0];
    let target = 4.0 * 12.0 + 7.0;
    assert!(
        (ch.cur_semis - target).abs() < 0.1,
        "expected tone porta to reach G-4 semitone {target}, got {}",
        ch.cur_semis
    );
    // Voice should still be active — tone porta must not retrigger.
    assert!(ch.voice.active, "tone-porta must not kill the voice");
}

/// Pattern break (Dxy) on row 0 of pattern 0 must advance to row
/// `x*10 + y` of pattern 1 (FT2-style decimal, mirrors XM parity).
#[test]
fn stm_player_pattern_break_lands_on_correct_row() {
    // Pattern 0: row 0 fires D04 (jump to row 4 of next pattern).
    // Two-pattern order so we can observe the landing in-song.
    let bytes = build_stm_with_pattern(2, |pat| {
        // No note — pure control cell.
        write_cell(pat, 0, 0, 251, 0, 0, 0xD, 0x04);
    });
    let hdr = parse_header(&bytes).expect("stm header");
    let pats = parse_patterns(&hdr, &bytes);
    let samples = extract_samples(&hdr, &bytes);
    let mut p = StmPlayerState::new(&hdr, samples, pats, 44_100);

    // Render 7 ticks worth — enough for row 0 (6 ticks) to complete
    // and next_row to fire the pattern break.
    let n_frames = 7 * 882;
    let mut buf = vec![0i16; n_frames * 2];
    p.render(&mut buf);
    assert_eq!(
        p.order_index, 1,
        "Dxy should advance to next order (got {})",
        p.order_index
    );
    assert_eq!(
        p.row, 4,
        "Dxy row=04 should land on row 4 of next pattern (got {})",
        p.row
    );
}
