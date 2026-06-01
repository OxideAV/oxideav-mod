//! Diagnostic harness for the user's `rhmst.mod` real-world fixture.
//!
//! Mirrors `halluc_diag.rs` but for the second 4-channel ProTracker
//! sample reported breaking around the same 4.5 s mark
//! ("rhythm master"). Renders the first N seconds (default 30) into
//! a WAV under `$TMPDIR`, dumps a row/tick CSV, and emits a tiny
//! cross-correlation report against a black-box reference render
//! (`rhmst_ref.wav`) when one is available next to the fixture.
//!
//! Marked `#[ignore]` so other developers' builds don't fail when they
//! lack the file. Run explicitly:
//!
//! ```bash
//! CARGO_TARGET_DIR=/tmp/cargo-target-mod \
//!   cargo test -p oxideav-mod --test rhmst_diag -- --ignored --nocapture
//! ```

use std::fs;
use std::io::Write;
use std::path::Path;

use oxideav_mod::container::OUTPUT_SAMPLE_RATE;
use oxideav_mod::header::parse_header;
use oxideav_mod::player::{parse_patterns, PlayerState};
use oxideav_mod::samples::extract_samples;

const FIXTURE: &str = "/Users/magicaltux/projects/oxideav-workspace/target/test-fixtures/rhmst.mod";
const REF_WAV: &str =
    "/Users/magicaltux/projects/oxideav-workspace/target/test-fixtures/rhmst.mod.wav";

fn write_wav(path: &Path, pcm_stereo_s16: &[i16]) -> std::io::Result<()> {
    let mut f = fs::File::create(path)?;
    let n_samples = pcm_stereo_s16.len() as u32;
    let byte_rate = OUTPUT_SAMPLE_RATE * 2 * 2;
    let data_size = n_samples * 2;
    let chunk_size = 36 + data_size;
    f.write_all(b"RIFF")?;
    f.write_all(&chunk_size.to_le_bytes())?;
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&OUTPUT_SAMPLE_RATE.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&(2u16 * 2u16).to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_size.to_le_bytes())?;
    for s in pcm_stereo_s16 {
        f.write_all(&s.to_le_bytes())?;
    }
    Ok(())
}

/// Read a 16-bit little-endian PCM WAV into interleaved S16 samples
/// alongside the channel count and sample rate. Tolerant minimal
/// parser — enough for a black-box-reference-render-produced WAV.
fn read_wav_s16(path: &Path) -> Option<(Vec<i16>, u16, u32)> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() < 44 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut i = 12usize;
    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits = 0u16;
    let mut data: Option<&[u8]> = None;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let sz =
            u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        i += 8;
        if i + sz > bytes.len() {
            break;
        }
        if id == b"fmt " {
            channels = u16::from_le_bytes([bytes[i + 2], bytes[i + 3]]);
            sample_rate =
                u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]);
            bits = u16::from_le_bytes([bytes[i + 14], bytes[i + 15]]);
        } else if id == b"data" {
            data = Some(&bytes[i..i + sz]);
            break;
        }
        i += sz + (sz & 1);
    }
    let data = data?;
    if bits != 16 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() / 2);
    for c in data.chunks_exact(2) {
        out.push(i16::from_le_bytes([c[0], c[1]]));
    }
    Some((out, channels, sample_rate))
}

/// Pearson correlation across two equal-length f32 slices.
fn pearson(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    let n = a.len() as f32;
    let mean_a: f32 = a.iter().sum::<f32>() / n;
    let mean_b: f32 = b.iter().sum::<f32>() / n;
    let mut num = 0.0f32;
    let mut da = 0.0f32;
    let mut db = 0.0f32;
    for (&x, &y) in a.iter().zip(b.iter()) {
        let xa = x - mean_a;
        let yb = y - mean_b;
        num += xa * yb;
        da += xa * xa;
        db += yb * yb;
    }
    let denom = (da * db).sqrt().max(1e-9);
    num / denom
}

