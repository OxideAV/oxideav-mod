//! STM playback integration tests: audible output, the per-step
//! commands heard in the PCM, and the staged example song.
//!
//! Fixtures are built with the crate's hidden `stm_writer` in the
//! layout Scream Tracker 2.21 writes
//! (`docs/audio/trackers/stm/stm-pattern-packing-and-order-table.md`).

use oxideav_mod::{
    stm::{StmCell, StmSampleBody, extract_samples, parse_header, parse_patterns_with_end},
    stm_player::{StmPlayerState, command},
    stm_writer::{StmWriter, StmWriterInstrument, StmWriterPattern},
};

/// 256-byte sine, unsigned 8-bit, looped over its whole length.
fn sine_instrument() -> StmWriterInstrument {
    let pcm: Vec<u8> = (0..256)
        .map(|i| {
            let phi = (i as f32) * std::f32::consts::TAU / 256.0;
            (128.0 + phi.sin() * 100.0) as u8
        })
        .collect();
    StmWriterInstrument {
        name: "SINE.SMP".into(),
        pcm,
        loop_start: 0,
        loop_end: 256,
        volume: 64,
        c2spd: 8363,
    }
}

fn writer(n_patterns: usize) -> StmWriter {
    let mut w = StmWriter::default();
    w.instruments.push(sine_instrument());
    for _ in 0..n_patterns {
        w.patterns.push(StmWriterPattern::new());
    }
    w.orders = (0..n_patterns as u8).collect();
    w
}

fn cell(note_raw: u8, instrument: u8, cmd: u8, info: u8) -> StmCell {
    StmCell {
        note_raw,
        instrument,
        command: cmd,
        command_param: info,
        ..StmCell::default()
    }
}

fn player(w: &StmWriter) -> StmPlayerState {
    let bytes = w.build();
    let hdr = parse_header(&bytes).expect("stm header");
    let (pats, _) = parse_patterns_with_end(&hdr, &bytes);
    let samples = extract_samples(&hdr, &bytes);
    StmPlayerState::new(&hdr, samples, pats, 44_100)
}

/// Approximate DFT magnitude at a target frequency.
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

fn left_channel(buf: &[i16]) -> Vec<f32> {
    buf.as_chunks::<2>().0.iter().map(|c| c[0] as f32).collect()
}

#[test]
fn stm_player_renders_an_audible_note_at_the_c2spd_pitch() {
    let mut w = writer(1);
    w.patterns[0].put(0, 0, cell(0x40, 1, 0, 0)); // C octave 4
    let mut p = player(&w);
    let n_frames = 22_050;
    let mut buf = vec![0i16; n_frames * 2];
    assert_eq!(p.render(&mut buf), n_frames);
    let signal = left_channel(&buf);
    // C2SPD 8363 Hz is C-2; C-4 is two octaves up: 33452 Hz through a
    // 256-byte cycle = 130.67 Hz.
    let f0 = 8363.0 * 4.0 / 256.0;
    let on = dft_mag(&signal, 44_100.0, f0);
    let off = dft_mag(&signal, 44_100.0, f0 * 1.5);
    assert!(on > 1000.0 && on > off * 20.0, "on {on}, off {off}");
    // One channel of four, centred: peak = 100/128 / 4 of full scale.
    let peak = buf.iter().map(|&s| s.unsigned_abs() as u32).max().unwrap();
    assert!((5800..=6600).contains(&peak), "peak {peak}");
}

#[test]
fn stm_player_silent_on_empty_pattern() {
    let w = writer(1);
    let mut p = player(&w);
    let mut buf = vec![0i16; 4410 * 2];
    assert_eq!(p.render(&mut buf), 4410);
    assert!(buf.iter().all(|&x| x == 0));
}

/// Vibrato (`H`) on a sustained note must push energy off the carrier
/// compared with the same note played dry.
#[test]
fn stm_player_vibrato_produces_spectral_sidebands() {
    let render = |cmd: u8, info: u8| {
        let mut w = writer(1);
        w.patterns[0].put(0, 0, cell(0x40, 1, cmd, info));
        for row in 1..64 {
            w.patterns[0].put(row, 0, cell(255, 0, cmd, info));
        }
        let mut p = player(&w);
        let mut buf = vec![0i16; 22_050 * 2];
        assert_eq!(p.render(&mut buf), 22_050);
        left_channel(&buf)
    };
    let vib = render(command::VIBRATO, 0x6F);
    let dry = render(0, 0);
    let carrier = 8363.0 * 4.0 / 256.0;
    let probe = carrier - 12.0;
    let sb_vib = dft_mag(&vib, 44_100.0, probe);
    let sb_dry = dft_mag(&dry, 44_100.0, probe);
    assert!(
        sb_vib > sb_dry * 1.5,
        "vibrato must boost off-carrier energy (with {sb_vib}, without {sb_dry})"
    );
}

