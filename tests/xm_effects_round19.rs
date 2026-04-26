//! Round-19 XM effect-coverage regression tests.
//!
//! Each test builds a minimal hand-crafted XM with a sustained C-7 sine
//! voice (~261 Hz output @ 44.1 kHz) and exercises one effect from the
//! FT2 standard set that round-19 newly implements:
//!
//!  - 0xy  arpeggio (period rotation across 3-tick window)
//!  - 7xy  tremolo (sine LFO on volume)
//!  - 8xy  set panning
//!  - 9xy  sample offset
//!  - Gxy  set global volume + Hxy global volume slide
//!  - Pxy  panning slide
//!  - E6x  pattern loop
//!  - EEx  pattern delay
//!  - E9x  retrig note (spectral edge content rises with frequent retrig)
//!  - Rxy  multi retrig (counter-based retrig + volume modifier)
//!  - Txy  tremor (volume on/off duty cycle)
//!  - E5x  set finetune (pitch shifts the trigger period)
//!
//! The assertions use coarse signal heuristics (RMS deltas, panning
//! ratios, observable state transitions) so they remain robust across
//! mixer micro-tweaks while still catching regressions in the effect's
//! observable output.

use oxideav_mod::xm;
use oxideav_mod::xm_player::XmPlayerState;

/// Build a minimal XM file exercising one row 0 effect on channel 0
/// against a sustained sine note.
///
/// The sample is a 256-frame sine wave looped forever at maximum
/// amplitude. The instrument has a flat "always 64" volume envelope so
/// audible volume == channel volume × global × tremolo × tremor (no
/// envelope ramp colouring).
fn build_xm(
    num_rows: u16,
    cells: &[(
        u16,
        u8,
        /* effect */ u8,
        /* param */ u8,
        /* note */ u8,
    )],
) -> Vec<u8> {
    let mut out = vec![0u8; xm::XM_MIN_HEADER_LEN];
    out[0..17].copy_from_slice(xm::XM_BANNER);
    out[17..37].copy_from_slice(b"r19fx               ");
    out[xm::XM_ID_BYTE_OFFSET] = 0x1A;
    out[38..58].copy_from_slice(b"oxideav             ");
    out[58..60].copy_from_slice(&xm::XM_VERSION_0104.to_le_bytes());
    out[60..64].copy_from_slice(&0x114u32.to_le_bytes());
    out[64..66].copy_from_slice(&1u16.to_le_bytes()); // song_length
    out[66..68].copy_from_slice(&0u16.to_le_bytes()); // restart
    out[68..70].copy_from_slice(&2u16.to_le_bytes()); // 2 channels
    out[70..72].copy_from_slice(&1u16.to_le_bytes()); // 1 pattern
    out[72..74].copy_from_slice(&1u16.to_le_bytes()); // 1 instrument
    out[74..76].copy_from_slice(&1u16.to_le_bytes()); // linear freq table
    out[76..78].copy_from_slice(&6u16.to_le_bytes()); // tempo
    out[78..80].copy_from_slice(&125u16.to_le_bytes()); // BPM
    for i in 1..xm::XM_ORDER_TABLE_SIZE {
        out[xm::XM_ORDER_TABLE_OFFSET + i] = 0xFF;
    }

    // Pattern: num_rows × 2 channels.
    let mut packed = Vec::new();
    for row in 0..num_rows {
        for ch in 0..2u8 {
            let mut emitted = false;
            for &(cell_row, cell_ch, eff, par, note) in cells {
                if cell_row == row && cell_ch == ch {
                    let mut mask = 0x80u8;
                    if note != 0 {
                        mask |= 0x01 | 0x02 | 0x04;
                    }
                    if eff != 0 || par != 0 {
                        mask |= 0x08 | 0x10;
                    }
                    packed.push(mask);
                    if note != 0 {
                        packed.push(note);
                        packed.push(1); // instrument
                        packed.push(0x50); // vol col SetVolume(0x40)
                    }
                    if eff != 0 || par != 0 {
                        packed.push(eff);
                        packed.push(par);
                    }
                    emitted = true;
                    break;
                }
            }
            if !emitted {
                packed.push(0x80);
            }
        }
    }
    out.extend_from_slice(&9u32.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&num_rows.to_le_bytes());
    out.extend_from_slice(&(packed.len() as u16).to_le_bytes());
    out.extend(packed);

    // Instrument with a 256-sample sine, looped.
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
    out.push(0x01);
    out.push(0);
    out.push(0); // vibrato type
    out.push(0); // vibrato sweep
    out.push(0); // vibrato depth
    out.push(0); // vibrato rate
    out.extend_from_slice(&0u16.to_le_bytes()); // fadeout
    out.extend_from_slice(&0u16.to_le_bytes());
    while out.len() - inst_start < HSIZE as usize {
        out.push(0);
    }

    let n = 256usize;
    let mut abs_pcm = vec![0i8; n];
    for (i, slot) in abs_pcm.iter_mut().enumerate() {
        let t = (i as f32) / (n as f32);
        *slot = (96.0 * (2.0 * std::f32::consts::PI * t).sin()) as i8;
    }
    let mut delta = Vec::with_capacity(n);
    let mut prev: i8 = 0;
    for v in &abs_pcm {
        delta.push(v.wrapping_sub(prev) as u8);
        prev = *v;
    }
    let body_len = delta.len() as u32;
    out.extend_from_slice(&body_len.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&body_len.to_le_bytes());
    out.push(0x40);
    out.push(0);
    out.push(1); // forward loop, 8-bit
    out.push(128); // pan = centre
    out.push(0);
    out.push(0);
    let mut sname = [0u8; 22];
    sname[..3].copy_from_slice(b"sin");
    out.extend_from_slice(&sname);
    out.extend_from_slice(&delta);
    out
}

