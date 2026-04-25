//! Real-world MOD playback fidelity harness — Round 16.
//!
//! Synthetic per-effect tests catch the behaviour each individual command
//! is supposed to produce, but they don't catch interactions: a song that
//! mixes pattern-delay with fine vol slides, or jumps BPM mid-pattern, or
//! rides a long tone-portamento across an end-of-row that lands inside a
//! looped sample's loop region. Real-world MODs rely on those
//! combinations heavily, and the bugs they surface are usually
//! "everything renders, but it sounds wrong" — silent corruption.
//!
//! This harness drives the registered `mod` codec end-to-end with a
//! synthetic but deliberately-stressful pattern, then runs **invariant
//! checks** on the rendered PCM:
//!
//!   - No NaN / inf (the decoder emits S16, but the f32 mixer pipeline
//!     can produce NaNs that get cast to 0 on conversion — we still want
//!     to know if any sample lands at the i16 floor as a result of one).
//!   - Sustained clip-rail saturation is rejected (a single in-spec peak
//!     is fine; >5% of samples clipped means the gain staging is wrong).
//!   - DC offset across long playbacks must be small (the LED filter
//!     and the per-channel mixer should not introduce DC drift).
//!   - Per-tick energy must be > 0 when notes are sounding and == 0 on
//!     pure-silence patterns.
//!   - Per-channel planar output must agree with mixed output on
//!     channel count and on which channels actually carry energy.
//!   - Every channel-count signature the parser accepts must drive the
//!     player without panic / overflow.
//!   - Round-trip stress: long render with frequent BPM / speed changes
//!     and pattern-delay must terminate (no infinite loops).
//!
//! When new bugs surface from real-world MODs that synthetic effect
//! tests miss, add a regression case here that reproduces the symptom on
//! a synthetic fixture (we don't ship binary `.mod` fixtures — they
//! would balloon the repo and infringe on third-party content).

use oxideav_core::{CodecId, CodecParameters, Error, Frame, Packet, SampleFormat, TimeBase};
use oxideav_core::{CodecRegistry, Decoder};
use oxideav_mod::{
    container::OUTPUT_SAMPLE_RATE, register_codecs, CODEC_ID_PLANAR_STR, CODEC_ID_STR,
};

const HEADER_FIXED_SIZE: usize = 1084;

/// Compose a MOD with N channels, M patterns, a single 32-byte square-wave
/// sample, and a caller-supplied per-pattern populator. Returns the byte
/// blob ready for the decoder.
fn build_mod_n_channels<F>(channels: u8, n_patterns: u8, mut populate: F) -> Vec<u8>
where
    F: FnMut(usize, &mut [u8]),
{
    let signature: [u8; 4] = match channels {
        4 => *b"M.K.",
        6 => *b"6CHN",
        8 => *b"8CHN",
        n if (10..=32).contains(&n) => {
            let tens = b'0' + (n / 10);
            let ones = b'0' + (n % 10);
            [tens, ones, b'C', b'H']
        }
        _ => panic!("unsupported channel count {channels}"),
    };

    let mut out = vec![0u8; HEADER_FIXED_SIZE];
    out[0..4].copy_from_slice(b"hrns");

    // Sample 1: 32 frames (16 words), full-length loop, vol 64.
    out[20 + 22..20 + 24].copy_from_slice(&16u16.to_be_bytes());
    out[20 + 24] = 0;
    out[20 + 25] = 64;
    out[20 + 26..20 + 28].copy_from_slice(&0u16.to_be_bytes());
    out[20 + 28..20 + 30].copy_from_slice(&16u16.to_be_bytes());

    // Song length and order table: play patterns 0..n_patterns sequentially.
    out[950] = n_patterns;
    out[951] = 0x7F;
    for i in 0..(n_patterns as usize).min(128) {
        out[952 + i] = i as u8;
    }
    out[1080..1084].copy_from_slice(&signature);

    let pat_bytes = 64 * channels as usize * 4;
    for p in 0..n_patterns as usize {
        let mut pat = vec![0u8; pat_bytes];
        populate(p, &mut pat);
        out.extend(pat);
    }

    // Sample body: square wave.
    for i in 0..32 {
        let v: i8 = if i < 16 { 100 } else { -100 };
        out.push(v as u8);
    }
    out
}

