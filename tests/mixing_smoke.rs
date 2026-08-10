//! Integration smoke tests for the MOD mixer.
//!
//! Confirms that the decoder produces audible stereo S16 at
//! `OUTPUT_SAMPLE_RATE` when a note is triggered, and produces pure
//! silence when the pattern contains no note events.

use oxideav_core::{CodecId, CodecParameters, Error, Frame, Packet, TimeBase};
use oxideav_core::{CodecRegistry, Decoder};
use oxideav_mod::{container::OUTPUT_SAMPLE_RATE, register_codecs, CODEC_ID_STR};

const HEADER_FIXED_SIZE: usize = 1084;
const PATTERN_BYTES: usize = 64 * 4 * 4;

/// Build a 4-channel `M.K.` MOD with a single pattern and one 32-byte
/// sine-ish sample. If `trigger` is true, row 0 / channel 0 triggers the
/// sample at a C-2 period; if false, the pattern is entirely empty.
fn build_mod(trigger: bool) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_FIXED_SIZE];
    out[0..4].copy_from_slice(b"test");

    // Sample 1: 32 samples (16 words), volume 64, full-length loop so
    // playback doesn't fade out after a single pass.
    out[20 + 22..20 + 24].copy_from_slice(&16u16.to_be_bytes());
    out[20 + 24] = 0; // finetune
    out[20 + 25] = 64; // volume
    out[20 + 26..20 + 28].copy_from_slice(&0u16.to_be_bytes()); // loop start
    out[20 + 28..20 + 30].copy_from_slice(&16u16.to_be_bytes()); // loop length (words)

    // Song: 1 pattern in the order table.
    out[950] = 1;
    out[951] = 0x7F;
    out[952] = 0;
    out[1080..1084].copy_from_slice(b"M.K.");

    // Pattern 0.
    let mut pat = vec![0u8; PATTERN_BYTES];
    if trigger {
        // Row 0, channel 0: sample 1, period 428 (C-2), no effect.
        let period: u16 = 428;
        let p_hi = ((period >> 8) & 0x0F) as u8;
        let p_lo = (period & 0xFF) as u8;
        pat[0] = p_hi; // high nibble of sample index (0) | high nibble of period
        pat[1] = p_lo;
        pat[2] = 1 << 4; // low nibble of sample index (1) | effect (0)
        pat[3] = 0;
    }
    out.extend(pat);

    // 32-sample body — a crude half-wave (positive then negative).
    for i in 0..32 {
        let v: i8 = if i < 16 { 80 } else { -80 };
        out.push(v as u8);
    }
    out
}

fn decode_full(mod_bytes: Vec<u8>, max_frames: usize) -> Vec<i16> {
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    let codec_id = CodecId::new(CODEC_ID_STR);
    let params = CodecParameters::audio(codec_id);
    let mut dec: Box<dyn Decoder> = reg.first_decoder(&params).expect("decoder available");

    let pkt = Packet::new(0, TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64), mod_bytes);
    dec.send_packet(&pkt).expect("send_packet");

    let mut pcm = Vec::new();
    loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(a)) => {
                for chunk in a.data[0].chunks_exact(2) {
                    pcm.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                }
                if pcm.len() / 2 >= max_frames {
                    break;
                }
            }
            Ok(_) => unreachable!("MOD decoder only emits audio frames"),
            Err(Error::Eof) => break,
            Err(e) => panic!("decode error: {e:?}"),
        }
    }
    pcm
}

#[test]
fn mod_with_note_produces_audible_output() {
    // ~0.2 s of audio is enough for several ticks at 50 Hz default.
    let pcm = decode_full(build_mod(true), 8820);

    assert!(!pcm.is_empty(), "decoder produced zero samples");

    // Basic sanity: no NaN-ish values (i16 can't be NaN, but the mixer
    // goes through f32 internally; check we're not stuck at the clip
    // rails either).
    let peak = pcm.iter().map(|&s| s.unsigned_abs() as u32).max().unwrap();
    let sum_sq: u64 = pcm.iter().map(|&s| (s as i64 * s as i64) as u64).sum();
    let rms = ((sum_sq / pcm.len() as u64) as f64).sqrt() as u32;
    let clipped = pcm.iter().filter(|&&s| s.abs() == i16::MAX).count();
    let clip_ratio = clipped as f32 / pcm.len() as f32;

    // Audible output: peak well above any DC offset floor.
    assert!(peak > 500, "expected audible peak, got {peak}");
    // Meaningful RMS energy.
    assert!(rms > 100, "expected non-trivial RMS, got {rms}");
    // Not sustained clipping (allow a tiny fraction near the rails).
    assert!(
        clip_ratio < 0.05,
        "output is clipping {:.2}% of the time (peak={peak}, rms={rms})",
        clip_ratio * 100.0,
    );

    // The file also has some left *and* right energy because the sample
    // is on channel 0 (hard-left in Amiga panning). We check that at
    // least the left channel has energy. Right may legitimately be 0.
    let mut left_peak = 0i16;
    let mut right_peak = 0i16;
    for pair in pcm.chunks_exact(2) {
        left_peak = left_peak.max(pair[0].abs());
        right_peak = right_peak.max(pair[1].abs());
    }
    assert!(
        left_peak > 500,
        "channel-0 hard-left energy missing (left_peak={left_peak}, right_peak={right_peak})"
    );

    eprintln!(
        "mixing_smoke: peak={peak} rms={rms} left_peak={left_peak} right_peak={right_peak} clip_ratio={:.4}",
        clip_ratio
    );
}