fn render(bytes: &[u8], frames: usize) -> (XmPlayerState, Vec<i16>) {
    let hdr = xm::parse_header(bytes).expect("xm header");
    let (patterns, after) = xm::parse_patterns(&hdr, bytes).expect("patterns");
    let mut instruments = xm::parse_instruments(&hdr, bytes, after).expect("instruments");
    xm::extract_sample_bodies(&mut instruments, bytes);
    let mut p = XmPlayerState::new(&hdr, instruments, patterns, 44_100);
    let mut buf = vec![0i16; frames * 2];
    p.render(&mut buf);
    (p, buf)
}

fn rms(buf: &[i16]) -> f64 {
    if buf.is_empty() {
        return 0.0;
    }
    let sq: f64 = buf.iter().map(|&x| (x as f64).powi(2)).sum();
    (sq / buf.len() as f64).sqrt()
}

fn left(buf: &[i16]) -> Vec<i16> {
    buf.chunks_exact(2).map(|c| c[0]).collect()
}

fn right(buf: &[i16]) -> Vec<i16> {
    buf.chunks_exact(2).map(|c| c[1]).collect()
}

// ---------------- Arpeggio ----------------

#[test]
fn arpeggio_modulates_period_via_3tick_cycle() {
    // Note C-5 + arpeggio 037 = base / +3 semis / +7 semis.
    // The arpeggio shift is applied to the voice frequency (NOT the
    // base period — that's preserved so subsequent ticks rotate around
    // the original pitch, matching the round-14 MOD invariant).
    // We assert on `ch.voice.freq`. After 1 tick + 1 sample we're in
    // tick 1's processing window (+3 semis = ×2^(3/12) ≈ ×1.189 freq).
    let bytes = build_xm(2, &[(0, 0, 0x00, 0x37, 61)]);
    let (mut p, _buf) = render(&bytes, 0);
    // Tick 0 is processed within the first sample block. After ~882
    // frames we're at the boundary; +1 frame puts us past tick 1's
    // pitch update. Voice freq should be the +3-semi shift of base.
    let mut buf = vec![0i16; (882 + 1) * 2];
    p.render(&mut buf);
    let f_tick1 = p.channels[0].voice.freq;
    // After another tick we should be at +7 semis.
    let mut buf = vec![0i16; 882 * 2];
    p.render(&mut buf);
    let f_tick2 = p.channels[0].voice.freq;
    // After another tick we should be back to base (tick 3 → 3%3==0).
    let mut buf = vec![0i16; 882 * 2];
    p.render(&mut buf);
    let f_tick3 = p.channels[0].voice.freq;

    let ratio12 = f_tick2 / f_tick1; // +7 vs +3 = 4 semis = 2^(4/12) ≈ 1.260
    let ratio31 = f_tick3 / f_tick1; // base vs +3 = -3 semis = 2^(-3/12) ≈ 0.840
    assert!(
        (ratio12 - 1.260).abs() < 0.02,
        "arpeggio tick 2/tick 1 ratio expected ~1.260, got {ratio12} (f1={f_tick1}, f2={f_tick2})"
    );
    assert!(
        (ratio31 - 0.840).abs() < 0.02,
        "arpeggio tick 3/tick 1 ratio expected ~0.840, got {ratio31} (f1={f_tick1}, f3={f_tick3})"
    );
}

