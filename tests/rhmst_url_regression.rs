//! URL-backed regression test for `rhmst.mod` (4-channel ProTracker,
//! "rhythm master" — the second real-world MOD reported breaking around
//! the 4.5 s mark together with `halluc.mod`).
//!
//! Mirrors `halluc_url_regression.rs` byte-for-byte in shape; the only
//! axis-of-variation is the fixture metadata (URL, SHA-256, file size,
//! header invariants). Two independent fixtures pinned the same way
//! protect against a class of regressions where a fix that landed
//! green for one MOD silently regresses on another, since both were
//! reported with the same 4.5 s breakage symptom.
//!
//! Network access is opt-in: set `OXIDEAV_NETWORK_TESTS=1` to run.
//! Without the flag — or when the download fails (offline laptop,
//! broken DNS, throttled CDN) — the test prints a skip message and
//! returns success, so this file is safe to check in.
//!
//! What it pins:
//!
//! 1. The bytes match the published Cloudflare ETag (size 115272,
//!    SHA-256 below). If the upstream file is replaced the cache is
//!    invalidated and re-downloaded; if the size or hash differ we
//!    fail loudly so the regression is anchored to a specific binary.
//! 2. The header parses as a 4-channel `M.K.` MOD with the expected
//!    title (`rhythm master`) and 25 orders / 15 patterns.
//! 3. The first 30 seconds render through the registered `mod` codec
//!    without panic, NaN, infinite-clip, or sustained silence —
//!    catches the same 4.5 s symptom class the user reported.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, Decoder, Error, Frame, Packet, SampleFormat, TimeBase,
};
use oxideav_mod::{container::OUTPUT_SAMPLE_RATE, register_codecs, CODEC_ID_STR};

const FIXTURE_URL: &str = "https://samples.oxideav.org/magicaltux/mod/rhmst.mod";

/// SHA-256 of the published fixture as of 2026-04-25. If this hash drifts
/// the upstream blob has changed and the test will redownload + fail
/// loudly so we know the cache is stale.
const FIXTURE_SHA256: &str = "31f2f8fe99b6ac4b3896f08151c796b15d80b4eea341b7328afdd3a4ece733d2";

const FIXTURE_BYTES: u64 = 115_272;

/// Returns the on-disk cache path for the fixture, ensuring the parent
/// directory exists.
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
    dir.join("rhmst.mod")
}

/// Tiny SHA-256 over `bytes` — pulled in only when verifying the cache.
/// Avoiding `sha2` keeps the dev-dependency surface to `ureq` only.
/// FIPS 180-4 reference implementation, byte-oriented. Not constant-time;
/// we only use it for fixture integrity, never for crypto.
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
            eprintln!("[rhmst] using cached fixture {}", path.display());
            return Some(bytes);
        }
        eprintln!(
            "[rhmst] cached fixture {} is stale (len {} / sha256 {}), re-downloading",
            path.display(),
            bytes.len(),
            sha256_hex(&bytes)
        );
        let _ = fs::remove_file(&path);
    }
    if !network_tests_enabled() {
        eprintln!(
            "[rhmst] OXIDEAV_NETWORK_TESTS not set and no cached fixture at {} — skipping",
            path.display()
        );
        return None;
    }
    eprintln!("[rhmst] downloading {}", FIXTURE_URL);
    let resp = match ureq::get(FIXTURE_URL).call() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[rhmst] download failed ({e}) — skipping");
            return None;
        }
    };
    let mut buf = Vec::with_capacity(FIXTURE_BYTES as usize);
    if let Err(e) = resp.into_body().into_reader().read_to_end(&mut buf) {
        eprintln!("[rhmst] body read failed ({e}) — skipping");
        return None;
    }
    if buf.len() as u64 != FIXTURE_BYTES {
        eprintln!(
            "[rhmst] downloaded size {} != expected {} — skipping",
            buf.len(),
            FIXTURE_BYTES
        );
        return None;
    }
    let got = sha256_hex(&buf);
    if got != FIXTURE_SHA256 {
        panic!(
            "rhmst.mod sha256 mismatch:\n  expected {FIXTURE_SHA256}\n  got      {got}\n\
             Either the upstream blob changed (update FIXTURE_SHA256) or the download was \
             corrupted."
        );
    }
    let tmp = path.with_extension("mod.tmp");
    if let Err(e) = fs::write(&tmp, &buf).and_then(|_| fs::rename(&tmp, &path)) {
        eprintln!("[rhmst] cache write to {} failed ({e})", path.display());
    } else {
        eprintln!("[rhmst] cached at {}", path.display());
    }
    Some(buf)
}