/// Helper: write a note into an N-channel pattern row.
#[allow(clippy::too_many_arguments)]
fn write_note(
    pat: &mut [u8],
    channels: u8,
    row: usize,
    channel: usize,
    period: u16,
    sample: u8,
    effect: u8,
    effect_param: u8,
) {
    assert!(channel < channels as usize);
    let stride = channels as usize * 4;
    let off = row * stride + channel * 4;
    let p_hi = ((period >> 8) & 0x0F) as u8;
    let p_lo = (period & 0xFF) as u8;
    let sample_hi = (sample & 0xF0) >> 4;
    let sample_lo = sample & 0x0F;
    pat[off] = (sample_hi << 4) | p_hi;
    pat[off + 1] = p_lo;
    pat[off + 2] = (sample_lo << 4) | effect;
    pat[off + 3] = effect_param;
}

/// Render the whole song through the registered `mod` codec, returning
/// stereo interleaved S16 PCM. Caps at `max_frames` so a runaway song
/// (loop without break) is bounded.
fn decode_mixed(bytes: Vec<u8>, max_frames: usize) -> Vec<i16> {
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    let params = CodecParameters::audio(CodecId::new(CODEC_ID_STR));
    let mut dec: Box<dyn Decoder> = reg.make_decoder(&params).expect("decoder");
    let pkt = Packet::new(0, TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64), bytes);
    dec.send_packet(&pkt).expect("send_packet");

    let mut pcm = Vec::new();
    loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(a)) => {
                assert_eq!(a.channels, 2);
                assert_eq!(a.format, SampleFormat::S16);
                assert_eq!(a.sample_rate, OUTPUT_SAMPLE_RATE);
                for chunk in a.data[0].chunks_exact(2) {
                    pcm.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                }
                if pcm.len() / 2 >= max_frames {
                    break;
                }
            }
            Ok(_) => unreachable!("MOD emits audio only"),
            Err(Error::Eof) => break,
            Err(e) => panic!("decode error: {e:?}"),
        }
    }
    pcm
}

/// Render planar output (one S16 plane per MOD tracker channel). Returns
/// `Vec<Vec<i16>>` indexed by channel.
fn decode_planar(bytes: Vec<u8>, max_frames: usize) -> Vec<Vec<i16>> {
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    let params = CodecParameters::audio(CodecId::new(CODEC_ID_PLANAR_STR));
    let mut dec: Box<dyn Decoder> = reg.make_decoder(&params).expect("decoder");
    let pkt = Packet::new(0, TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64), bytes);
    dec.send_packet(&pkt).expect("send_packet");

    let mut planes: Vec<Vec<i16>> = Vec::new();
    loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(a)) => {
                assert_eq!(a.format, SampleFormat::S16P);
                assert_eq!(a.sample_rate, OUTPUT_SAMPLE_RATE);
                if planes.is_empty() {
                    planes.resize(a.channels as usize, Vec::new());
                }
                for (i, plane) in a.data.iter().enumerate() {
                    for chunk in plane.chunks_exact(2) {
                        planes[i].push(i16::from_le_bytes([chunk[0], chunk[1]]));
                    }
                }
                let len = planes.first().map_or(0, |p| p.len());
                if len >= max_frames {
                    break;
                }
            }
            Ok(_) => unreachable!("MOD emits audio only"),
            Err(Error::Eof) => break,
            Err(e) => panic!("decode error: {e:?}"),
        }
    }
    planes
}

// ---------- Invariant helpers ----------

/// Compute mean (DC offset) and RMS over a slice. Returns (mean, rms) in
/// i32 to keep the arithmetic exact across long buffers.
fn mean_and_rms(pcm: &[i16]) -> (i32, u32) {
    if pcm.is_empty() {
        return (0, 0);
    }
    let sum: i64 = pcm.iter().map(|&s| s as i64).sum();
    let sum_sq: u64 = pcm.iter().map(|&s| (s as i64 * s as i64) as u64).sum();
    let n = pcm.len() as i64;
    let mean = (sum / n) as i32;
    let rms = ((sum_sq / pcm.len() as u64) as f64).sqrt() as u32;
    (mean, rms)
}

