//! Scream Tracker 2.21 jump / break probes.
//!
//! The seven modules under `tests/fixtures/st221-jump-probes/` are the
//! self-authored probe files staged in
//! `docs/audio/trackers/stm/fixtures/st221-jump-probes/` (byte-identical
//! copies). Each was played in the original Scream Tracker 2.21 as a
//! black box and its status line traced; the observed order / row flow
//! is recorded in that directory's `notes.md` and `trace.txt` and
//! summarised in `stm-effect-semantics-gap.md`. These tests drive this
//! crate's engine over the same files and assert the same flow, row by
//! row. (The tracker's status line shows rows 1-based; rows here are
//! 0-based.)

use oxideav_mod::stm::{extract_samples, parse_header, parse_patterns_with_end};
use oxideav_mod::stm_player::StmPlayerState;

fn load(name: &str) -> (Vec<u8>, StmPlayerState) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/st221-jump-probes")
        .join(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let hdr = parse_header(&bytes).expect("header");
    assert!(hdr.is_st221(), "{name}: probes carry the ST 2.21 header");
    assert_eq!(hdr.tempo, 0x60);
    assert_eq!(&hdr.order[..4], &[0, 1, 2, 99]);
    let (patterns, end) = parse_patterns_with_end(&hdr, &bytes);
    assert_eq!(end, 0x490 + 3 * 1024, "{name}: fixed 1 KB patterns");
    let samples = extract_samples(&hdr, &bytes);
    let player = StmPlayerState::new(&hdr, samples, patterns, 44_100);
    (bytes, player)
}

/// The `(order, row)` of every row the engine plays, until song end.
fn row_trace(p: &mut StmPlayerState) -> Vec<(usize, u8)> {
    let mut rows = Vec::new();
    while !p.ended {
        if p.tick == 0 {
            rows.push((p.order_index, p.row));
        }
        p.step_tick();
        assert!(rows.len() < 10_000, "runaway flow");
    }
    rows
}

fn rows_of(order: usize, from: u8, to_inclusive: u8) -> Vec<(usize, u8)> {
    (from..=to_inclusive).map(|r| (order, r)).collect()
}

#[test]
fn stc_break_is_immediate() {
    // `C00` on row 4: the next displayed state is order 1, row 1
    // (0-based row 0).
    let (_, mut p) = load("STC.STM");
    let trace = row_trace(&mut p);
    let mut want = rows_of(0, 0, 4);
    want.extend(rows_of(1, 0, 63));
    want.extend(rows_of(2, 0, 63));
    assert_eq!(trace, want);
    assert_eq!(p.loops, 1);
}

#[test]
fn stb_jump_alone_does_not_break() {
    // `B02` on row 4: pattern 0 plays to row 64, then order 2.
    for name in ["STB.STM", "STBN.STM", "STBAUD.STM"] {
        let (_, mut p) = load(name);
        let trace = row_trace(&mut p);
        let mut want = rows_of(0, 0, 63);
        want.extend(rows_of(2, 0, 63));
        assert_eq!(trace, want, "{name}");
    }
}

#[test]
fn stbc_and_stcb_break_to_the_jump_order() {
    // `B02` + `C00` on row 4, in either channel order: immediate break
    // to order 2.
    for name in ["STBC.STM", "STCB.STM"] {
        let (_, mut p) = load(name);
        let trace = row_trace(&mut p);
        let mut want = rows_of(0, 0, 4);
        want.extend(rows_of(2, 0, 63));
        assert_eq!(trace, want, "{name}");
    }
}

#[test]
fn stbpast_jump_to_an_unused_order_restarts_after_the_pattern() {
    // `B05` (order 5 holds 99): pattern 0 plays to its end, then the
    // song restarts at order 0 with the loop counter incremented.
    let (_, mut p) = load("STBPAST.STM");
    let trace = row_trace(&mut p);
    assert_eq!(trace, rows_of(0, 0, 63));
    assert_eq!((p.order_index, p.row, p.loops), (0, 0, 1));
}

#[test]
fn stbaud_pitch_regimes_last_one_full_pattern_each() {
    // The audio cross-check in `trace.txt`: the pattern-0 pitch regime
    // lasts one full pattern (64 rows x 0.12 s = 7.68 s) before the
    // pattern-2 regime starts — an immediate break would give ~0.6 s.
    let (bytes, mut p) = load("STBAUD.STM");
    // The square-wave sample: 4 096 unsigned bytes after the patterns.
    assert_eq!(bytes.len(), 0x490 + 3 * 1024 + 4096);
    let mut buf = vec![0i16; 2 * 44_100 * 20];
    let frames = p.render(&mut buf);
    assert!(p.ended);
    let pattern_frames = 64 * 6 * 882; // 7.68 s
    assert_eq!(frames, 2 * pattern_frames);

    // Dominant period per regime by zero-crossing count on a window in
    // the middle of each pattern. Pattern 0 plays octave 2 (the 32-byte
    // square cycle at C2SPD 8363 Hz = 261.3 Hz), pattern 2 octave 4
    // (1045.4 Hz).
    let crossings = |from: usize, len: usize| {
        let mono: Vec<i16> = buf[from * 2..(from + len) * 2]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|f| f[0])
            .collect();
        mono.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count() as f64
    };
    let window = 44_100;
    let hz0 = crossings(pattern_frames / 2, window) / 2.0;
    let hz2 = crossings(pattern_frames + pattern_frames / 2, window) / 2.0;
    assert!((hz0 - 261.3).abs() < 8.0, "pattern 0 regime: {hz0} Hz");
    assert!((hz2 - 1045.4).abs() < 30.0, "pattern 2 regime: {hz2} Hz");
    // Just before the pattern boundary the first regime still holds.
    let late = crossings(pattern_frames - window - 882, window) / 2.0;
    assert!(
        (late - 261.3).abs() < 8.0,
        "regime 0 lasts the pattern: {late}"
    );
}