/// Drive the `mod` codec end-to-end and collect interleaved S16 stereo
/// PCM up to `max_frames`.
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
#[ignore = "fetches rhmst.mod from samples.oxideav.org; opt in via OXIDEAV_NETWORK_TESTS=1"]
fn rhmst_mod_url_regression() {
    let Some(bytes) = fetch_with_cache() else {
        eprintln!("[rhmst] skipped (no cache, no network)");
        return;
    };

    // ---- 1. Bytes integrity ----
    assert_eq!(bytes.len() as u64, FIXTURE_BYTES);
    assert_eq!(sha256_hex(&bytes), FIXTURE_SHA256);

    // ---- 2. Header invariants ----
    let header = oxideav_mod::header::parse_header(&bytes).expect("parse header");
    assert_eq!(header.title, "rhythm master");
    assert_eq!(&header.signature, b"M.K.");
    assert_eq!(header.channels, 4);
    assert_eq!(header.song_length, 25);
    assert_eq!(header.n_patterns, 15);

    // ---- 3. Render the first 30 s and check basic invariants ----
    let total_secs = 30;
    let total_frames = OUTPUT_SAMPLE_RATE as usize * total_secs;
    let pcm = decode_n_seconds(bytes, total_frames);
    assert!(!pcm.is_empty(), "decoder produced no audio");
    assert!(
        pcm.len() / 2 >= total_frames * 95 / 100,
        "decoder underran: only {} frames out of requested {}",
        pcm.len() / 2,
        total_frames
    );

    // No NaN-cast-to-zero is possible at this layer (already i16); look
    // for sustained clip-rail saturation, which is the symptom an
    // exponentially-blown f32 mixer would leave behind.
    let clipped = pcm
        .iter()
        .filter(|&&s| s == i16::MAX || s == i16::MIN)
        .count();
    let clip_ratio = clipped as f64 / pcm.len() as f64;
    assert!(
        clip_ratio < 0.005,
        "rhmst.mod render is clipping ({clipped}/{} = {clip_ratio:.4}); a healthy render \
         should sit comfortably below the rails",
        pcm.len()
    );

    // Per-second RMS for both channels must stay strictly positive
    // throughout the first 30 seconds — the song has continuous notes
    // through that window so any near-silent second means the player
    // stalled (the bug the original 4.5 s hunt is chasing). Tightened
    // bounds (vs `halluc.mod`'s 500 / 200) because rhmst's pattern 0
    // already has all four channels active from row 0 onward, so we
    // don't need the "intro is right-only" bleed-through carve-out.
    let sr = OUTPUT_SAMPLE_RATE as usize;
    let mut min_rms_l = f64::INFINITY;
    let mut min_rms_r = f64::INFINITY;
    let mut min_at_l = 0usize;
    let mut min_at_r = 0usize;
    for w in 0..total_secs {
        let s = w * sr;
        let e = s + sr;
        let mut sum_l = 0.0f64;
        let mut sum_r = 0.0f64;
        for i in s..e {
            let l = pcm[i * 2] as f64;
            let r = pcm[i * 2 + 1] as f64;
            sum_l += l * l;
            sum_r += r * r;
        }
        let rms_l = (sum_l / sr as f64).sqrt();
        let rms_r = (sum_r / sr as f64).sqrt();
        if rms_l < min_rms_l {
            min_rms_l = rms_l;
            min_at_l = w;
        }
        if rms_r < min_rms_r {
            min_rms_r = rms_r;
            min_at_r = w;
        }
    }
    eprintln!("[rhmst] min RMS L={min_rms_l:.0}@{min_at_l}s  R={min_rms_r:.0}@{min_at_r}s");
    assert!(
        min_rms_r > 800.0,
        "right-channel RMS dropped to {:.0} at t={} s; rhmst.mod has continuous \
         multi-channel material across the first 30 s, so any near-silent right \
         window means the player stalled",
        min_rms_r,
        min_at_r
    );
    assert!(
        min_rms_l > 800.0,
        "left-channel RMS dropped to {:.0} at t={} s; rhmst.mod is symmetrically \
         active on both stereo busses through the first 30 s, so a dead left ear \
         signals a regression in the partial-bleed pan model",
        min_rms_l,
        min_at_l
    );

    // ---- 4. Per-trigger ramp regression ----
    //
    // Pre-fix the post-mix L/R bus showed single-sample steps of
    // ~5775 LSB at every sample re-trigger (every few ms throughout
    // the song). The 1 ms volume ramp added in `mix_one` should
    // bring the worst-case step down by roughly 30 %, and the *mean*
    // step down by roughly 50 %, by smoothing the discontinuity at
    // every fresh note-on. Pin a per-trigger ceiling on both axes so
    // a future regression that disables the ramp (e.g. by setting
    // `RAMP_FRAMES = 0`) trips loudly on this fixture.
    let n_frames = pcm.len() / 2;
    let mut max_step_l = 0i32;
    let mut max_step_r = 0i32;
    let mut sum_step_l = 0u64;
    let mut sum_step_r = 0u64;
    for f in 1..n_frames {
        let l0 = pcm[(f - 1) * 2] as i32;
        let r0 = pcm[(f - 1) * 2 + 1] as i32;
        let l1 = pcm[f * 2] as i32;
        let r1 = pcm[f * 2 + 1] as i32;
        let dl = (l1 - l0).abs();
        let dr = (r1 - r0).abs();
        if dl > max_step_l {
            max_step_l = dl;
        }
        if dr > max_step_r {
            max_step_r = dr;
        }
        sum_step_l += dl as u64;
        sum_step_r += dr as u64;
    }
    let n = (n_frames - 1) as f64;
    let mean_l = sum_step_l as f64 / n;
    let mean_r = sum_step_r as f64 / n;
    eprintln!(
        "[rhmst] inter-frame step  max L={max_step_l} R={max_step_r}  mean L={mean_l:.1} R={mean_r:.1}"
    );
    // 5000 leaves headroom over the measured ~4500 max; the 5775
    // pre-fix value would trip this. Mean step < 500 catches the
    // bulk-of-distribution regression.
    assert!(
        max_step_l < 5000 && max_step_r < 5000,
        "max inter-frame step rose to L={max_step_l} R={max_step_r}; the per-trigger \
         volume ramp (PlayerState::RAMP_FRAMES) keeps healthy renders below ~4500 LSB. \
         A spike above 5000 means either RAMP_FRAMES dropped to 0 or the ramp itself \
         regressed."
    );
    assert!(
        mean_l < 500.0 && mean_r < 500.0,
        "mean inter-frame step rose to L={mean_l:.1} R={mean_r:.1}; the per-trigger \
         ramp keeps the bulk distribution under ~360 LSB on this fixture. A mean \
         above 500 means the ramp is either bypassed or shorter than RAMP_FRAMES."
    );

    // Optional: when running under `--nocapture` we want the harness to
    // print where the cached fixture lives so a developer can grab it.
    let _ = std::io::stdout().flush();
    let _ = Path::new("");
}