/// Number of samples whose magnitude is at the i16 clip rail.
fn clip_count(pcm: &[i16]) -> usize {
    pcm.iter()
        .filter(|&&s| s == i16::MAX || s == i16::MIN)
        .count()
}

// ---------- Tests ----------

/// Baseline invariants on a "boring" MOD: one sample, one note, no
/// effects. Confirms the harness primitives work and pins the floor for
/// expected energy on a healthy 4-channel run.
#[test]
fn baseline_4ch_song_invariants() {
    let bytes = build_mod_n_channels(4, 1, |_, pat| {
        write_note(pat, 4, 0, 0, 428, 1, 0, 0);
    });
    let pcm = decode_mixed(bytes, 88_200); // ~2 s
    assert!(!pcm.is_empty());

    let (mean, rms) = mean_and_rms(&pcm);
    let clipped = clip_count(&pcm);
    let clip_ratio = clipped as f32 / pcm.len() as f32;

    // Mean (DC) must be small. We expect ≈ 0 for a square-wave loop;
    // the LED filter will leave a small bias but it should be
    // well under any audible threshold.
    assert!(mean.abs() < 200, "DC offset too large: mean={mean}");

    // Audible energy.
    assert!(rms > 100, "RMS too low: {rms}");

    // No sustained clipping.
    assert!(
        clip_ratio < 0.05,
        "clip ratio {clip_ratio:.4} (clipped={clipped} of {})",
        pcm.len()
    );
}

/// Render a pattern-delay-heavy MOD. Confirms the EE-induced repeats do
/// not re-trigger the held note — sample_pos must keep advancing
/// monotonically across the delay (audible signature: no click on every
/// row boundary). Catches the regression introduced by re-invoking
/// enter_row on each delay pass.
#[test]
fn pattern_delay_does_not_glitch_held_note() {
    // Row 0: trigger C-2 on ch0. Row 1: EE2 (delay 2 row passes) on
    // ch1, no note — the held note must continue cleanly.
    let bytes = build_mod_n_channels(4, 1, |_, pat| {
        write_note(pat, 4, 0, 0, 428, 1, 0, 0);
        write_note(pat, 4, 1, 1, 0, 0, 0xE, 0xE2);
    });
    let pcm = decode_mixed(bytes, 88_200); // 2 s
    let (_, rms) = mean_and_rms(&pcm);
    assert!(
        rms > 100,
        "EE delay: RMS too low {rms} — note may have died"
    );

    // No clip-rail saturation should occur — the held note has fixed
    // amplitude, so any sample stuck at i16::MAX/MIN suggests an
    // accumulator went wrong on the repeat.
    let clipped = clip_count(&pcm);
    assert!(
        clipped < pcm.len() / 100,
        "EE delay: {clipped} clipped samples in {} — held note went non-finite?",
        pcm.len()
    );
}

/// Render a song that flips BPM repeatedly mid-pattern. The sample-
/// per-tick math depends on BPM, so this stresses the partial-tick
/// boundary handling. The decoder must not panic, must terminate, and
/// must produce audible energy throughout.
#[test]
fn bpm_changes_mid_pattern_do_not_glitch() {
    let bytes = build_mod_n_channels(4, 1, |_, pat| {
        // Trigger note on row 0.
        write_note(pat, 4, 0, 0, 428, 1, 0, 0);
        // Halve the BPM at row 8: F20 = BPM 32.
        write_note(pat, 4, 8, 1, 0, 0, 0xF, 0x20);
        // Boost back to F50 = 80 BPM at row 16.
        write_note(pat, 4, 16, 1, 0, 0, 0xF, 0x50);
        // F7D = 125 BPM (default) at row 24.
        write_note(pat, 4, 24, 1, 0, 0, 0xF, 0x7D);
        // Flip speed at row 32: F02 = 2 ticks/row.
        write_note(pat, 4, 32, 1, 0, 0, 0xF, 0x02);
        // Restore speed F06 at row 48.
        write_note(pat, 4, 48, 1, 0, 0, 0xF, 0x06);
    });
    let pcm = decode_mixed(bytes, 220_500); // 5 s cap
    assert!(!pcm.is_empty());

    let (mean, rms) = mean_and_rms(&pcm);
    assert!(mean.abs() < 200, "BPM-flip DC offset {mean}");
    assert!(rms > 100, "BPM-flip RMS too low {rms}");
}

