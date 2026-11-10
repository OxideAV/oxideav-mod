//! FastTracker 2.08 jump / break probes.
//!
//! The seven modules under `tests/fixtures/ft208-jump-probes/` are the
//! self-authored probe files staged in
//! `docs/audio/trackers/xm/fixtures/ft208-jump-probes/` (byte-identical
//! copies: v0104, 4 channels, 64-row patterns, no instruments, speed
//! 12). Each was played in the original FastTracker 2.08 as a black box
//! and its song-position box and row number traced; the results are
//! recorded in that directory's `notes.md` / `trace.txt` and ruled on in
//! `docs/audio/trackers/xm/xm-jump-break-precedence.md`. These tests
//! drive this crate's engine over the same files and assert the same
//! `(song position, row)` flow.
//!
//! The trace's one-frame "songpos = xx − 1 on the `B` row" state is a
//! display artefact of the tracker (see the note), not a played row.

use oxideav_mod::xm;
use oxideav_mod::xm_player::XmPlayerState;

fn load(name: &str) -> XmPlayerState {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ft208-jump-probes")
        .join(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let hdr = xm::parse_header(&bytes).expect("xm header");
    assert_eq!(hdr.song_length, 3, "{name}: order list 0 1 2");
    assert_eq!(hdr.num_channels, 4);
    assert_eq!(hdr.default_tempo, 12);
    let (patterns, after) = xm::parse_patterns(&hdr, &bytes).expect("patterns");
    assert!(patterns.iter().all(|p| p.num_rows == 64));
    let mut instruments = xm::parse_instruments(&hdr, &bytes, after).expect("instruments");
    xm::extract_sample_bodies(&mut instruments, &bytes);
    XmPlayerState::new(&hdr, instruments, patterns, 44_100)
}

/// The `(song position, row)` of the first `n` rows the engine plays.
fn row_trace(p: &mut XmPlayerState, n: usize) -> Vec<(usize, u16)> {
    let mut rows = Vec::new();
    while rows.len() < n {
        // A row is recorded when its first tick is about to start; the
        // engine is then advanced by exactly one tick of audio.
        if p.tick == 0 && p.tick_sample_cursor == 0 {
            rows.push((p.order_index, p.row));
        }
        let spt = p.samples_per_tick() as usize;
        let mut tick_buf = vec![0i16; spt * 2];
        if p.render(&mut tick_buf) == 0 {
            break;
        }
    }
    rows
}

/// Rows 0..=4 of song position 0, then `rows` consecutive rows from
/// `(pos, row)`.
fn expect(pos: usize, row: u16, rows: u16) -> Vec<(usize, u16)> {
    let mut v: Vec<(usize, u16)> = (0..=4).map(|r| (0, r)).collect();
    v.extend((row..row + rows).map(|r| (pos, r)));
    v
}

#[test]
fn xb_position_jump_alone_lands_on_row_zero() {
    // `B02` on row 4 -> songpos 2, row 0.
    let mut p = load("XB.XM");
    assert_eq!(row_trace(&mut p, 5 + 40), expect(2, 0, 40));
}

#[test]
fn xd_pattern_break_alone_goes_to_the_next_position() {
    // `D05` on row 4 -> songpos 1, row 5.
    let mut p = load("XD.XM");
    assert_eq!(row_trace(&mut p, 5 + 40), expect(1, 5, 40));
}

#[test]
fn xd10_break_parameter_is_two_decimal_digits() {
    // `D10` -> row 0x0A, not 0x10.
    let mut p = load("XD10.XM");
    assert_eq!(row_trace(&mut p, 5 + 40), expect(1, 10, 40));
}

#[test]
fn xbd_jump_left_of_break_merges_position_and_row() {
    // ch1 `B02`, ch2 `D05` -> songpos 2, row 5.
    let mut p = load("XBD.XM");
    assert_eq!(row_trace(&mut p, 5 + 40), expect(2, 5, 40));
}

#[test]
fn xdb_jump_right_of_break_discards_the_break_row() {
    // ch1 `D05`, ch2 `B02` -> songpos 2, row 0.
    let mut p = load("XDB.XM");
    assert_eq!(row_trace(&mut p, 5 + 40), expect(2, 0, 40));
}

#[test]
fn xbpast_out_of_range_jump_goes_to_restart_position_zero() {
    // `B05` with song length 3, restart position 0 -> songpos 0, row 0,
    // over and over (the trace shows the 5-row cycle repeating).
    let mut p = load("XBPAST.XM");
    assert_eq!(p.restart_position, 0);
    p.stop_on_repeat = false;
    let cycle: Vec<(usize, u16)> = (0..=4).map(|r| (0, r)).collect();
    let want: Vec<(usize, u16)> = cycle.iter().cycle().take(25).copied().collect();
    assert_eq!(row_trace(&mut p, 25), want);
    assert!(!p.ended);
    assert_eq!(p.loops, 4);

    // By default the first wrap ends the pass: five rows of 12 ticks.
    let mut p = load("XBPAST.XM");
    let mut buf = vec![0i16; 2 * 44_100];
    let mut frames = 0usize;
    loop {
        let n = p.render(&mut buf);
        if n == 0 {
            break;
        }
        frames += n;
        assert!(frames < 44_100 * 60, "the pass must be finite");
    }
    assert!(p.ended);
    assert_eq!(frames, 5 * 12 * p.samples_per_tick() as usize);
}

#[test]
fn xbpastr_out_of_range_jump_goes_to_the_restart_position() {
    // Same `B05`, header restart position 1 -> songpos 1, row 0: the
    // out-of-range jump takes the restart position, not a hard-coded 0.
    let mut p = load("XBPASTR.XM");
    assert_eq!(p.restart_position, 1);
    assert_eq!(row_trace(&mut p, 5 + 40), expect(1, 0, 40));
    assert!(!p.ended, "position 1 had not been played yet");
}
