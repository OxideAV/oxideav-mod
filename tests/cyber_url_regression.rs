//! URL-backed regression test for `cyber.mod` (4-channel ProTracker).
//!
//! Pins the round-105 arpeggio-base-persistence fix against the
//! user-reported "effects feel a bit off around 12-14s" complaint.
//! The fixture is hosted at
//!
//!   https://samples.oxideav.org/magicaltux/mod/cyber.mod
//!
//! and cached on disk under `target/test-fixtures/cyber.mod` after the
//! first fetch so repeat runs are offline and fast.
//!
//! Network access is opt-in: set `OXIDEAV_NETWORK_TESTS=1` (or `=true`)
//! to run. Without the flag — or when the download fails (offline
//! laptop, broken DNS, throttled CDN) — the test prints a skip message
//! and returns success, so this file is safe to check in.
//!
//! What it pins:
//!
//! 1. The bytes match the published Cloudflare blob (size 74304,
//!    SHA-256 below).
//! 2. The header parses as a 4-channel `M.K.` MOD with the expected
//!    title (`cyber`), 64 orders, 35 patterns, and the known sample
//!    name signature (`st-05:bassdrum28`, `st-07:buzzshot`).
//! 3. Pattern 1 channel-2 row 33 (the first "no new note" arpeggio
//!    continuation row in the user-reported 12-14 s window) plays
//!    its tick-1 audio at the correct semitone — base period 160
//!    (A-3 in finetune-0 row, index 29) plus the row's `034` x=3
//!    nibble, landing on `PERIOD_TABLE[0][29 + 3]` = period 135
//!    (C#-4). Pre-fix the channel was playing tick 1 at the
//!    extended-period floor (113): the previous row's last tick
//!    left period at base + y = 127, which the `enter_row` "no
//!    note" branch then captured as the NEW arp base; tick 1's +x
//!    landed past the period table's high end and got hard-clamped
//!    at 113 — a fifth higher than the chord the file calls for.
//!    See the in-file comment in `src/player.rs::enter_row` and the
//!    `arpeggio_base_persists_across_rows_without_new_note` unit
//!    test for the spec citation.
//! 4. The first 16 seconds render through the `mod` codec without
//!    panic, NaN, infinite-clip, or sustained silence.

use std::fs;
use std::io::Read;
use std::path::PathBuf;

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, Decoder, Error, Frame, Packet, SampleFormat, TimeBase,
};
use oxideav_mod::{container::OUTPUT_SAMPLE_RATE, register_codecs, CODEC_ID_STR};

const FIXTURE_URL: &str = "https://samples.oxideav.org/magicaltux/mod/cyber.mod";

/// SHA-256 of the published fixture as of 2026-04-25.
const FIXTURE_SHA256: &str = "2c9596614f5c1578730af62d2dc8f6a135686975c7b229c18179dc83748ee8a7";

const FIXTURE_BYTES: u64 = 74_304;

fn cache_path() -> PathBuf {
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let crate_dir = std::env::var("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .expect("CARGO_MANIFEST_DIR set during cargo test");
            crate_dir.join("..").join("..").join("target")
        });
    let dir = target_dir.join("test-fixtures");
    fs::create_dir_all(&dir).expect("create test-fixtures dir");
    dir.join("cyber.mod")
}

/// Tiny SHA-256. FIPS 180-4 reference implementation, byte-oriented.
fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (bytes.len() as u64) * 8;
    let mut msg = Vec::with_capacity(bytes.len() + 72);
    msg.extend_from_slice(bytes);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let mj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(mj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = String::with_capacity(64);
    for word in h {
        for byte in word.to_be_bytes() {
            out.push_str(&format!("{:02x}", byte));
        }
    }
    out
}