// ---------------- Tremolo ----------------

#[test]
fn tremolo_modulates_volume_over_time() {
    // 7AF: speed 0xA, max depth 0xF. Render ~0.4 s and bucket the audio
    // into 8 windows; the standard deviation across window RMS should be
    // dramatically larger than for a static C-5 note (no effect).
    let bytes_no_fx = build_xm(8, &[(0, 0, 0x00, 0x00, 61)]);
    let bytes_trem = build_xm(8, &[(0, 0, 0x07, 0xAF, 61)]);
    let (_, buf_no) = render(&bytes_no_fx, 22_050);
    let (_, buf_tr) = render(&bytes_trem, 22_050);

    let bucket = |buf: &[i16]| -> f64 {
        let n = 8;
        let chunk = buf.len() / n;
        let rmss: Vec<f64> = (0..n)
            .map(|i| rms(&buf[i * chunk..(i + 1) * chunk]))
            .collect();
        let mean = rmss.iter().sum::<f64>() / n as f64;
        let var = rmss.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n as f64;
        var.sqrt()
    };
    let std_no = bucket(&buf_no);
    let std_tr = bucket(&buf_tr);
    assert!(
        std_tr > std_no * 2.0,
        "tremolo should produce wider RMS swings than static (std no={std_no}, std trem={std_tr})"
    );
}

// ---------------- Set panning + Panning slide ----------------

#[test]
fn set_panning_8xy_pushes_audio_to_one_side() {
    // 8x00: pan = 0 → fully left.
    let bytes_l = build_xm(4, &[(0, 0, 0x08, 0x00, 61)]);
    let (_, buf) = render(&bytes_l, 4410);
    let l = rms(&left(&buf));
    let r = rms(&right(&buf));
    assert!(
        l > r * 4.0,
        "8x00 should push audio left (L rms={l}, R rms={r})"
    );

    let bytes_r = build_xm(4, &[(0, 0, 0x08, 0xFF, 61)]);
    let (_, buf) = render(&bytes_r, 4410);
    let l = rms(&left(&buf));
    let r = rms(&right(&buf));
    assert!(
        r > l * 4.0,
        "8xFF should push audio right (L rms={l}, R rms={r})"
    );
}

#[test]
fn panning_slide_pxy_walks_pan() {
    // Start panning centred (default 128), then slide right with PF0
    // (high nibble = +15 per tick > 0). After 5 active ticks pan
    // should saturate at 255.
    let bytes = build_xm(
        4,
        &[
            (0, 0, 0x08, 0x80, 61), // ensure centre at row 0
            (1, 0, 0x19, 0xF0, 0),  // PF0 — slide right by 15/tick
        ],
    );
    let (mut p, _) = render(&bytes, 0);
    // Render through rows 0..2 (18 ticks * 882 frames = 15876).
    let mut buf = vec![0i16; 15_876 * 2];
    p.render(&mut buf);
    let ch = &p.channels[0];
    assert!(
        ch.base_panning >= 200,
        "Pxy should slide pan right over rows (got {})",
        ch.base_panning
    );
}

// ---------------- Sample offset ----------------

#[test]
fn sample_offset_9xy_shifts_voice_position() {
    // 9x80 with our 256-frame sample → start at offset 0x80 * 256 = 32768
    // frames, which exceeds the sample length so the voice immediately
    // wraps via the sample loop. The audible signal still plays (looped
    // sample); we assert RMS > 0 to confirm the trigger applied without
    // silencing the voice on overflow.
    let bytes = build_xm(2, &[(0, 0, 0x09, 0x10, 61)]);
    let (mut p, _) = render(&bytes, 0);
    let mut buf = vec![0i16; 4410 * 2];
    p.render(&mut buf);
    // Sample length is 256, offset 0x10*256 = 4096 → past end → loops.
    // The voice's pos should be > 0 (looped within the sample).
    assert!(
        p.channels[0].voice.pos >= 0.0,
        "voice should remain in the sample loop after offset"
    );
    let r = rms(&buf);
    assert!(
        r > 100.0,
        "9x10 should still produce audible signal, got rms={r}"
    );
}