#[test]
#[ignore = "requires user-local rhmst.mod fixture"]
fn render_rhmst_to_wav_with_per_row_diagnostics() {
    if !Path::new(FIXTURE).exists() {
        eprintln!("rhmst.mod not found at {FIXTURE} — skipping");
        return;
    }
    let bytes = fs::read(FIXTURE).expect("read rhmst.mod");
    println!("file size: {} bytes", bytes.len());

    let header = parse_header(&bytes).expect("parse header");
    println!("title: {:?}", header.title);
    println!(
        "signature: {:?}",
        std::str::from_utf8(&header.signature).unwrap_or("?")
    );
    println!("channels: {}", header.channels);
    println!("song_length: {}", header.song_length);
    println!("n_patterns: {}", header.n_patterns);

    let samples = extract_samples(&header, &bytes);
    let patterns = parse_patterns(&header, &bytes);

    let total_secs: usize = std::env::var("RHMST_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let total_frames = OUTPUT_SAMPLE_RATE as usize * total_secs;

    let mut player = PlayerState::new(&header, samples, patterns, OUTPUT_SAMPLE_RATE);
    let mut pcm = vec![0i16; total_frames * 2];
    let chunk_frames = 4096;
    let mut produced = 0usize;
    while produced < total_frames {
        let want = chunk_frames.min(total_frames - produced);
        let off = produced * 2;
        let n = player.render(&mut pcm[off..off + want * 2]);
        if n == 0 {
            break;
        }
        produced += n;
    }
    println!(
        "Rendered {} frames ({:.3}s)",
        produced,
        produced as f32 / OUTPUT_SAMPLE_RATE as f32
    );

    let wav_path = std::env::temp_dir().join("rhmst_render.wav");
    write_wav(&wav_path, &pcm[..produced * 2]).expect("write wav");
    println!("WAV: {}", wav_path.display());

    // Largest single-sample inter-frame step (left + right). The
    // pre-fix render was dominated by ~5775-LSB jumps at every
    // re-trigger; the per-trigger volume ramp should bring those
    // down by an order of magnitude.
    let mut max_step_l: i32 = 0;
    let mut max_step_r: i32 = 0;
    let mut sum_step_l: u64 = 0;
    let mut sum_step_r: u64 = 0;
    let mut n_steps: u64 = 0;
    let frames_n = produced;
    for f in 1..frames_n {
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
        n_steps += 1;
    }
    if n_steps > 0 {
        println!(
            "ours max |Δsample| L={max_step_l} R={max_step_r}; mean L={:.1} R={:.1}",
            sum_step_l as f64 / n_steps as f64,
            sum_step_r as f64 / n_steps as f64,
        );
    }

    // Same step measurement on the black-box reference render — gives
    // us a reality check on what magnitude of inter-frame step is
    // actually expected of a healthy MOD renderer.
    if let Some((ref_pcm, ch, sr)) = read_wav_s16(Path::new(REF_WAV)) {
        if ch == 2 && sr == OUTPUT_SAMPLE_RATE {
            let n = (produced * 2).min(ref_pcm.len());
            let mut max_l = 0i32;
            let mut max_r = 0i32;
            let mut sum_l = 0u64;
            let mut sum_r = 0u64;
            let mut nn = 0u64;
            for f in 1..(n / 2) {
                let l0 = ref_pcm[(f - 1) * 2] as i32;
                let r0 = ref_pcm[(f - 1) * 2 + 1] as i32;
                let l1 = ref_pcm[f * 2] as i32;
                let r1 = ref_pcm[f * 2 + 1] as i32;
                let dl = (l1 - l0).abs();
                let dr = (r1 - r0).abs();
                if dl > max_l {
                    max_l = dl;
                }
                if dr > max_r {
                    max_r = dr;
                }
                sum_l += dl as u64;
                sum_r += dr as u64;
                nn += 1;
            }
            if nn > 0 {
                println!(
                    "ref  max |Δsample| L={max_l} R={max_r}; mean L={:.1} R={:.1}",
                    sum_l as f64 / nn as f64,
                    sum_r as f64 / nn as f64,
                );
            }
        }
    }

    // Reference cross-correlation when available.
    if let Some((ref_pcm, ch, sr)) = read_wav_s16(Path::new(REF_WAV)) {
        if ch != 2 || sr != OUTPUT_SAMPLE_RATE {
            println!(
                "skip xcorr — ref has {ch}ch @ {sr}Hz, expected stereo @ {}",
                OUTPUT_SAMPLE_RATE
            );
        } else {
            let n = (produced * 2).min(ref_pcm.len());
            let ours: Vec<f32> = pcm[..n].iter().map(|&s| s as f32).collect();
            let theirs: Vec<f32> = ref_pcm[..n].iter().map(|&s| s as f32).collect();

            let win_secs = 1usize;
            let win = OUTPUT_SAMPLE_RATE as usize * 2 * win_secs;
            for w in 0..(n / win) {
                let s = w * win;
                let e = (s + win).min(n);
                let r = pearson(&ours[s..e], &theirs[s..e]);
                println!(
                    "xcorr t={:>3}s..{:>3}s = {:.4}",
                    w * win_secs,
                    w * win_secs + win_secs,
                    r
                );
            }
            let r_total = pearson(&ours, &theirs);
            println!("xcorr total ({} frames) = {:.4}", n / 2, r_total);
        }
    } else {
        println!("no reference at {REF_WAV} — skip xcorr");
    }
}
