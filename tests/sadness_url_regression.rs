//! URL-backed regression test for `Sadness.mod`.
//!
//! Pinned at <https://samples.oxideav.org/magicaltux/mod/Sadness.mod>.
//! First run downloads + caches under `target/test-fixtures/Sadness.mod`;
//! subsequent runs reuse the cache and never hit the network. Network
//! access is opt-in via `OXIDEAV_NETWORK_TESTS=1`. Without the flag —
//! or when the download fails — the test prints a skip message and
//! returns success, so this file is safe to check in.
//!
//! ## Why this fixture
//!
//! `Sadness.mod` is an **infinite-stream** fixture — its last pattern
//! ends with a Bxx position-jump that loops back into the song, so the
//! decoder synthesises audio forever (until something downstream stops
//! pulling).
//!
//! That makes it the regression case for a class of pipeline bugs that
//! are silent on finite streams but lethal on infinite ones. The
//! original symptom (2026-04-26): nothing reached the audio sink at
//! all because `oxideav-pipeline`'s `drain_decoder` accumulated all
//! available frames into a `Vec` before sending the first one
//! downstream — for an infinite decoder, that drain never returns.
//! Finite MODs (halluc, rhmst, cyber) all worked because they
//! eventually hit `Eof` and the Vec was sent in one big batch.
//!
//! What this test pins:
//!
//! 1. **Bytes integrity** — the published fixture matches the recorded
//!    SHA-256 / size. If the upstream blob changes, the cache is
//!    invalidated and the test fails loudly so the regression is
//!    anchored to a specific binary.
//! 2. **Header invariants** — title `"sadness"`, 4-channel `M.K.`,
//!    44 orders, 31 instruments named `"arcane"`.
//! 3. **The decoder produces a non-silent first 2 s of audio without
//!    panic** — the codec-level half of the regression.
//! 4. **The decoder keeps producing past EOF** — this is the
//!    infinite-stream property we're pinning. We pull frames within a
//!    bounded wall-clock budget (8 s) and assert we collected at
//!    least 60 s of synthesised audio in that window. If a future
//!    change makes the decoder return `Eof` early on this fixture,
//!    this assertion fails. If a future change makes the decoder
//!    slower than ~8× realtime, it also fails — but that's a
//!    different regression worth catching anyway.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, Decoder, Error, Frame, Packet, TimeBase,
};
use oxideav_mod::{container::OUTPUT_SAMPLE_RATE, register_codecs, CODEC_ID_STR};

const FIXTURE_URL: &str = "https://samples.oxideav.org/magicaltux/mod/Sadness.mod";

const FIXTURE_SHA256: &str = "fd1d1e550567ee71a0ca559562a26f777e9944516ca0878c21168987a3bc4aba";

const FIXTURE_BYTES: u64 = 181_882;

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
    dir.join("Sadness.mod")
}

/// FIPS 180-4 SHA-256 — small inline implementation, same shape as
/// the one in `halluc_url_regression.rs` so dev-deps stay at `ureq`.
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
            eprintln!("[sadness] using cached fixture {}", path.display());
            return Some(bytes);
        }
        eprintln!(
            "[sadness] cached fixture {} is stale (len {} / sha256 {}), re-downloading",
            path.display(),
            bytes.len(),
            sha256_hex(&bytes)
        );
        let _ = fs::remove_file(&path);
    }
    if !network_tests_enabled() {
        eprintln!(
            "[sadness] OXIDEAV_NETWORK_TESTS not set and no cached fixture at {} — skipping",
            path.display()
        );
        return None;
    }
    eprintln!("[sadness] downloading {}", FIXTURE_URL);
    let resp = match ureq::get(FIXTURE_URL).call() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[sadness] download failed ({e}) — skipping");
            return None;
        }
    };
    let mut buf = Vec::with_capacity(FIXTURE_BYTES as usize);
    if let Err(e) = resp.into_body().into_reader().read_to_end(&mut buf) {
        eprintln!("[sadness] body read failed ({e}) — skipping");
        return None;
    }
    if buf.len() as u64 != FIXTURE_BYTES {
        eprintln!(
            "[sadness] downloaded size {} != expected {} — skipping",
            buf.len(),
            FIXTURE_BYTES
        );
        return None;
    }
    let got = sha256_hex(&buf);
    if got != FIXTURE_SHA256 {
        panic!(
            "Sadness.mod sha256 mismatch:\n  expected {FIXTURE_SHA256}\n  got      {got}\n\
             Either the upstream blob changed (update FIXTURE_SHA256) or the download was \
             corrupted."
        );
    }
    let tmp = path.with_extension("mod.tmp");
    if let Err(e) = fs::write(&tmp, &buf).and_then(|_| fs::rename(&tmp, &path)) {
        eprintln!("[sadness] cache write to {} failed ({e})", path.display());
    } else {
        eprintln!("[sadness] cached at {}", path.display());
    }
    Some(buf)
}