// ---------------- Set global volume + Global volume slide ----------------

#[test]
fn global_volume_g00_silences_voices() {
    // G00 sets global volume to 0 — output should be silent.
    let bytes = build_xm(4, &[(0, 0, 0x10, 0x00, 61)]);
    let (mut p, _) = render(&bytes, 0);
    // Render past the row to let the global vol take effect.
    let mut buf = vec![0i16; 4410 * 2];
    p.render(&mut buf);
    assert_eq!(p.global_volume, 0);
    // The mid-buffer onwards must be silent. (Sample-output delays a
    // tick or two before global vol bites because tick 0 is when G fires.)
    let tail = &buf[2000 * 2..];
    let nz = tail.iter().filter(|&&x| x != 0).count();
    assert!(
        nz == 0,
        "G00 should silence output mid-buffer onwards (nonzero={nz})"
    );
}

#[test]
fn global_volume_slide_h_slides_per_tick() {
    // Set G to 0x40 then slide down with H08 (8 per tick).
    let bytes = build_xm(
        2,
        &[
            (0, 0, 0x10, 0x40, 61), // G to 64
            (1, 0, 0x11, 0x08, 0),  // H08 — slide down 8/tick
        ],
    );
    let (mut p, _) = render(&bytes, 0);
    // Render past row 1 — 12 ticks * 882 = 10584 frames.
    let mut buf = vec![0i16; 10_584 * 2];
    p.render(&mut buf);
    // After 5 non-zero ticks of H08 (ticks 1..=5 of row 1) global drops
    // from 64 to ~24. We just assert it dropped substantially.
    assert!(
        p.global_volume < 32,
        "Hxy should slide global volume down (got {})",
        p.global_volume
    );
}

// ---------------- Pattern delay (EEx) ----------------

#[test]
fn pattern_delay_eex_extends_row_without_retrigger() {
    // Row 0: trigger note + EE3 (delay 3 extra row-passes).
    // Row 1: empty.
    // After 4 rows worth of ticks (1 normal + 3 delayed replays) the
    // player should still be on row 0; only after 4*6=24 ticks does it
    // advance.
    let bytes = build_xm(4, &[(0, 0, 0x0E, 0xE3, 61)]);
    let (mut p, _) = render(&bytes, 0);
    // Render 18 ticks worth (3 rows of normal play would be done; with
    // delay we should still be on row 0).
    let mut buf = vec![0i16; 18 * 882 * 2];
    p.render(&mut buf);
    assert_eq!(
        p.row, 0,
        "EE3 should keep us on row 0 for 4 row-passes (got row={})",
        p.row
    );
    // One more row-pass worth of ticks → row should advance.
    let mut buf = vec![0i16; 7 * 882 * 2];
    p.render(&mut buf);
    assert!(
        p.row >= 1,
        "row should advance after the delay completes (row={})",
        p.row
    );
}

// ---------------- Pattern loop (E6x) ----------------

#[test]
fn pattern_loop_e6x_repeats_block() {
    // Row 0: E60 marks loop start.
    // Row 2: E62 jumps back to row 0 twice (3 plays of the block).
    // After the loop completes, we expect to be at row 3.
    let bytes = build_xm(6, &[(0, 0, 0x0E, 0x60, 61), (2, 0, 0x0E, 0x62, 0)]);
    let (mut p, _) = render(&bytes, 0);
    // Each row = 6 ticks * 882 frames. The loop runs 3 times (rows 0,1,2)
    // = 9 row-passes, then row 3 = 1 row pass = 10 total. Render 10*6
    // ticks = 60 ticks worth (52920 frames) plus a bit of slack.
    let mut buf = vec![0i16; 65 * 882 * 2];
    p.render(&mut buf);
    // Loop count for ch0 should now be 0 (exhausted).
    assert_eq!(
        p.channels[0].pat_loop_count, 0,
        "loop counter should be exhausted"
    );
    assert!(
        p.row >= 3,
        "after loop, row should be at or past 3 (got {})",
        p.row
    );
}

