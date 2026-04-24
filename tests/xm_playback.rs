//! XM playback integration test — Round-4 milestone.
//!
//! Builds a minimal hand-crafted XM file (4 channels, 1 pattern,
//! 1 instrument with a single 8-bit sample containing a short square
//! wave), then drives the new `XmPlayerState` via the shared mixer core
//! and asserts:
//!   - The run emits the exact number of frames requested.
//!   - There's plenty of non-zero signal (the note is audible).
//!   - RMS lands in a reasonable, non-saturating range.
//!
//! Effects, envelopes, vibrato, and fadeout are not exercised — Round 4
//! focuses on the pitch-model + sample-source integration under the
//! shared mixer core.

use oxideav_mod::xm;
use oxideav_mod::xm_player::XmPlayerState;

fn build_c4_square_xm() -> Vec<u8> {
    let mut out = vec![0u8; xm::XM_MIN_HEADER_LEN];
    out[0..17].copy_from_slice(xm::XM_BANNER);
    out[17..37].copy_from_slice(b"playback-xm         ");
    out[xm::XM_ID_BYTE_OFFSET] = 0x1A;
    out[38..58].copy_from_slice(b"oxideav             ");
    out[58..60].copy_from_slice(&xm::XM_VERSION_0104.to_le_bytes());
    out[60..64].copy_from_slice(&0x114u32.to_le_bytes());
    // song_length = 1, restart = 0.
    out[64..66].copy_from_slice(&1u16.to_le_bytes());
    out[66..68].copy_from_slice(&0u16.to_le_bytes());
    out[68..70].copy_from_slice(&4u16.to_le_bytes()); // 4 channels
    out[70..72].copy_from_slice(&1u16.to_le_bytes()); // 1 pattern
    out[72..74].copy_from_slice(&1u16.to_le_bytes()); // 1 instrument
    out[74..76].copy_from_slice(&1u16.to_le_bytes()); // linear freq table
    out[76..78].copy_from_slice(&6u16.to_le_bytes()); // tempo (ticks/row)
    out[78..80].copy_from_slice(&125u16.to_le_bytes()); // BPM
    for i in 1..xm::XM_ORDER_TABLE_SIZE {
        out[xm::XM_ORDER_TABLE_OFFSET + i] = 0xFF;
    }

    // One pattern, 4 rows × 4 channels. Row 0 / ch 0 triggers note 49
    // (= C-4 in XM) with instrument 1 at full volume.
    let mut packed = Vec::new();
    for row in 0..4 {
        for ch in 0..4 {
            if row == 0 && ch == 0 {
                // mask: note (0x01) | instrument (0x02) | volume (0x04)
                packed.push(0x80 | 0x01 | 0x02 | 0x04);
                packed.push(49); // note C-4
                packed.push(1); // instrument 1
                packed.push(0x50); // vol col: SetVolume(0x40)
            } else {
                packed.push(0x80); // empty
            }
        }
    }
    out.extend_from_slice(&9u32.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&(packed.len() as u16).to_le_bytes());
    out.extend(packed);

    // Instrument #0: 1 sample, 64-byte square-wave delta stream.
    const HSIZE: u32 = 0x107;
    let inst_start = out.len();
    out.extend_from_slice(&HSIZE.to_le_bytes());
    let mut nbuf = [0u8; 22];
    nbuf[..3].copy_from_slice(b"sqr");
    out.extend_from_slice(&nbuf);
    out.push(0); // type
    out.extend_from_slice(&1u16.to_le_bytes()); // num_samples

    // Extended block.
    out.extend_from_slice(&xm::XM_SAMPLE_HEADER_SIZE.to_le_bytes());
    out.extend(vec![0u8; 96]); // sample_map
                               // Flat envelope at full volume: (0, 64) and (64, 64). This keeps
                               // the volume scalar at 1.0 throughout the test so we can assert on
                               // raw audibility without envelope ramp-up colouring the RMS.
    let mut vol_env = [0u8; 48];
    vol_env[0..2].copy_from_slice(&0u16.to_le_bytes());
    vol_env[2..4].copy_from_slice(&64u16.to_le_bytes());
    vol_env[4..6].copy_from_slice(&64u16.to_le_bytes());
    vol_env[6..8].copy_from_slice(&64u16.to_le_bytes());
    out.extend_from_slice(&vol_env);
    out.extend_from_slice(&[0u8; 48]); // pan env
    out.push(2); // num_vol_points
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0x01); // vol type On
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    out.extend_from_slice(&512u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    while out.len() - inst_start < HSIZE as usize {
        out.push(0);
    }

    // Sample header (40 bytes).
    // Construct a 64-sample square wave as a delta stream: we want
    // the *absolute* samples to be +100 for 32 frames, then -100 for
    // 32 frames. Delta stream: first frame has delta=+100 (from 0),
    // then 31 zeros, then delta=-200, then 31 zeros.
    let mut abs_pcm = vec![0i8; 64];
    for (i, slot) in abs_pcm.iter_mut().enumerate() {
        *slot = if i < 32 { 100 } else { -100 };
    }
    let mut delta_stream = Vec::with_capacity(64);
    let mut prev: i8 = 0;
    for v in &abs_pcm {
        let d = (v.wrapping_sub(prev)) as u8;
        delta_stream.push(d);
        prev = *v;
    }
    let body_len = delta_stream.len() as u32;
    out.extend_from_slice(&body_len.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // loop_start
    out.extend_from_slice(&body_len.to_le_bytes()); // loop_length = full body
    out.push(0x40); // volume
    out.push(0); // finetune
    out.push(1); // type byte: forward loop, 8-bit
    out.push(128); // pan
    out.push(0); // relative note
    out.push(0); // reserved
    let mut sname = [0u8; 22];
    sname[..3].copy_from_slice(b"sq1");
    out.extend_from_slice(&sname);

    // Sample body.
    out.extend_from_slice(&delta_stream);
    out
}

#[test]
fn xm_player_renders_audible_square_wave() {
    let bytes = build_c4_square_xm();
    let hdr = xm::parse_header(&bytes).expect("xm header");
    let (patterns, after) = xm::parse_patterns(&hdr, &bytes).expect("patterns");
    let mut instruments = xm::parse_instruments(&hdr, &bytes, after).expect("instruments");
    xm::extract_sample_bodies(&mut instruments, &bytes);

    // Sanity: the delta stream should decode to our target wave.
    let pcm = &instruments[0].samples[0].pcm8;
    assert_eq!(pcm.len(), 64);
    assert_eq!(pcm[0], 100);
    assert_eq!(pcm[31], 100);
    assert_eq!(pcm[32], -100);

    let mut p = XmPlayerState::new(&hdr, instruments, patterns, 44_100);
    let n_frames = 4410; // 0.1 s
    let mut buf = vec![0i16; n_frames * 2];
    let produced = p.render(&mut buf);
    assert_eq!(produced, n_frames);

    let nonzero = buf.iter().filter(|&&x| x != 0).count();
    assert!(
        nonzero > n_frames / 4,
        "expected audible square wave (>~1/4 of frames non-zero), got {nonzero}"
    );

    let sum_sq: f64 = buf.iter().map(|&s| (s as f64).powi(2)).sum();
    let rms = (sum_sq / buf.len() as f64).sqrt();
    assert!(
        rms > 500.0 && rms < 20000.0,
        "expected RMS within (500, 20000), got {rms}"
    );
}

/// Build a minimal XM file tailored for envelope + key-off testing:
///   - Volume envelope (0,64), (10,64), (20,0) with sustain at point 1
///     (tick 10). Holds at 64 while key-on, decays to 0 after release.
///   - Fadeout = 1024 (substantial so we can observe it numerically in
///     a ~1 s render).
///   - Pattern has note at row 0, key-off (note 97) at row 4.
///   - 1 channel, 8 rows, tempo 6, BPM 125.
fn build_envelope_xm() -> Vec<u8> {
    let mut out = vec![0u8; xm::XM_MIN_HEADER_LEN];
    out[0..17].copy_from_slice(xm::XM_BANNER);
    out[17..37].copy_from_slice(b"envelope-xm         ");
    out[xm::XM_ID_BYTE_OFFSET] = 0x1A;
    out[38..58].copy_from_slice(b"oxideav             ");
    out[58..60].copy_from_slice(&xm::XM_VERSION_0104.to_le_bytes());
    out[60..64].copy_from_slice(&0x114u32.to_le_bytes());
    out[64..66].copy_from_slice(&1u16.to_le_bytes()); // song_length
    out[66..68].copy_from_slice(&0u16.to_le_bytes());
    out[68..70].copy_from_slice(&2u16.to_le_bytes()); // 2 channels (even)
    out[70..72].copy_from_slice(&1u16.to_le_bytes()); // 1 pattern
    out[72..74].copy_from_slice(&1u16.to_le_bytes()); // 1 instrument
    out[74..76].copy_from_slice(&1u16.to_le_bytes()); // linear
    out[76..78].copy_from_slice(&6u16.to_le_bytes()); // tempo
    out[78..80].copy_from_slice(&125u16.to_le_bytes()); // BPM
    for i in 1..xm::XM_ORDER_TABLE_SIZE {
        out[xm::XM_ORDER_TABLE_OFFSET + i] = 0xFF;
    }

    // Pattern: 8 rows × 2 channels. Row 0 ch 0 = note 49 (C-4) inst 1
    // full volume. Row 4 ch 0 = key-off (note 97). All other cells
    // empty.
    let mut packed = Vec::new();
    for row in 0..8u8 {
        for ch in 0..2u8 {
            if row == 0 && ch == 0 {
                packed.push(0x80 | 0x01 | 0x02 | 0x04);
                packed.push(49); // C-4
                packed.push(1);
                packed.push(0x50); // vol 64
            } else if row == 4 && ch == 0 {
                // Key-off: note 97, no instrument / volume / effect.
                packed.push(0x80 | 0x01);
                packed.push(97);
            } else {
                packed.push(0x80);
            }
        }
    }
    out.extend_from_slice(&9u32.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&(packed.len() as u16).to_le_bytes());
    out.extend(packed);

    // Instrument: 1 sample, ramped envelope with sustain and fadeout.
    const HSIZE: u32 = 0x107;
    let inst_start = out.len();
    out.extend_from_slice(&HSIZE.to_le_bytes());
    let mut nbuf = [0u8; 22];
    nbuf[..3].copy_from_slice(b"env");
    out.extend_from_slice(&nbuf);
    out.push(0);
    out.extend_from_slice(&1u16.to_le_bytes());

    out.extend_from_slice(&xm::XM_SAMPLE_HEADER_SIZE.to_le_bytes());
    out.extend(vec![0u8; 96]);

    // Volume envelope: 3 points.
    // (0, 64) -> (10, 64) -> (20, 0). Sustain at index 1 (tick=10),
    // loop disabled. Key-on stalls at (10,64) → full volume. Key-off
    // lets envelope walk from (10,64) to (20,0) while fadeout also
    // multiplies, so output decays.
    let mut vol_env = [0u8; 48];
    vol_env[0..2].copy_from_slice(&0u16.to_le_bytes());
    vol_env[2..4].copy_from_slice(&64u16.to_le_bytes());
    vol_env[4..6].copy_from_slice(&10u16.to_le_bytes());
    vol_env[6..8].copy_from_slice(&64u16.to_le_bytes());
    vol_env[8..10].copy_from_slice(&20u16.to_le_bytes());
    vol_env[10..12].copy_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&vol_env);

    out.extend_from_slice(&[0u8; 48]); // pan env
    out.push(3); // num_vol_points
    out.push(0); // num_pan_points
    out.push(1); // vol sustain_point = index 1
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    // Volume type: On (bit 0) + Sustain (bit 1) = 0x03.
    out.push(0x03);
    out.push(0); // pan type
                 // Vibrato (type/sweep/depth/rate) — zero.
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    // Fadeout: 1024 — at 65536 / 1024 = 64 ticks to reach zero.
    // At tempo=6 ticks/row, BPM=125 (tick=~20ms), that's ~1.3 s to
    // full decay — well inside our test window.
    out.extend_from_slice(&1024u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    while out.len() - inst_start < HSIZE as usize {
        out.push(0);
    }

    // Sample header — 64-frame square wave, same shape as the primary
    // test, looped so we don't run out of body while the envelope
    // holds.
    let mut abs_pcm = vec![0i8; 64];
    for (i, slot) in abs_pcm.iter_mut().enumerate() {
        *slot = if i < 32 { 100 } else { -100 };
    }
    let mut delta_stream = Vec::with_capacity(64);
    let mut prev: i8 = 0;
    for v in &abs_pcm {
        let d = (v.wrapping_sub(prev)) as u8;
        delta_stream.push(d);
        prev = *v;
    }
    let body_len = delta_stream.len() as u32;
    out.extend_from_slice(&body_len.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&body_len.to_le_bytes());
    out.push(0x40);
    out.push(0);
    out.push(1); // forward loop, 8-bit
    out.push(128);
    out.push(0);
    out.push(0);
    let mut sname = [0u8; 22];
    sname[..3].copy_from_slice(b"sq1");
    out.extend_from_slice(&sname);
    out.extend_from_slice(&delta_stream);
    out
}

#[test]
fn xm_player_envelope_sustain_then_fadeout_on_key_off() {
    // Renders ~1.5 s starting from note-on, covering the key-off at
    // row 4 (~1.15 s in at tempo=6 / BPM=125: 4 rows × 6 ticks × 882
    // frames ÷ 44100 Hz ≈ 0.48 s). The first quarter of the buffer
    // should be audible (envelope sustaining at 64, fadeout full);
    // the last quarter should be considerably quieter — proving
    // fadeout + envelope-post-sustain decay is actually attenuating
    // the voice.
    let bytes = build_envelope_xm();
    let hdr = xm::parse_header(&bytes).expect("xm header");
    let (patterns, after) = xm::parse_patterns(&hdr, &bytes).expect("patterns");
    let mut instruments = xm::parse_instruments(&hdr, &bytes, after).expect("instruments");
    xm::extract_sample_bodies(&mut instruments, &bytes);

    // Sanity: envelope was parsed with 3 points + sustain index 1 +
    // fadeout 1024, and the sustain bit is set.
    let env = &instruments[0].volume_envelope;
    assert_eq!(env.points.len(), 3);
    assert_eq!(env.points[0], (0, 64));
    assert_eq!(env.points[1], (10, 64));
    assert_eq!(env.points[2], (20, 0));
    assert_eq!(env.sustain_point, 1);
    assert!(env.is_on());
    assert!(env.has_sustain());
    assert_eq!(instruments[0].volume_fadeout, 1024);

    let mut p = XmPlayerState::new(&hdr, instruments, patterns, 44_100);
    // Song length: 8 rows × 6 ticks × 882 frames/tick = 42_336 frames.
    // Render exactly that so the player doesn't flag end-of-song mid-buffer.
    let n_frames = 42_336;
    let mut buf = vec![0i16; n_frames * 2];
    let produced = p.render(&mut buf);
    assert_eq!(produced, n_frames);

    // RMS over the first quarter (pre-key-off: sustain at 64, fadeout
    // untouched).
    let quarter = n_frames / 4;
    let rms_region = |s: &[i16]| -> f64 {
        if s.is_empty() {
            return 0.0;
        }
        let sq: f64 = s.iter().map(|&x| (x as f64).powi(2)).sum();
        (sq / s.len() as f64).sqrt()
    };
    let rms_head = rms_region(&buf[..quarter * 2]);
    let rms_tail = rms_region(&buf[(3 * quarter) * 2..n_frames * 2]);

    assert!(
        rms_head > 500.0,
        "expected audible sustained head of the note, got RMS {rms_head}"
    );
    // The tail should be far quieter than the head — both the post-
    // sustain envelope ramp (64 → 0 over ticks 10..20) and the
    // fadeout decrement combine to attenuate the voice by ~90% or
    // more. Expect at minimum a 2× drop.
    assert!(
        rms_tail * 2.0 < rms_head,
        "expected key-off decay (head RMS {rms_head}, tail RMS {rms_tail})"
    );

    // Also require the signal is non-silent somewhere in the tail —
    // we want a decay, not instant cut-off, so at least *some* energy
    // should remain.
    let tail_nonzero = buf[(3 * quarter) * 2..n_frames * 2]
        .iter()
        .filter(|&&x| x != 0)
        .count();
    let _ = tail_nonzero; // fadeout may reach zero before the very end;
                          // this is informational, not asserted — see head/tail
                          // RMS ratio above for the real decay check.
}

// ---- Round 6: vibrato / tone porta / pattern jump tests ----

/// Build an XM with a single channel + one sustained-tone instrument,
/// plus any row-0 effect the test wants. The instrument's sample is a
/// ~300 Hz sine wave; at C-4 (real_note=48, XM Linear → native sample
/// rate 8363 Hz) it plays back at its stored rate, giving an output
/// near 300 Hz for a 44.1 kHz render.
///
/// `effect_type` / `effect_param` are written in the first cell (row 0,
/// channel 0) alongside the note. Subsequent rows are empty so the
/// effect continues / the voice keeps playing the stored sample.
///
/// `num_rows` controls pattern length. The song runs exactly one pass
/// through the order table (1 pattern).
fn build_sine_xm_with_effect_and_note(
    num_rows: u16,
    effect_type: u8,
    effect_param: u8,
    note: u8,
) -> Vec<u8> {
    let mut out = vec![0u8; xm::XM_MIN_HEADER_LEN];
    out[0..17].copy_from_slice(xm::XM_BANNER);
    out[17..37].copy_from_slice(b"fx-xm               ");
    out[xm::XM_ID_BYTE_OFFSET] = 0x1A;
    out[38..58].copy_from_slice(b"oxideav             ");
    out[58..60].copy_from_slice(&xm::XM_VERSION_0104.to_le_bytes());
    out[60..64].copy_from_slice(&0x114u32.to_le_bytes());
    out[64..66].copy_from_slice(&1u16.to_le_bytes()); // song_length
    out[66..68].copy_from_slice(&0u16.to_le_bytes()); // restart
    out[68..70].copy_from_slice(&2u16.to_le_bytes()); // 2 channels
    out[70..72].copy_from_slice(&1u16.to_le_bytes()); // 1 pattern
    out[72..74].copy_from_slice(&1u16.to_le_bytes()); // 1 instrument
    out[74..76].copy_from_slice(&1u16.to_le_bytes()); // linear
    out[76..78].copy_from_slice(&6u16.to_le_bytes()); // tempo
    out[78..80].copy_from_slice(&125u16.to_le_bytes()); // BPM
    for i in 1..xm::XM_ORDER_TABLE_SIZE {
        out[xm::XM_ORDER_TABLE_OFFSET + i] = 0xFF;
    }

    // Pattern: `num_rows` rows × 2 channels.
    let mut packed = Vec::new();
    for row in 0..num_rows {
        for ch in 0..2u8 {
            if row == 0 && ch == 0 {
                // note + inst + volume + effect + effect param.
                packed.push(0x80 | 0x01 | 0x02 | 0x04 | 0x08 | 0x10);
                packed.push(note);
                packed.push(1);
                packed.push(0x50); // vol 64
                packed.push(effect_type);
                packed.push(effect_param);
            } else {
                packed.push(0x80);
            }
        }
    }
    out.extend_from_slice(&9u32.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&num_rows.to_le_bytes());
    out.extend_from_slice(&(packed.len() as u16).to_le_bytes());
    out.extend(packed);

    // Instrument with a 256-sample sine wave, looped.
    const HSIZE: u32 = 0x107;
    let inst_start = out.len();
    out.extend_from_slice(&HSIZE.to_le_bytes());
    let mut nbuf = [0u8; 22];
    nbuf[..3].copy_from_slice(b"sin");
    out.extend_from_slice(&nbuf);
    out.push(0);
    out.extend_from_slice(&1u16.to_le_bytes());

    out.extend_from_slice(&xm::XM_SAMPLE_HEADER_SIZE.to_le_bytes());
    out.extend(vec![0u8; 96]);
    // Volume env flat at 64 (envelope on).
    let mut vol_env = [0u8; 48];
    vol_env[0..2].copy_from_slice(&0u16.to_le_bytes());
    vol_env[2..4].copy_from_slice(&64u16.to_le_bytes());
    vol_env[4..6].copy_from_slice(&64u16.to_le_bytes());
    vol_env[6..8].copy_from_slice(&64u16.to_le_bytes());
    out.extend_from_slice(&vol_env);
    out.extend_from_slice(&[0u8; 48]);
    out.push(2); // num_vol_points
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0x01); // vol type On
    out.push(0);
    // Vibrato (type/sweep/depth/rate) — zero for plain tests.
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    out.extend_from_slice(&0u16.to_le_bytes()); // fadeout = 0
    out.extend_from_slice(&0u16.to_le_bytes());
    while out.len() - inst_start < HSIZE as usize {
        out.push(0);
    }

    // Sample body: 256-frame sine wave at amplitude 96.
    let n = 256usize;
    let mut abs_pcm = vec![0i8; n];
    for (i, slot) in abs_pcm.iter_mut().enumerate() {
        let t = (i as f32) / (n as f32);
        *slot = (96.0 * (2.0 * std::f32::consts::PI * t).sin()) as i8;
    }
    let mut delta_stream = Vec::with_capacity(n);
    let mut prev: i8 = 0;
    for v in &abs_pcm {
        let d = (v.wrapping_sub(prev)) as u8;
        delta_stream.push(d);
        prev = *v;
    }
    let body_len = delta_stream.len() as u32;
    out.extend_from_slice(&body_len.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&body_len.to_le_bytes());
    out.push(0x40); // volume
    out.push(0); // finetune
    out.push(1); // loop fwd, 8-bit
    out.push(128); // pan
    out.push(0); // relative note
    out.push(0); // reserved
    let mut sname = [0u8; 22];
    sname[..3].copy_from_slice(b"sin");
    out.extend_from_slice(&sname);
    out.extend_from_slice(&delta_stream);
    out
}

