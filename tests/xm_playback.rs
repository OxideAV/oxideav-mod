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
    let mut vol_env = [0u8; 48];
    vol_env[0..2].copy_from_slice(&0u16.to_le_bytes());
    vol_env[2..4].copy_from_slice(&0u16.to_le_bytes());
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
    for i in 0..64 {
        abs_pcm[i] = if i < 32 { 100 } else { -100 };
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