/// Note portamento: row 0 triggers C-4, row 1 glides toward G-4 with a
/// large `G` amount; the pitch lands on G-4 without a retrigger.
#[test]
fn stm_player_note_portamento_reaches_target() {
    let mut w = writer(1);
    w.patterns[0].put(0, 0, cell(0x40, 1, 0, 0));
    w.patterns[0].put(1, 0, cell(0x47, 0, command::NOTE_PORTA, 0xFF));
    let mut p = player(&w);
    let mut buf = vec![0i16; 12_000 * 2];
    p.render(&mut buf);
    let ch = &p.channels[0];
    assert!((ch.cur_semis - 55.0).abs() < 0.01, "got {}", ch.cur_semis);
    assert!(ch.voice.active, "the glide must not stop the voice");
    assert!(ch.voice.pos > 256.0 || ch.voice.pos > 0.0);
}

/// `C` breaks to row 0 of the next order — Scream Tracker's break has
/// no row argument ("plays the next one in orderlist").
#[test]
fn stm_player_break_lands_on_row_zero_of_the_next_order() {
    let mut w = writer(2);
    w.patterns[0].put(0, 0, cell(255, 0, command::BREAK, 0x04));
    let mut p = player(&w);
    let mut buf = vec![0i16; 7 * 882 * 2];
    p.render(&mut buf);
    assert_eq!((p.order_index, p.row), (1, 0));
}

/// The song example shipped with Scream Tracker 2.21, read from the
/// staged docs tree when it is checked out next to this crate. The file
/// is a type-1 *song*: packed cells, no sample bodies — the five
/// instrument files sit beside it. Skips (passing) when absent.
#[test]
fn st221_example_song_parses_exactly_and_plays_one_finite_pass() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/audio/trackers/stm/fixtures/st221-shareware-example");
    let Ok(bytes) = std::fs::read(dir.join("SONG.STS")) else {
        eprintln!("[stm] SKIP: staged SONG.STS not found");
        return;
    };
    let hdr = parse_header(&bytes).expect("header");
    assert_eq!(hdr.title, "Example song");
    assert!(hdr.is_st221());
    assert_eq!(
        (hdr.tempo, hdr.n_patterns, hdr.global_volume),
        (0x60, 6, 64)
    );
    assert_eq!(&hdr.order[..8], &[5, 0, 1, 2, 2, 3, 4, 99]);
    assert_eq!(hdr.song_length(), 7);
    // The packed pattern data consumes the file exactly.
    let (pats, end) = parse_patterns_with_end(&hdr, &bytes);
    assert_eq!(end, bytes.len());
    assert_eq!(pats.len(), 6);
    // Pattern 4's last row carries `B01` on channel 2: the loop back
    // past the intro.
    let b = pats[4].rows[63][1];
    assert_eq!((b.command, b.command_param), (command::JUMP, 1));

    // Attach the external instrument files and play.
    let mut samples = extract_samples(&hdr, &bytes);
    assert!(samples.iter().all(|s| s.pcm.is_empty()));
    let mut attached = 0;
    for (i, inst) in hdr.instruments.iter().enumerate() {
        if inst.name.is_empty() {
            continue;
        }
        let path = dir.join("INS").join(inst.name.to_uppercase());
        if let Ok(raw) = std::fs::read(path) {
            samples[i] = StmSampleBody::from_unsigned(inst, &raw);
            assert_eq!(samples[i].pcm.len(), inst.length as usize);
            attached += 1;
        }
    }
    assert_eq!(attached, 5);
    let mut p = StmPlayerState::new(&hdr, samples, pats, 44_100);
    let mut buf = vec![0i16; 8192 * 2];
    let mut frames = 0usize;
    let mut energy = 0f64;
    loop {
        let n = p.render(&mut buf);
        if n == 0 {
            break;
        }
        frames += n;
        energy += buf[..n * 2]
            .iter()
            .map(|&s| (s as f64).powi(2))
            .sum::<f64>();
        assert!(frames < 44_100 * 600, "the pass must be finite");
    }
    assert!(p.ended);
    // The `B01` loop-back re-enters order 1: one pass is orders 0..=6.
    assert_eq!(p.order_index, 1);
    let rms = (energy / (frames as f64 * 2.0)).sqrt();
    assert!(rms > 300.0, "the song must be audible (rms {rms:.0})");
    // Seven patterns, minus the rows the intro's `C` skips, at tempos
    // between 0x30 and 0x60: between 20 s and 60 s.
    let secs = frames as f64 / 44_100.0;
    assert!((20.0..60.0).contains(&secs), "{secs:.2} s");
}