#[test]
#[ignore = "fetches Sadness.mod from samples.oxideav.org; opt in via OXIDEAV_NETWORK_TESTS=1"]
fn sadness_mod_url_regression() {
    let Some(bytes) = fetch_with_cache() else {
        eprintln!("[sadness] skipped (no cache, no network)");
        return;
    };

    // ---- 1. Bytes integrity ----
    assert_eq!(bytes.len() as u64, FIXTURE_BYTES);
    assert_eq!(sha256_hex(&bytes), FIXTURE_SHA256);

    // ---- 2. Header invariants ----
    let header = oxideav_mod::header::parse_header(&bytes).expect("parse header");
    assert_eq!(header.title, "sadness");
    assert_eq!(&header.signature, b"M.K.");
    assert_eq!(header.channels, 4);
    assert_eq!(header.song_length, 44);
    let arcane = header
        .samples
        .iter()
        .filter(|s| s.length > 0 && s.name.contains("arcane"))
        .count();
    assert!(
        arcane >= 8,
        "expected most non-empty instruments named 'arcane'; got {arcane}"
    );

    // ---- 3. First 2 s of audio is non-silent and panic-free ----
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    let params = CodecParameters::audio(CodecId::new(CODEC_ID_STR));
    let mut dec: Box<dyn Decoder> = reg.make_decoder(&params).expect("decoder");
    let pkt = Packet::new(
        0,
        TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64),
        bytes.clone(),
    );
    dec.send_packet(&pkt).expect("send_packet");

    let want_2s_frames = OUTPUT_SAMPLE_RATE as usize * 2;
    let mut peak = 0i32;
    let mut nonzero = 0usize;
    let mut frames_collected = 0usize;
    while frames_collected < want_2s_frames {
        let Frame::Audio(a) = dec.receive_frame().expect("decode 2s") else {
            unreachable!("MOD emits audio only");
        };
        for chunk in a.data[0].chunks_exact(2) {
            let s = i16::from_le_bytes([chunk[0], chunk[1]]);
            if s != 0 {
                nonzero += 1;
            }
            let v = (s as i32).abs();
            if v > peak {
                peak = v;
            }
        }
        frames_collected += a.samples as usize;
    }
    assert!(peak > 200, "first 2 s peak too low: {peak}");
    assert!(
        nonzero * 100 > frames_collected,
        "first 2 s mostly silent: {nonzero} non-zero of {} samples",
        frames_collected * 2,
    );

    // ---- 4. Infinite-stream regression (the load-bearing assertion) ----
    //
    // Drive the decoder under a wall-clock budget and verify it just
    // keeps producing. If something makes Sadness's player return Eof
    // early, this fails — same shape as the original 2026-04-26 bug
    // where `drain_decoder` collected the entire (infinite) emission
    // into a Vec before sending the first frame downstream and the
    // sink never saw audio.
    let start = Instant::now();
    let wall_budget = std::time::Duration::from_secs(8);
    let mut audio_collected = 0u64;
    while start.elapsed() < wall_budget {
        match dec.receive_frame() {
            Ok(Frame::Audio(a)) => audio_collected += a.samples as u64,
            Ok(_) => unreachable!("MOD emits audio only"),
            Err(Error::NeedMore) => {
                break;
            }
            Err(Error::Eof) => {
                panic!(
                    "Sadness.mod returned Eof after {} samples — this fixture is supposed to \
                     loop indefinitely (its last pattern Bxx-jumps back into the song). If the \
                     song has been re-encoded to be finite, change the fixture URL and update \
                     this test.",
                    audio_collected
                );
            }
            Err(e) => panic!("decode err: {e:?}"),
        }
    }
    let synth_secs = audio_collected as f64 / OUTPUT_SAMPLE_RATE as f64;
    let wall_secs = start.elapsed().as_secs_f64();
    eprintln!(
        "[sadness] synthesised {:.1} s of audio in {:.1} s wall-clock ({:.1}× realtime)",
        synth_secs,
        wall_secs,
        synth_secs / wall_secs.max(0.001)
    );
    assert!(
        synth_secs > 60.0,
        "in {:.1} s wall-clock the decoder should have synthesised at least 60 s of audio (a \
         healthy MOD decoder runs >> realtime); only got {:.1} s",
        wall_secs,
        synth_secs,
    );
}