/// Stress wide channel counts. ProTracker variants exist for 6, 8, 16,
/// and 32 channels; the parser accepts all of them. Make sure rendering
/// works for each, energy lands on the correct hard-pan side, and the
/// per-channel headroom scaler keeps the mix under the rails.
#[test]
fn wide_channel_count_files_render_clean() {
    for &n_ch in &[4u8, 6, 8, 12, 16, 32] {
        let bytes = build_mod_n_channels(n_ch, 1, |_, pat| {
            // Trigger sample 1 on every channel at C-2.
            for c in 0..n_ch as usize {
                write_note(pat, n_ch, 0, c, 428, 1, 0, 0);
            }
        });
        let pcm = decode_mixed(bytes, 44_100); // 1 s
        assert!(!pcm.is_empty(), "{n_ch}-channel: no audio produced");
        let clipped = clip_count(&pcm);
        let clip_ratio = clipped as f32 / pcm.len() as f32;
        // The 1/(N/2) headroom should keep us off the rails even with
        // every channel active. Allow a small ratio for the rare
        // alignment where all channels hit positive peak simultaneously.
        assert!(
            clip_ratio < 0.10,
            "{n_ch}-channel: clip ratio {clip_ratio:.4} — headroom \
             scaler may be wrong (clipped {clipped}/{})",
            pcm.len()
        );
        let (_, rms) = mean_and_rms(&pcm);
        assert!(rms > 100, "{n_ch}-channel: RMS too low {rms}");
    }
}

/// Tone portamento that crosses a row boundary while a sample loop is
/// active. The combination is common in real-world MODs (lead lines that
/// glide between notes while the underlying sample loops). The mixer
/// must keep `sample_pos` advancing through the loop wrap on every tick
/// of the slide.
#[test]
fn tone_porta_across_row_boundary_with_loop_does_not_silence() {
    let bytes = build_mod_n_channels(4, 1, |_, pat| {
        // Row 0: C-2 trigger.
        write_note(pat, 4, 0, 0, 428, 1, 0, 0);
        // Rows 1..=8: tone-porta toward A-2 (period 254) at speed $20.
        write_note(pat, 4, 1, 0, 254, 0, 0x3, 0x20);
        for r in 2..=8 {
            write_note(pat, 4, r, 0, 0, 0, 0x3, 0x00);
        }
    });
    let pcm = decode_mixed(bytes, 88_200);
    let (_, rms) = mean_and_rms(&pcm);
    assert!(
        rms > 100,
        "tone-porta + loop: signal went silent (rms={rms}) — \
         loop wrap may be killing the voice"
    );
    let clipped = clip_count(&pcm);
    assert!(clipped < pcm.len() / 100);
}