fn network_tests_enabled() -> bool {
    matches!(
        std::env::var("OXIDEAV_NETWORK_TESTS").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

fn fetch_with_cache() -> Option<Vec<u8>> {
    let path = cache_path();
    if let Ok(bytes) = fs::read(&path) {
        if bytes.len() as u64 == FIXTURE_BYTES && sha256_hex(&bytes) == FIXTURE_SHA256 {
            eprintln!("[cyber] using cached fixture {}", path.display());
            return Some(bytes);
        }
        eprintln!(
            "[cyber] cached fixture {} is stale (len {} / sha256 {}), re-downloading",
            path.display(),
            bytes.len(),
            sha256_hex(&bytes)
        );
        let _ = fs::remove_file(&path);
    }
    if !network_tests_enabled() {
        eprintln!(
            "[cyber] OXIDEAV_NETWORK_TESTS not set and no cached fixture at {} — skipping",
            path.display()
        );
        return None;
    }
    eprintln!("[cyber] downloading {}", FIXTURE_URL);
    let resp = match ureq::get(FIXTURE_URL).call() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[cyber] download failed ({e}) — skipping");
            return None;
        }
    };
    let mut buf = Vec::with_capacity(FIXTURE_BYTES as usize);
    if let Err(e) = resp.into_body().into_reader().read_to_end(&mut buf) {
        eprintln!("[cyber] body read failed ({e}) — skipping");
        return None;
    }
    if buf.len() as u64 != FIXTURE_BYTES {
        eprintln!(
            "[cyber] downloaded size {} != expected {} — skipping",
            buf.len(),
            FIXTURE_BYTES
        );
        return None;
    }
    let got = sha256_hex(&buf);
    if got != FIXTURE_SHA256 {
        panic!(
            "cyber.mod sha256 mismatch:\n  expected {FIXTURE_SHA256}\n  got      {got}\n\
             Either the upstream blob changed (update FIXTURE_SHA256) or the download was \
             corrupted."
        );
    }
    let tmp = path.with_extension("mod.tmp");
    if let Err(e) = fs::write(&tmp, &buf).and_then(|_| fs::rename(&tmp, &path)) {
        eprintln!("[cyber] cache write to {} failed ({e})", path.display());
    } else {
        eprintln!("[cyber] cached at {}", path.display());
    }
    Some(buf)
}

fn decode_n_seconds(bytes: Vec<u8>, max_frames: usize) -> Vec<i16> {
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    let params = CodecParameters::audio(CodecId::new(CODEC_ID_STR));
    let mut dec: Box<dyn Decoder> = reg.make_decoder(&params).expect("decoder");
    let pkt = Packet::new(0, TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64), bytes);
    dec.send_packet(&pkt).expect("send_packet");

    let mut pcm = Vec::with_capacity(max_frames * 2);
    loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(a)) => {
                assert_eq!(a.format, SampleFormat::S16, "MOD codec emits S16");
                assert_eq!(a.channels, 2, "MOD codec emits stereo");
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
    pcm.truncate(max_frames * 2);
    pcm
}