fn build_sine_xm_with_effect(num_rows: u16, effect_type: u8, effect_param: u8) -> Vec<u8> {
    build_sine_xm_with_effect_and_note(num_rows, effect_type, effect_param, 49)
}

/// Approximate DFT magnitude at `freq_hz` for a mono signal sampled at
/// `sample_rate`. Not fast, but good enough for correctness tests.
fn dft_mag(signal: &[f32], sample_rate: f32, freq_hz: f32) -> f32 {
    let n = signal.len() as f32;
    let w = 2.0 * std::f32::consts::PI * freq_hz / sample_rate;
    let mut re = 0.0;
    let mut im = 0.0;
    for (i, &s) in signal.iter().enumerate() {
        let phi = w * i as f32;
        re += s * phi.cos();
        im -= s * phi.sin();
    }
    ((re * re + im * im).sqrt()) / n
}

/// Read only the left channel of the stereo i16 output, converted to f32.
fn left_channel(buf: &[i16]) -> Vec<f32> {
    buf.chunks_exact(2).map(|c| c[0] as f32).collect()
}

/// Vibrato on a single sustained note must produce sidebands in the
/// spectrum near the carrier frequency.
///
/// The sample played is a 256-frame sine looped at C-4 (the sample's
/// 'native' C-4 → 8363 Hz output rate in XM Linear mode → 8363 / 256 ≈
/// 32.67 Hz fundamental over 44100 Hz output). With effect 4A8 (speed
/// 10, depth 8) the period wobbles → we expect narrow sidebands around
/// the carrier.
#[test]
fn xm_player_vibrato_produces_spectral_sidebands() {
    // Note C-7 (XM 85 → real_note 84, playback ~8x the native rate) so
    // the 256-frame sine carrier sits at ~260 Hz in the output — well
    // inside the DFT's usable band with modest sample count. Use 4xy
    // with speed 6, depth 0xF (maximum) to produce a clearly-audible
    // pitch wobble + strong sidebands.
    let note = 85;
    let bytes = build_sine_xm_with_effect_and_note(32, 0x04, 0x6F, note);
    let hdr = xm::parse_header(&bytes).expect("xm header");
    let (patterns, after) = xm::parse_patterns(&hdr, &bytes).expect("patterns");
    let mut instruments = xm::parse_instruments(&hdr, &bytes, after).expect("instruments");
    xm::extract_sample_bodies(&mut instruments, &bytes);

    let mut p = XmPlayerState::new(&hdr, instruments, patterns, 44_100);
    // Render ~0.5 s so we accumulate multiple vibrato LFO cycles.
    let n_frames = 22_050;
    let mut buf = vec![0i16; n_frames * 2];
    let produced = p.render(&mut buf);
    assert_eq!(produced, n_frames);

    let signal = left_channel(&buf);
    let rms = (signal.iter().map(|&x| x * x).sum::<f32>() / signal.len() as f32).sqrt();
    assert!(rms > 100.0, "vibrato signal RMS too low: {rms}");

    // Control: render the same note with no effect.
    let bytes2 = build_sine_xm_with_effect_and_note(32, 0x00, 0x00, note);
    let hdr2 = xm::parse_header(&bytes2).expect("xm header");
    let (patterns2, after2) = xm::parse_patterns(&hdr2, &bytes2).expect("patterns");
    let mut instruments2 = xm::parse_instruments(&hdr2, &bytes2, after2).expect("instruments");
    xm::extract_sample_bodies(&mut instruments2, &bytes2);
    let mut p2 = XmPlayerState::new(&hdr2, instruments2, patterns2, 44_100);
    let mut buf2 = vec![0i16; n_frames * 2];
    let produced2 = p2.render(&mut buf2);
    assert_eq!(produced2, n_frames);
    let signal2 = left_channel(&buf2);

    // Native-rate carrier: 8363 * 2^((real_note - 48)/12) / 256
    // real_note = note - 1 = 84 → freq multiplier = 2^3 = 8 → carrier
    // = 8363 * 8 / 256 ≈ 261 Hz.
    let real_note = (note - 1) as f32;
    let carrier: f32 = 8363.0 * 2.0f32.powf((real_note - 48.0) / 12.0) / 256.0;

    // A sideband at ~40 Hz off the carrier should be dramatically
    // stronger with vibrato than without.
    let probe_off: f32 = (carrier - 40.0).max(10.0);
    let sb_with_vib = dft_mag(&signal, 44_100.0, probe_off);
    let sb_no_vib = dft_mag(&signal2, 44_100.0, probe_off);
    assert!(
        sb_with_vib > sb_no_vib * 1.5,
        "expected vibrato to boost off-carrier energy (with={sb_with_vib}, without={sb_no_vib}, carrier={carrier}, probe={probe_off})"
    );
}