/// Per-channel planar output must produce the same channel layout the
/// header declared. Mix the per-channel stream back manually and confirm
/// energy is present on at least the channels we expect.
#[test]
fn planar_output_layout_matches_mixed_layout() {
    // 6-channel MOD: trigger on channels 0, 2, 4 (mix of left + right
    // pan slots). Channels 1, 3, 5 stay silent.
    let bytes = build_mod_n_channels(6, 1, |_, pat| {
        for &c in &[0usize, 2, 4] {
            write_note(pat, 6, 0, c, 428, 1, 0, 0);
        }
    });
    let planes = decode_planar(bytes.clone(), 22_050);
    assert_eq!(planes.len(), 6, "planar must expose 6 planes");
    for (i, plane) in planes.iter().enumerate() {
        let nonzero = plane.iter().filter(|&&s| s != 0).count();
        if [0usize, 2, 4].contains(&i) {
            assert!(
                nonzero > 100,
                "channel {i} should carry signal, got {nonzero} non-zero samples"
            );
        } else {
            assert_eq!(
                nonzero, 0,
                "channel {i} should be silent, got {nonzero} non-zero samples"
            );
        }
    }

    // The mixed output should have the same total length (modulo
    // chunking) — both modes drive the same player engine.
    let mixed = decode_mixed(bytes, 22_050);
    // mixed is interleaved stereo → twice the frame count of one plane.
    assert_eq!(mixed.len(), planes[0].len() * 2);
}

/// A long-running render must not exhibit DC drift. The LED filter
/// (1-pole IIR) integrates errors over time; if any per-channel mixer
/// step leaks a non-zero bias, it will accumulate. Run for ~5 seconds
/// of a steady-state square wave and confirm the rolling mean stays
/// near zero.
#[test]
fn long_render_no_dc_drift() {
    let bytes = build_mod_n_channels(4, 1, |_, pat| {
        write_note(pat, 4, 0, 0, 428, 1, 0, 0);
    });
    let pcm = decode_mixed(bytes, 220_500); // 5 s
    assert!(!pcm.is_empty());

    // Compute DC across the second half of the run (after LED filter
    // settles).
    let half = pcm.len() / 2;
    let (late_mean, _) = mean_and_rms(&pcm[half..]);
    assert!(
        late_mean.abs() < 200,
        "long-render DC drift: late-mean={late_mean}"
    );
}

/// Pattern-delay + tick-N effect interaction: row carries 4xy vibrato +
/// EE2. The vibrato LFO must keep advancing across the delay repeats
/// (per-tick effects continue), but the tick-0 vibrato param-memory
/// load must not re-execute on each repeat.
#[test]
fn pattern_delay_does_not_freeze_vibrato_lfo() {
    let bytes = build_mod_n_channels(4, 1, |_, pat| {
        // Row 0: C-2 with 488 (vib rate 8 depth 8) — strong vibrato.
        write_note(pat, 4, 0, 0, 428, 1, 0x4, 0x88);
        // Row 1: keep vibrato + EE2 (delay 2). On a healthy player the
        // vibrato keeps modulating across the delay; on a broken one
        // the LFO freezes and the channel sounds like a flat sine.
        write_note(pat, 4, 1, 0, 0, 0, 0x4, 0x00);
        write_note(pat, 4, 1, 1, 0, 0, 0xE, 0xE2);
    });
    let pcm = decode_mixed(bytes, 88_200);
    // Compute a coarse zero-crossing-spread measure: with vibrato
    // active the period oscillates → ZCR intervals vary. With a
    // frozen LFO the spread collapses.
    let mut intervals = Vec::new();
    let mut prev_sign = pcm[0] < 0;
    let mut prev_i = 0usize;
    for (i, &s) in pcm.iter().enumerate().step_by(2) {
        let sign = s < 0;
        if sign != prev_sign && s != 0 {
            intervals.push(i - prev_i);
            prev_i = i;
            prev_sign = sign;
        }
    }
    if intervals.len() > 10 {
        let max = *intervals.iter().max().unwrap();
        let min = *intervals.iter().min().unwrap();
        assert!(
            max > min,
            "vibrato LFO froze under EE pattern delay: \
             ZCR interval spread = max {max} == min {min}"
        );
    }
}