#[test]
#[ignore = "fetches cyber.mod from samples.oxideav.org; opt in via OXIDEAV_NETWORK_TESTS=1"]
fn cyber_mod_url_regression() {
    let Some(bytes) = fetch_with_cache() else {
        eprintln!("[cyber] skipped (no cache, no network)");
        return;
    };

    // ---- 1. Bytes integrity ----
    assert_eq!(bytes.len() as u64, FIXTURE_BYTES);
    assert_eq!(sha256_hex(&bytes), FIXTURE_SHA256);

    // ---- 2. Header invariants ----
    let header = oxideav_mod::header::parse_header(&bytes).expect("parse header");
    assert_eq!(header.title, "cyber");
    assert_eq!(&header.signature, b"M.K.");
    assert_eq!(header.channels, 4);
    assert_eq!(header.song_length, 64);
    assert_eq!(header.n_patterns, 35);
    let names: Vec<&str> = header
        .samples
        .iter()
        .filter(|s| s.length > 0)
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("bassdrum")),
        "expected `st-05:bassdrum28` sample, got {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("buzzshot")),
        "expected `st-07:buzzshot` sample, got {names:?}"
    );

    // ---- 3. Round-105 arpeggio fix invariant ----
    //
    // Pattern 1 (which plays in the user-reported 12-14 s window)
    // contains a stretch of effect-0xy "continuation" rows: row 32
    // triggers a new note + arpeggio (period 160 = A-3 in
    // finetune-0 row, sample 5, effect 034 → x=3, y=4), and rows
    // 33 / 37 / 49 / 53 carry no new note but keep the same effect
    // 034 active. Step the player to row 33 tick 1 (= 64 rows × 6
    // ticks of pattern 0 + 33 rows × 6 + 2 ticks = 584 step calls)
    // and assert the channel-2 period landed at
    // `PERIOD_TABLE[0][29 + 3]` = 135 — i.e. arp_base = 160 plus
    // x = 3 semitones up = C#-4. Pre-fix this measured 113 (the
    // extended-period floor), a doubly-shifted value caused by the
    // `enter_row` "no note" branch capturing the previous row's
    // last-tick modulated period (127 = base + y semis) as the new
    // arp base.
    {
        use oxideav_mod::header::parse_header;
        use oxideav_mod::player::{parse_patterns, PlayerState, PERIOD_TABLE};
        use oxideav_mod::samples::extract_samples;
        let header2 = parse_header(&bytes).expect("parse header");
        let samples = extract_samples(&header2, &bytes);
        let patterns = parse_patterns(&header2, &bytes);
        let mut player = PlayerState::new(&header2, samples, patterns, OUTPUT_SAMPLE_RATE);

        // Step to render tick 1 of (order=1, row=33):
        // - Order 0 (pattern 0) = 64 rows × 6 ticks = 384 ticks.
        // - Order 1 (pattern 1) rows 0..32 = 33 rows × 6 = 198 ticks.
        // - Row 33 ticks 0 + 1 = 2 ticks.
        // Total: 384 + 198 + 2 = 584 ticks. After this, state shows
        // (order=1, row=33, tick=2) — i.e. just rendered tick 1 of
        // row 33, and the internal counter has just advanced.
        let total_ticks = 584usize;
        let mut buf_chunk = vec![0i16; 882 * 2];
        for _ in 0..total_ticks {
            let _ = player.render(&mut buf_chunk);
        }
        // The internal counter has just incremented — we're now sitting
        // at (order=1, row=33, tick=2) state-wise, having just finished
        // tick 1's render.
        assert_eq!(player.order_index, 1);
        assert_eq!(player.row, 33);
        assert_eq!(
            player.tick, 2,
            "expected to be observing state right after tick 1 of row 33"
        );
        assert_eq!(
            player.channels[2].arp_base_period, 160,
            "arpeggio base on cyber.mod pat-1 ch-2 must persist as 160 \
             (A-3, finetune-0 row index 29) across the no-note \
             continuation row 33; pre-fix this would equal the previous \
             row's last-tick modulated period (127 = base + y semis), \
             causing the chord to shift up another (x, y) every \
             continuation row. See \
             `arpeggio_base_persists_across_rows_without_new_note` \
             unit test for the synthetic minimal repro."
        );
        // PERIOD_TABLE[0][29] = 160 (A-3). +3 semis = index 32 = 135
        // (C#-4). Pre-fix the previous row's tick-5 left period at
        // index 33 = 127; the "no note" branch captured 127 as the
        // new arp base; tick 1's +3 semis pushed past the table's
        // high end (max index 35 = 113), clamping to 113 — a fifth
        // higher than the chord the file calls for.
        assert_eq!(
            player.channels[2].period,
            PERIOD_TABLE[0][29 + 3],
            "row 33 tick 1 of pattern 1 ch-2 must play at \
             PERIOD_TABLE[0][32] = {} (A-3 + 3 semis = C#-4); pre-fix \
             this was 113 (clamped at the period table's high end).",
            PERIOD_TABLE[0][29 + 3]
        );
    }

    // ---- 4. End-to-end render invariants over the user-reported window ----
    let total_secs = 16;
    let total_frames = OUTPUT_SAMPLE_RATE as usize * total_secs;
    let pcm = decode_n_seconds(bytes, total_frames);
    assert!(!pcm.is_empty(), "decoder produced no audio");
    assert!(
        pcm.len() / 2 >= total_frames * 95 / 100,
        "decoder underran: only {} frames out of requested {}",
        pcm.len() / 2,
        total_frames
    );
    // No sustained clip rail saturation.
    let clipped = pcm
        .iter()
        .filter(|&&s| s == i16::MAX || s == i16::MIN)
        .count();
    let clip_ratio = clipped as f64 / pcm.len() as f64;
    assert!(
        clip_ratio < 0.005,
        "cyber.mod render is clipping ({clipped}/{} = {clip_ratio:.4})",
        pcm.len()
    );
    // Per-second RMS must stay strictly positive — a stalled player
    // would produce silent windows.
    let sr = OUTPUT_SAMPLE_RATE as usize;
    for w in 0..total_secs {
        let s = w * sr;
        let e = s + sr;
        let mut sum_sq = 0.0f64;
        for i in s..e {
            let v = pcm[i * 2] as f64;
            sum_sq += v * v;
        }
        let rms = (sum_sq / sr as f64).sqrt();
        assert!(
            rms > 100.0,
            "cyber.mod left RMS dropped to {:.0} at t={} s; the song has \
             continuous notes throughout, so any near-silent window means \
             the player stalled",
            rms,
            w,
        );
    }
}
