//! Diagnostic harness for the user's `halluc.mod` real-world fixture.
//!
//! Reads the file from disk (path hard-coded — it's a personal user file
//! that we deliberately don't ship in-tree), drives the decoder through
//! the registered `mod` codec, and dumps a 16-bit little-endian stereo
//! WAV alongside a per-row diagnostic CSV.
//!
//! Marked `#[ignore]` so other developers' builds don't fail when they
//! lack the file. Run explicitly:
//!
//! ```bash
//! CARGO_TARGET_DIR=/tmp/cargo-target-mod \
//!   cargo test -p oxideav-mod --test halluc_diag -- --ignored --nocapture
//! ```

use std::fs;
use std::io::Write;
use std::path::Path;

use oxideav_mod::container::OUTPUT_SAMPLE_RATE;
use oxideav_mod::header::parse_header;
use oxideav_mod::player::{parse_patterns, PlayerState};
use oxideav_mod::samples::extract_samples;

// Two equally-valid sources: Mark's local Dropbox copy (hard-coded for
// convenience) and the URL cache populated by `halluc_url_regression`.
// Whichever exists first wins.
const FIXTURE_LOCAL: &str =
    "/Users/magicaltux/Library/CloudStorage/Dropbox/perso/old_music/halluc.mod";
const FIXTURE_CACHE: &str =
    "/Users/magicaltux/projects/oxideav-workspace/target/test-fixtures/halluc.mod";

/// openmpt123 reference render for cross-correlation comparison.
/// Generate with:
///   openmpt123 --quiet --samplerate 44100 --gain 0 --no-progress \
///     --no-float --render --output-type wav --force halluc.mod
#[allow(dead_code)]
const REF_WAV: &str =
    "/Users/magicaltux/projects/oxideav-workspace/target/test-fixtures/halluc.mod.wav";

/// Read a 16-bit little-endian PCM WAV into interleaved S16 samples
/// alongside the channel count and sample rate. Tolerant minimal
/// parser — enough for an openmpt123-produced reference.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&2u16.to_le_bytes())?; // 2 channels
    f.write_all(&OUTPUT_SAMPLE_RATE.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&(2u16 * 2u16).to_le_bytes())?; // block align
    f.write_all(&16u16.to_le_bytes())?; // bits per sample
    f.write_all(b"data")?;
    f.write_all(&data_size.to_le_bytes())?;
    for s in pcm_stereo_s16 {
        f.write_all(&s.to_le_bytes())?;
    }
    Ok(())
}