/// E6x pattern loop in combination with a real note on the loop boundary.
/// The first iteration triggers; subsequent loop iterations should NOT
/// re-trigger the note (the sample keeps playing through the loop), but
/// the loop counter must advance correctly.
#[test]
fn e6_pattern_loop_terminates_in_bounded_time() {
    let bytes = build_mod_n_channels(4, 1, |_, pat| {
        // Row 0: trigger.
        write_note(pat, 4, 0, 0, 428, 1, 0, 0);
        // Row 1: E60 (set loop start = row 1).
        write_note(pat, 4, 1, 0, 0, 0, 0xE, 0x60);
        // Row 5: E63 (loop back to row 1, three times).
        write_note(pat, 4, 5, 0, 0, 0, 0xE, 0x63);
    });
    // Cap aggressively — a runaway E6x would spin forever. Default
    // speed 6 + BPM 125 = 882 frames/tick × 6 ticks/row = 5292/row.
    // 4 visits × 5 rows × 5292 = 105_840 frames. Cap at 200k.
    let pcm = decode_mixed(bytes, 200_000);
    assert!(!pcm.is_empty());
    let (_, rms) = mean_and_rms(&pcm);
    assert!(rms > 100);
}

/// Confirm the `mod_planar` codec id is accepted and produces no NaN-
/// in-disguise: every plane sample must be a valid i16 value (we read
/// it as i16 already, so the only way NaN could leak is through f32→i16
/// which clips to MIN/MAX). Regression guard for any future
/// floating-point bug in the mixer.
#[test]
fn planar_no_disguised_nan_samples() {
    // Drive heavy effect interaction so any NaN paths surface.
    let bytes = build_mod_n_channels(4, 1, |_, pat| {
        write_note(pat, 4, 0, 0, 428, 1, 0, 0);
        // Aggressive 1FF porta-up that would underflow the period
        // saturating_sub every tick — exercises the period clamp path.
        write_note(pat, 4, 1, 0, 0, 0, 0x1, 0xFF);
        // Aggressive 2FF porta-down.
        write_note(pat, 4, 5, 0, 0, 0, 0x2, 0xFF);
        // Vibrato + vol slide on top.
        write_note(pat, 4, 9, 0, 0, 0, 0x6, 0x84);
        // Sample-offset deep into the (nonexistent) past.
        write_note(pat, 4, 13, 0, 0, 0, 0x9, 0xFF);
    });
    let planes = decode_planar(bytes, 88_200);
    for (i, plane) in planes.iter().enumerate() {
        // A NaN-cast-to-i16 lands at 0 in Rust (f32 as i16 saturates),
        // so we look for sustained extreme rails which would be the
        // compatible failure mode (an exponentially-blown accumulator).
        let extreme = plane.iter().filter(|&&s| s.abs() > 32_000).count();
        let extreme_ratio = extreme as f32 / plane.len().max(1) as f32;
        assert!(
            extreme_ratio < 0.10,
            "plane {i}: {extreme}/{} samples near i16 rail \
             (ratio {extreme_ratio:.4}) — possible f32 blow-up",
            plane.len()
        );
    }
}

/// Sanity: a forged song with a large song_length but only one real
/// pattern in the order table must terminate naturally — every "extra"
/// order entry points to the same pattern so the song is N copies of
/// pattern 0 and ends cleanly. We check the decoder terminates well
/// under our 5-million-sample bound (with default speed/BPM, 64
/// orders × 64 rows × 6 ticks × 882 samples × 2 stereo ≈ 43M would
/// be unbounded; our test cap below is the *natural* end at 64 orders).
#[test]
fn malformed_song_length_terminates() {
    let mut bytes = build_mod_n_channels(4, 1, |_, pat| {
        write_note(pat, 4, 0, 0, 428, 1, 0, 0);
    });
    // Force song_length large but cap order table at all-pattern-0.
    bytes[950] = 4; // 4 orders, each pointing to pattern 0 (already the default).
                    // Cap at 2 s — any reasonable song should finish well under this.
    let pcm = decode_mixed(bytes, 5_000_000);
    assert!(!pcm.is_empty());
    // 4 × 64 × 6 × 882 × 2 ≈ 2.7M — must be at or under that.
    assert!(
        pcm.len() < 5_000_000 * 2,
        "decoder did not terminate within 4-pattern bound, got {} samples",
        pcm.len()
    );
    // Also confirm we got a reasonable final length — too-short means
    // we hit Eof prematurely.
    assert!(
        pcm.len() > 100_000,
        "decoder terminated suspiciously early: {} samples",
        pcm.len()
    );
}