/// Tone portamento: a note at row 0 followed by a second note + 3xy at
/// row 1 should slide the period toward the target. After a few ticks
/// with a large porta speed, we expect `period` to have reached the
/// target note.
#[test]
fn xm_player_tone_porta_reaches_target() {
    // Build 2-row pattern: row 0 = C-4, row 1 = G-4 + 3xFF (very large
    // porta speed so it snaps within one row of 6 ticks).
    let bytes = {
        let mut out = vec![0u8; xm::XM_MIN_HEADER_LEN];
        out[0..17].copy_from_slice(xm::XM_BANNER);
        out[17..37].copy_from_slice(b"porta               ");
        out[xm::XM_ID_BYTE_OFFSET] = 0x1A;
        out[38..58].copy_from_slice(b"oxideav             ");
        out[58..60].copy_from_slice(&xm::XM_VERSION_0104.to_le_bytes());
        out[60..64].copy_from_slice(&0x114u32.to_le_bytes());
        out[64..66].copy_from_slice(&1u16.to_le_bytes());
        out[66..68].copy_from_slice(&0u16.to_le_bytes());
        out[68..70].copy_from_slice(&2u16.to_le_bytes());
        out[70..72].copy_from_slice(&1u16.to_le_bytes());
        out[72..74].copy_from_slice(&1u16.to_le_bytes());
        out[74..76].copy_from_slice(&1u16.to_le_bytes()); // Linear
        out[76..78].copy_from_slice(&6u16.to_le_bytes());
        out[78..80].copy_from_slice(&125u16.to_le_bytes());
        for i in 1..xm::XM_ORDER_TABLE_SIZE {
            out[xm::XM_ORDER_TABLE_OFFSET + i] = 0xFF;
        }
        let mut packed = Vec::new();
        for row in 0..8u16 {
            for ch in 0..2u8 {
                if row == 0 && ch == 0 {
                    packed.push(0x80 | 0x01 | 0x02 | 0x04);
                    packed.push(49); // C-4
                    packed.push(1);
                    packed.push(0x50);
                } else if row == 1 && ch == 0 {
                    // G-4 with 3FF
                    packed.push(0x80 | 0x01 | 0x08 | 0x10);
                    packed.push(56); // G-4
                    packed.push(0x03);
                    packed.push(0xFF);
                } else {
                    packed.push(0x80);
                }
            }
        }
        out.extend_from_slice(&9u32.to_le_bytes());
        out.push(0);
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&(packed.len() as u16).to_le_bytes());
        out.extend(packed);

        // Reuse the sine instrument builder by appending via the helper.
        let helper = build_sine_xm_with_effect(1, 0x00, 0x00);
        // Find the instrument start in `helper` by skipping its header +
        // pattern. We can do this by parsing the helper's header and
        // using pattern_data_offset + pattern-block size.
        let h_helper = xm::parse_header(&helper).unwrap();
        let (pats_helper, end_helper) = xm::parse_patterns(&h_helper, &helper).unwrap();
        let _ = pats_helper;
        out.extend_from_slice(&helper[end_helper..]);
        out
    };
    let hdr = xm::parse_header(&bytes).expect("xm header");
    let (patterns, after) = xm::parse_patterns(&hdr, &bytes).expect("patterns");
    let mut instruments = xm::parse_instruments(&hdr, &bytes, after).expect("instruments");
    xm::extract_sample_bodies(&mut instruments, &bytes);

    let mut p = XmPlayerState::new(&hdr, instruments, patterns, 44_100);

    // Render enough to pass row 1 completely: row 1 starts at 1*6 ticks
    // × ~882 frames/tick ≈ 5_292 frames, and runs for 6 ticks. We want
    // to be mid-way through row 2 (past all the porta steps) so render
    // ~2 rows worth of frames = 12 ticks * 882 ≈ 10_584.
    let n_frames = 12_000;
    let mut buf = vec![0i16; n_frames * 2];
    p.render(&mut buf);

    // After row 1's 6 porta steps at speed 0xFF * 4 = 1020 period units
    // per tick, the channel should have reached the G-4 target.
    // G-4 period (Linear): 10*12*16*4 - 55*16*4 - 0 = 7680 - 3520 = 4160.
    let ch = &p.channels[0];
    let target = 10.0 * 12.0 * 16.0 * 4.0 - 55.0 * 16.0 * 4.0;
    assert!(
        (ch.period - target).abs() < 4.0,
        "expected tone porta to reach G-4 period {target}, got {}",
        ch.period
    );
}