#[test]
fn mod_decode_pipeline_emits_exact_pcm_checkpoints() {
    // End-to-end bit-exact lock on the PUBLIC decode path
    // (container packet → registry decoder → S16 frames), complementing
    // the in-`player` render checkpoints. `build_mod(true)` triggers a
    // full-volume C-2 (period 428) on channel 0 (hard-left under the
    // Amiga LRRL default) with a ±80/128 square-wave sample.
    let pcm = decode_full(build_mod(true), 3500);
    assert!(pcm.len() >= 3500 * 2, "need at least 3500 stereo frames");

    let frame = |f: usize| (pcm[f * 2], pcm[f * 2 + 1]);
    // The per-trigger anti-click ramp opens the channel from silence.
    assert_eq!(frame(0), (0, 0), "decode opens from silence");
    // ±80/128 of full scale, scaled by the hard-left LRRL pan with the
    // default 0.5-separation quarter-bleed: left = round, right = left/3.
    assert_eq!(frame(50), (5119, 1706), "positive square plateau");
    assert_eq!(frame(100), (-5119, -1706), "negative plateau");
    assert_eq!(frame(3000), (-5119, -1706), "loop preserves the waveform");
    // The hard-left LRRL ratio: left magnitude is ~3x the right (the two
    // channels round independently, so allow a 1-LSB slack).
    assert!((pcm[50 * 2] - pcm[50 * 2 + 1] * 3).abs() <= 1);
}

#[test]
fn mod_with_no_notes_is_silent() {
    let pcm = decode_full(build_mod(false), 4410);
    assert!(!pcm.is_empty(), "decoder produced zero samples");
    let peak = pcm.iter().map(|&s| s.unsigned_abs() as u32).max().unwrap();
    assert_eq!(
        peak, 0,
        "expected pure silence with no note events, got peak {peak}"
    );
}

/// Build a minimal SoundTracker 2.6 module
/// (`docs/audio/trackers/mod/Soundtracker-v2.6-IceTracker-st26.txt`):
/// one pattern-list position naming stored track 0 on every channel,
/// track 0 triggering sample 1 at C-2 on row 0, magic `MTN\0` at +1464.
fn build_st26_mod() -> Vec<u8> {
    const ST26_TRACK_DATA_OFFSET: usize = 1468;
    const ST26_MAGIC_OFFSET: usize = 1464;
    let mut out = vec![0u8; ST26_TRACK_DATA_OFFSET];
    out[0..4].copy_from_slice(b"st26");
    out[20 + 22..20 + 24].copy_from_slice(&16u16.to_be_bytes());
    out[20 + 25] = 64; // volume
    out[20 + 28..20 + 30].copy_from_slice(&16u16.to_be_bytes()); // loop len
    out[950] = 1; // pattern-list size
    out[951] = 1; // stored tracks
    out[952..956].copy_from_slice(&[0, 0, 0, 0]);
    out[ST26_MAGIC_OFFSET..ST26_MAGIC_OFFSET + 4].copy_from_slice(b"MTN\0");
    // Track 0: row 0 = sample 1, period 428 (C-2).
    let mut track = vec![0u8; 64 * 4];
    track[0] = 0x01; // period high nibble
    track[1] = 0xAC; // period low byte (0x1AC = 428)
    track[2] = 1 << 4; // sample 1, effect 0
    out.extend(track);
    for i in 0..32 {
        let v: i8 = if i < 16 { 80 } else { -80 };
        out.push(v as u8);
    }
    out
}

#[test]
fn st26_module_decodes_through_public_registry_path() {
    // End-to-end over the PUBLIC path (packet → registry decoder → S16
    // frames) for the SoundTracker 2.6 per-track layout: the +1464
    // magic must dispatch inside the same "mod" codec and the
    // synthesized pattern must produce audible hard-left output.
    let pcm = decode_full(build_st26_mod(), 8820);
    assert!(!pcm.is_empty(), "decoder produced zero samples");
    let peak = pcm.iter().map(|&s| s.unsigned_abs() as u32).max().unwrap();
    assert!(peak > 500, "expected audible ST2.6 output, got peak {peak}");
    let mut left_peak = 0i16;
    for pair in pcm.chunks_exact(2) {
        left_peak = left_peak.max(pair[0].abs());
    }
    assert!(
        left_peak > 500,
        "ST2.6 channel 0 must land hard-left (left_peak={left_peak})"
    );
}