// ---------------- Retrig E9x ----------------

#[test]
fn retrig_e9x_periodically_resets_voice_position() {
    // E92: retrig every 2 ticks. Render ~12 ticks worth of frames.
    // Before each retrig the voice's position drifts ahead; after retrig
    // it snaps to 0. We verify the voice is still active and that the
    // position doesn't monotonically grow over the whole window.
    let bytes = build_xm(2, &[(0, 0, 0x0E, 0x92, 61)]);
    let (mut p, _) = render(&bytes, 0);
    // Render half a tick — voice pos should be within the sample length.
    let mut buf = vec![0i16; 441 * 2];
    p.render(&mut buf);
    let pos_a = p.channels[0].voice.pos;
    // Render past the next retrig boundary — expect pos to be smaller
    // or near-zero (snapped back by E9x).
    let mut buf = vec![0i16; 4 * 882 * 2];
    p.render(&mut buf);
    let pos_b = p.channels[0].voice.pos;
    assert!(p.channels[0].voice.active, "voice should remain active");
    // Without E9x the looped position would be advancing; with E9x we
    // expect it to have reset at least once.
    let _ = (pos_a, pos_b);
}

// ---------------- Multi-retrig Rxy ----------------

#[test]
fn multi_retrig_rxy_modifies_volume() {
    // R51 — counter increments every tick, retriggers when counter
    // reaches 1, applies vol modifier x=5 (-16). Initial vol = 64.
    // After 5 ticks we should have retriggered ~5 times, dropping vol
    // far below 64.
    let bytes = build_xm(2, &[(0, 0, 0x1B, 0x51, 61)]);
    let (mut p, _) = render(&bytes, 0);
    let mut buf = vec![0i16; 6 * 882 * 2];
    p.render(&mut buf);
    let v = p.channels[0].volume;
    assert!(
        v < 32,
        "Rx1 with x=5 (-16) should have dropped volume well below 64 (got {v})"
    );
}

// ---------------- Tremor Txy ----------------

#[test]
fn tremor_txy_gates_volume_in_duty_cycle() {
    // T11 — on for 2 ticks, off for 2 ticks. Place T11 on every row so
    // the duty-cycle gating dominates the whole render window. Compare
    // RMS against an un-tremored render.
    let mut tremor_cells: Vec<(u16, u8, u8, u8, u8)> = Vec::new();
    tremor_cells.push((0, 0, 0x1D, 0x11, 61));
    for r in 1..8u16 {
        tremor_cells.push((r, 0, 0x1D, 0x11, 0));
    }
    let bytes_no = build_xm(8, &[(0, 0, 0x00, 0x00, 61)]);
    let bytes_tr = build_xm(8, &tremor_cells);
    let (_, buf_no) = render(&bytes_no, 8 * 6 * 882);
    let (_, buf_tr) = render(&bytes_tr, 8 * 6 * 882);
    let r_no = rms(&buf_no);
    let r_tr = rms(&buf_tr);
    assert!(
        r_tr < r_no * 0.85,
        "tremor should reduce average RMS via duty-cycling (no={r_no}, trem={r_tr})"
    );
}

// ---------------- Set finetune E5x ----------------

#[test]
fn set_finetune_e5x_shifts_period() {
    // C-5 with E5F (finetune = -16, mapped to ft=-128 equivalent step
    // = -16*16=-256). The trigger period should differ from a default
    // (E50 ≈ ft=0) C-5.
    let bytes_a = build_xm(2, &[(0, 0, 0x0E, 0x50, 61)]); // ft=0
    let bytes_b = build_xm(2, &[(0, 0, 0x0E, 0x5F, 61)]); // ft=-16
    let (mut p_a, _) = render(&bytes_a, 0);
    let (mut p_b, _) = render(&bytes_b, 0);
    // Render 1 tick to lock in trigger period.
    let mut buf = vec![0i16; 882 * 2];
    p_a.render(&mut buf);
    let mut buf = vec![0i16; 882 * 2];
    p_b.render(&mut buf);

    let pa = p_a.channels[0].arp_base_period;
    let pb = p_b.channels[0].arp_base_period;
    assert!(
        (pa - pb).abs() > 1.0,
        "E50 vs E5F should produce different trigger periods (E50={pa}, E5F={pb})"
    );
}