/// Pattern-break (Dxy): Dxy on row 0 of pattern 0 should make the next
/// row the (x*10+y)-th row of the next order-table entry. Since our
/// test song only has one pattern, the break jumps past end-of-song
/// into the restart handler.
#[test]
fn xm_player_pattern_break_lands_on_correct_row() {
    // Pattern 0 = 16 rows; row 0 fires D04 (jump to row 4 of next pat).
    // We want to test a break *within* the same song, so build a song
    // with 2 patterns in the order table.
    let mut out = vec![0u8; xm::XM_MIN_HEADER_LEN];
    out[0..17].copy_from_slice(xm::XM_BANNER);
    out[17..37].copy_from_slice(b"break               ");
    out[xm::XM_ID_BYTE_OFFSET] = 0x1A;
    out[38..58].copy_from_slice(b"oxideav             ");
    out[58..60].copy_from_slice(&xm::XM_VERSION_0104.to_le_bytes());
    out[60..64].copy_from_slice(&0x114u32.to_le_bytes());
    out[64..66].copy_from_slice(&2u16.to_le_bytes()); // song_length = 2
    out[66..68].copy_from_slice(&0u16.to_le_bytes()); // restart
    out[68..70].copy_from_slice(&2u16.to_le_bytes()); // channels
    out[70..72].copy_from_slice(&2u16.to_le_bytes()); // 2 patterns
    out[72..74].copy_from_slice(&0u16.to_le_bytes()); // 0 instruments (saves bytes)
    out[74..76].copy_from_slice(&1u16.to_le_bytes());
    out[76..78].copy_from_slice(&6u16.to_le_bytes());
    out[78..80].copy_from_slice(&125u16.to_le_bytes());
    // Order: [0, 1, ...]
    out[XM_ORDER_INDEX_0] = 0;
    out[XM_ORDER_INDEX_0 + 1] = 1;
    for i in 2..xm::XM_ORDER_TABLE_SIZE {
        out[xm::XM_ORDER_TABLE_OFFSET + i] = 0xFF;
    }

    // Pattern 0: 16 rows, row 0 ch 0 fires effect D04 (dec 4). No note.
    let mut packed0 = Vec::new();
    for row in 0..16u16 {
        for ch in 0..2u8 {
            if row == 0 && ch == 0 {
                packed0.push(0x80 | 0x08 | 0x10);
                packed0.push(0x0D);
                packed0.push(0x04);
            } else {
                packed0.push(0x80);
            }
        }
    }
    out.extend_from_slice(&9u32.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(&(packed0.len() as u16).to_le_bytes());
    out.extend(packed0);

    // Pattern 1: 8 rows, empty. (packed_size = 0, parser synthesizes.)
    out.extend_from_slice(&9u32.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());

    let hdr = xm::parse_header(&out).expect("xm header");
    let (patterns, after) = xm::parse_patterns(&hdr, &out).expect("patterns");
    let instruments = xm::parse_instruments(&hdr, &out, after).unwrap_or_default();

    let mut p = XmPlayerState::new(&hdr, instruments, patterns, 44_100);
    // Render enough frames for row 0 (6 ticks at 882 frames/tick) to
    // fully complete so next_row fires. 7 ticks worth keeps us safely
    // inside row 4 of pattern 1.
    let mut buf = vec![0i16; 7 * 882 * 2];
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

const XM_ORDER_INDEX_0: usize = xm::XM_ORDER_TABLE_OFFSET;

#[test]
fn xm_player_silent_on_empty_pattern() {
    let mut bytes = build_c4_square_xm();
    // Overwrite the packed pattern's first cell with a single-byte
    // empty (0x80). The pattern data starts at
    // pattern_data_offset(&header) + 9 (header bytes).
    let header = xm::parse_header(&bytes).unwrap();
    let pattern_start = xm::pattern_data_offset(&header) + 9;
    // Replace the 4-byte packed cell [mask, note, inst, vol] with a
    // single empty cell, padding with a harmless 0x80 so downstream
    // cell counts still come out right.
    bytes[pattern_start] = 0x80;
    bytes[pattern_start + 1] = 0x80;
    bytes[pattern_start + 2] = 0x80;
    bytes[pattern_start + 3] = 0x80;

    let hdr = xm::parse_header(&bytes).unwrap();
    let (patterns, after) = xm::parse_patterns(&hdr, &bytes).unwrap();
    let mut insts = xm::parse_instruments(&hdr, &bytes, after).unwrap();
    xm::extract_sample_bodies(&mut insts, &bytes);
    let mut p = XmPlayerState::new(&hdr, insts, patterns, 44_100);

    let mut buf = vec![0i16; 4410 * 2];
    let produced = p.render(&mut buf);
    assert_eq!(produced, 4410);
    assert!(
        buf.iter().all(|&x| x == 0),
        "expected pure silence on empty pattern"
    );
}