#[test]
#[ignore = "requires user-local halluc.mod fixture"]
fn render_halluc_to_wav_with_per_row_diagnostics() {
    let fixture = if Path::new(FIXTURE_LOCAL).exists() {
        FIXTURE_LOCAL
    } else if Path::new(FIXTURE_CACHE).exists() {
        FIXTURE_CACHE
    } else {
        eprintln!(
            "halluc.mod not found at {FIXTURE_LOCAL} or {FIXTURE_CACHE} — skipping (run halluc_url_regression to populate the cache)"
        );
        return;
    };
    let bytes = fs::read(fixture).expect("read halluc.mod");
    println!("file size: {} bytes", bytes.len());

    let header = parse_header(&bytes).expect("parse header");
    println!("title: {:?}", header.title);
    println!(
        "signature: {:?}",
        std::str::from_utf8(&header.signature).unwrap_or("?")
    );
    println!("channels: {}", header.channels);
    println!("song_length: {}", header.song_length);
    println!("restart: {:#x}", header.restart);
    println!("n_patterns: {}", header.n_patterns);
    println!(
        "order[..song_length]: {:?}",
        &header.order[..header.song_length as usize]
    );

    println!("\nSamples (1-indexed):");
    for (i, s) in header.samples.iter().enumerate() {
        if s.length > 0 || !s.name.is_empty() {
            println!(
                "  {:2}: name={:22?} len={} ft={:+} vol={} loop_start={} loop_len={}",
                i + 1,
                s.name,
                s.length,
                s.finetune,
                s.volume,
                s.repeat_start,
                s.repeat_length,
            );
        }
    }

    let samples = extract_samples(&header, &bytes);
    let patterns = parse_patterns(&header, &bytes);
    println!("\nPattern count parsed: {}", patterns.len());

    // Render up to ~30 seconds with row/tick diagnostics so we can
    // see across the user-reported 4.5 s "breakage" boundary AND
    // through the order 0 → order 1 transition. Override with
    // env var HALLUC_SECS=N for longer renders.
    let mut player = PlayerState::new(&header, samples, patterns.clone(), OUTPUT_SAMPLE_RATE);
    let total_secs: usize = std::env::var("HALLUC_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let total_frames = OUTPUT_SAMPLE_RATE as usize * total_secs;
    let mut pcm = vec![0i16; total_frames * 2];

    // Render in small chunks so we can snapshot row/tick boundaries
    // for the CSV.
    // Render in chunks well below one tick (~128 frames so we get
    // multiple snapshots per tick when the row is interesting).
    let chunk_frames = 128;
    let mut produced = 0usize;
    let mut log = String::new();
    log.push_str("frame,sec,order,row,tick,speed,bpm,c0_smp,c0_per,c0_pos,c0_vol,c1_smp,c1_per,c1_pos,c1_vol,c2_smp,c2_per,c2_pos,c2_vol,c3_smp,c3_per,c3_pos,c3_vol\n");
    let mut last_row_logged: Option<(u8, u8)> = None;
    while produced < total_frames {
        let want = chunk_frames.min(total_frames - produced);
        let off = produced * 2;
        let written = player.render(&mut pcm[off..off + want * 2]);
        if written == 0 {
            break;
        }
        produced += written;

        let key = (player.order_index, player.row, player.tick);
        let key2 = (player.order_index, player.row);
        let _ = key;
        if last_row_logged != Some(key2) {
            last_row_logged = Some(key2);
            let sec = produced as f32 / OUTPUT_SAMPLE_RATE as f32;
            let mut row = format!(
                "{},{:.4},{},{},{},{},{}",
                produced,
                sec,
                player.order_index,
                player.row,
                player.tick,
                player.speed,
                player.bpm,
            );
            for ch in player.channels.iter() {
                row.push_str(&format!(
                    ",{},{},{:.0},{}",
                    ch.sample_index, ch.period, ch.sample_pos, ch.volume,
                ));
            }
            row.push('\n');
            log.push_str(&row);
        }
    }

    println!(
        "\nRendered {produced} frames ({:.3}s)",
        produced as f32 / OUTPUT_SAMPLE_RATE as f32
    );

    let wav_path = std::env::temp_dir().join("halluc_render.wav");
    let csv_path = std::env::temp_dir().join("halluc_rows.csv");
    write_wav(&wav_path, &pcm[..produced * 2]).expect("write wav");
    fs::write(&csv_path, log).expect("write csv");
    println!("WAV written to {}", wav_path.display());
    println!("CSV written to {}", csv_path.display());

    // Per-second cross-correlation against an openmpt123 reference, when
    // a `halluc.mod.wav` sits next to the cached fixture (see REF_WAV).
    if let Some((ref_pcm, ch, sr)) = read_wav_s16(Path::new(REF_WAV)) {
        if ch == 2 && sr == OUTPUT_SAMPLE_RATE {
            let n = (produced * 2).min(ref_pcm.len());
            let ours: Vec<f32> = pcm[..n].iter().map(|&s| s as f32).collect();
            let theirs: Vec<f32> = ref_pcm[..n].iter().map(|&s| s as f32).collect();
            let win = OUTPUT_SAMPLE_RATE as usize * 2; // 1 second of stereo S16
            for w in 0..(n / win) {
                let s = w * win;
                let e = (s + win).min(n);
                let r = pearson(&ours[s..e], &theirs[s..e]);
                println!("xcorr t={:>3}s..{:>3}s = {:.4}", w, w + 1, r);
            }
            let r_total = pearson(&ours, &theirs);
            println!("xcorr total ({} frames) = {:.4}", n / 2, r_total);
        }
    }

    // ---- Per-channel render too, so we can listen to each channel
    // independently. ----
    let header2 = parse_header(&bytes).expect("parse header");
    let samples2 = extract_samples(&header2, &bytes);
    let patterns2 = parse_patterns(&header2, &bytes);
    let mut player2 = PlayerState::new(&header2, samples2, patterns2, OUTPUT_SAMPLE_RATE);
    let n_ch = header2.channels as usize;
    let mut planes: Vec<Vec<i16>> = (0..n_ch).map(|_| vec![0i16; total_frames]).collect();
    {
        let mut views: Vec<&mut [i16]> = planes.iter_mut().map(|v| v.as_mut_slice()).collect();
        player2.render_per_channel(&mut views, total_frames);
    }
    for (i, plane) in planes.iter().enumerate() {
        let path = std::env::temp_dir().join(format!("halluc_ch{}.wav", i));
        // Write each plane as a mono WAV.
        let mut f = fs::File::create(&path).expect("ch wav");
        let n = plane.len() as u32;
        let byte_rate = OUTPUT_SAMPLE_RATE * 2;
        let data_size = n * 2;
        let chunk_size = 36 + data_size;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&chunk_size.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // mono
        f.write_all(&OUTPUT_SAMPLE_RATE.to_le_bytes()).unwrap();
        f.write_all(&byte_rate.to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_size.to_le_bytes()).unwrap();
        for s in plane {
            f.write_all(&s.to_le_bytes()).unwrap();
        }
        println!("ch{} -> {}", i, path.display());
    }

    // Print first ~5 patterns/rows of channel data to give the analyst
    // a quick view of what's playing in the first 4.5s.
    println!("\nOrders 0-3 all rows + the row 47-50 region of order 2:");
    for ord in 0..4.min(header.song_length as usize) {
        let pat_idx = header.order[ord] as usize;
        if pat_idx >= patterns.len() {
            continue;
        }
        println!("--- order {} -> pattern {}", ord, pat_idx);
        for r in 0..64 {
            print!("row {:02}: ", r);
            for ch in 0..header.channels as usize {
                let n = patterns[pat_idx].rows[r][ch];
                print!(
                    "[per={:3} smp={:2} fx={:X}{:02X}] ",
                    n.period, n.sample, n.effect, n.effect_param
                );
            }
            println!();
        }
    }
}
