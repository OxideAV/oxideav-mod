//! Impulse Tracker black-box render comparison.
//!
//! Synthetic `.it` fixtures (built with the crate's own writer) are
//! rendered by this crate and by an installed command-line player —
//! `openmpt123 --render` or `xmp -o` — invoked strictly as opaque
//! binaries. Only their PCM output is consumed; no player source is
//! read or referenced. When neither binary is on `PATH` every test
//! prints a SKIP line and passes, so CI (which has no oracle) stays
//! green while a developer machine gets the real gate.
//!
//! What is compared, per fixture:
//!
//! - **Pitch**: the dominant frequency of each analysis window (FFT
//!   peak of the mono mix), which pins the pitch table, slide maths,
//!   vibrato depth/rate and envelope units independently of gain.
//! - **Envelope**: the RMS of each window, normalised by the loudest
//!   window, which pins volume slides, envelopes, fadeout, tremolo and
//!   tremor independently of the players' absolute gain.
//! - **Timing**: the onset positions of notes (RMS rising edges),
//!   which pins speed / tempo / delays / jumps.
//!
//! Every threshold below is a conformance *floor*; the verbose mode
//! (`OXIDEAV_IT_ORACLE_VERBOSE=1`) prints the per-window numbers.

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_mod::it::{
    parse_module, ItCell, IT_ENV_LOOP, IT_ENV_ON, IT_ENV_SUSTAIN_LOOP, IT_NOTE_OFF,
};
use oxideav_mod::it_player::ItPlayerState;
use oxideav_mod::it_writer::{
    cell_effect, cell_note, square_sample, with_effect, with_volpan, ItWriter, ItWriterEnvelope,
    ItWriterInstrument, ItWriterPattern, ItWriterSample,
};

const RATE: u32 = 44_100;
/// Analysis window: one row at speed 6 / tempo 125 = 6 × 882 frames.
const ROW_FRAMES: usize = 6 * 882;

fn verbose() -> bool {
    std::env::var_os("OXIDEAV_IT_ORACLE_VERBOSE").is_some()
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join(bin))
        .find(|p| p.is_file())
}

fn fixture_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("it-oracle");
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

/// Which oracle to use, or `None` (skip).
enum Oracle {
    OpenMpt(PathBuf),
    Xmp(PathBuf),
}

fn oracle() -> Option<Oracle> {
    if let Some(p) = which("openmpt123") {
        return Some(Oracle::OpenMpt(p));
    }
    which("xmp").map(Oracle::Xmp)
}

/// Render `it_bytes` through the oracle; returns mono f32 at 44.1 kHz.
fn oracle_render(oracle: &Oracle, name: &str, it_bytes: &[u8]) -> Option<Vec<f32>> {
    let st = oracle_render_stereo(oracle, name, it_bytes)?;
    Some(st.chunks_exact(2).map(|f| (f[0] + f[1]) / 2.0).collect())
}

/// Render through the oracle; returns interleaved stereo f32.
fn oracle_render_stereo(oracle: &Oracle, name: &str, it_bytes: &[u8]) -> Option<Vec<f32>> {
    let dir = fixture_dir();
    let it_path = dir.join(format!("{name}.it"));
    std::fs::write(&it_path, it_bytes).ok()?;
    let wav_path = match oracle {
        Oracle::OpenMpt(bin) => {
            let out = dir.join(format!("{name}.it.wav"));
            let _ = std::fs::remove_file(&out);
            let status = Command::new(bin)
                .args([
                    "--render",
                    "--quiet",
                    "--samplerate",
                    "44100",
                    "--channels",
                    "2",
                    "--no-float",
                    "--gain",
                    "0",
                    "--stereo",
                    "100",
                    "--filter",
                    "1",
                    "--repeat",
                    "0",
                    "--output-type",
                    "wav",
                ])
                .arg(&it_path)
                .status()
                .ok()?;
            if !status.success() {
                return None;
            }
            out
        }
        Oracle::Xmp(bin) => {
            let out = dir.join(format!("{name}.xmp.wav"));
            let _ = std::fs::remove_file(&out);
            let status = Command::new(bin)
                .args(["-o"])
                .arg(&out)
                .args(["-f", "44100", "-i", "nearest", "--nocmd", "-b", "16"])
                .arg(&it_path)
                .status()
                .ok()?;
            if !status.success() {
                return None;
            }
            out
        }
    };
    read_wav_stereo(&std::fs::read(&wav_path).ok()?)
}

/// Minimal RIFF/WAVE reader (PCM16 or IEEE float32) → interleaved
/// stereo f32 (mono input is duplicated).
fn read_wav_stereo(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u16)> = None; // (format, channels, bits)
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body = &bytes[pos + 8..(pos + 8 + size).min(bytes.len())];
        if id == b"fmt " && body.len() >= 16 {
            let mut format = u16::from_le_bytes([body[0], body[1]]);
            let channels = u16::from_le_bytes([body[2], body[3]]);
            let bits = u16::from_le_bytes([body[14], body[15]]);
            if format == 0xFFFE && body.len() >= 26 {
                format = u16::from_le_bytes([body[24], body[25]]);
            }
            fmt = Some((format, channels, bits));
        } else if id == b"data" {
            let (format, channels, bits) = fmt?;
            let ch = channels.max(1) as usize;
            let mut out = Vec::new();
            let push = |out: &mut Vec<f32>, l: f32, r: f32| {
                out.push(l);
                out.push(r);
            };
            match (format, bits) {
                (1, 16) => {
                    for frame in body.chunks_exact(2 * ch) {
                        let v = |c: usize| {
                            i16::from_le_bytes([frame[2 * c], frame[2 * c + 1]]) as f32 / 32768.0
                        };
                        push(&mut out, v(0), v(if ch > 1 { 1 } else { 0 }));
                    }
                }
                (3, 32) => {
                    for frame in body.chunks_exact(4 * ch) {
                        let v = |c: usize| {
                            f32::from_le_bytes([
                                frame[4 * c],
                                frame[4 * c + 1],
                                frame[4 * c + 2],
                                frame[4 * c + 3],
                            ])
                        };
                        push(&mut out, v(0), v(if ch > 1 { 1 } else { 0 }));
                    }
                }
                _ => return None,
            }
            return Some(out);
        }
        pos += 8 + size + (size & 1);
    }
    None
}

/// Render through this crate; returns mono f32 plus interleaved stereo
/// f32.
fn our_render(it_bytes: &[u8], max_frames: usize) -> (Vec<f32>, Vec<f32>) {
    let module = parse_module(it_bytes).expect("fixture parses");
    let mut p = ItPlayerState::new(module, RATE);
    let mut stereo = Vec::new();
    let mut buf = vec![0i16; 4096];
    while stereo.len() / 2 < max_frames {
        let n = p.render(&mut buf);
        if n == 0 {
            break;
        }
        stereo.extend_from_slice(&buf[..n * 2]);
    }
    let mono = stereo
        .chunks_exact(2)
        .map(|f| (f[0] as f32 + f[1] as f32) / 65536.0)
        .collect();
    let stereo_f: Vec<f32> = stereo.iter().map(|&v| v as f32 / 32768.0).collect();
    (mono, stereo_f)
}

/// Per-row right-channel share `R / (L + R)` of the RMS (0 = hard left,
/// 0.5 = centre, 1 = hard right); silent rows report 0.5.
fn balance_profile(st: &[f32], rows: usize) -> Vec<f32> {
    (0..rows)
        .map(|r| {
            let s = r * ROW_FRAMES * 2;
            let e = (s + ROW_FRAMES * 2).min(st.len());
            if s >= e {
                return 0.5;
            }
            let l: Vec<f32> = st[s..e].iter().step_by(2).copied().collect();
            let rr: Vec<f32> = st[s..e].iter().skip(1).step_by(2).copied().collect();
            let (lr, rr) = (rms(&l), rms(&rr));
            if lr + rr < 1e-5 {
                0.5
            } else {
                rr / (lr + rr)
            }
        })
        .collect()
}

/// Per-tick pitch profile (rows × 6 ticks).
fn tick_pitch_profile(x: &[f32], from_row: usize, to_row: usize) -> Vec<f32> {
    let mut v = Vec::new();
    for row in from_row..to_row {
        for t in 0..6 {
            let a = (row * 6 + t) * 882;
            let b = a + 882;
            v.push(if b <= x.len() && rms(&x[a..b]) > 1e-4 {
                dominant_hz(&x[a..b], 100.0, 2500.0)
            } else {
                0.0
            });
        }
    }
    v
}

fn run_case_stereo(oracle: &Oracle, case: &Case) -> Option<(Vec<f32>, Vec<f32>)> {
    let theirs = oracle_render_stereo(oracle, case.name, &case.bytes)?;
    let (_, ours) = our_render(&case.bytes, case.rows * ROW_FRAMES + ROW_FRAMES);
    Some((ours, theirs))
}

fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

/// Dominant frequency (Hz) of a window by a plain DFT peak search over
/// `lo..hi` Hz with 1 Hz resolution (windows are short, so this is
/// cheap enough).
fn dominant_hz(x: &[f32], lo: f32, hi: f32) -> f32 {
    let n = x.len().min(4096);
    let x = &x[..n];
    let mut best = (0.0f32, lo);
    let mut f = lo;
    while f <= hi {
        let (mut re, mut im) = (0.0f32, 0.0f32);
        let w = 2.0 * std::f32::consts::PI * f / RATE as f32;
        for (i, &s) in x.iter().enumerate() {
            let a = w * i as f32;
            re += s * a.cos();
            im -= s * a.sin();
        }
        let mag = re * re + im * im;
        if mag > best.0 {
            best = (mag, f);
        }
        f += 1.0;
    }
    best.1
}

/// Refine a coarse dominant-frequency estimate by parabolic search in
/// 0.1 Hz steps around it.
fn dominant_hz_fine(x: &[f32], coarse: f32) -> f32 {
    let n = x.len().min(8192);
    let x = &x[..n];
    let mut best = (0.0f32, coarse);
    let mut f = coarse - 2.0;
    while f <= coarse + 2.0 {
        let (mut re, mut im) = (0.0f32, 0.0f32);
        let w = 2.0 * std::f32::consts::PI * f / RATE as f32;
        for (i, &s) in x.iter().enumerate() {
            let a = w * i as f32;
            re += s * a.cos();
            im -= s * a.sin();
        }
        let mag = re * re + im * im;
        if mag > best.0 {
            best = (mag, f);
        }
        f += 0.1;
    }
    best.1
}

/// Per-row RMS profile (normalised to the loudest row).
fn rms_profile(x: &[f32], rows: usize) -> Vec<f32> {
    let mut out: Vec<f32> = (0..rows)
        .map(|r| {
            let s = r * ROW_FRAMES;
            let e = (s + ROW_FRAMES).min(x.len());
            if s >= e {
                0.0
            } else {
                rms(&x[s..e])
            }
        })
        .collect();
    let peak = out.iter().cloned().fold(0.0f32, f32::max).max(1e-9);
    for v in out.iter_mut() {
        *v /= peak;
    }
    out
}

/// Per-row dominant pitch profile over the square wave's fundamental
/// range.
fn pitch_profile(x: &[f32], rows: usize) -> Vec<f32> {
    (0..rows)
        .map(|r| {
            let s = r * ROW_FRAMES;
            let e = (s + ROW_FRAMES).min(x.len());
            if s >= e || rms(&x[s..e]) < 1e-4 {
                0.0
            } else {
                let coarse = dominant_hz(&x[s..e], 100.0, 2500.0);
                dominant_hz_fine(&x[s..e], coarse)
            }
        })
        .collect()
}

fn report(name: &str, label: &str, ours: &[f32], theirs: &[f32]) {
    if verbose() {
        let o: Vec<String> = ours.iter().map(|v| format!("{v:7.3}")).collect();
        let t: Vec<String> = theirs.iter().map(|v| format!("{v:7.3}")).collect();
        eprintln!("[{name}] {label} ours:   {}", o.join(" "));
        eprintln!("[{name}] {label} oracle: {}", t.join(" "));
    }
}

/// Compare pitch profiles in cents; rows silent in either are skipped.
fn max_cents_diff(ours: &[f32], theirs: &[f32]) -> f32 {
    ours.iter()
        .zip(theirs)
        .filter(|(a, b)| **a > 0.0 && **b > 0.0)
        .map(|(a, b)| (1200.0 * (a / b).log2()).abs())
        .fold(0.0f32, f32::max)
}

/// Compare normalised RMS profiles; returns the maximum absolute
/// difference over rows where either is above the floor.
fn max_rms_diff(ours: &[f32], theirs: &[f32]) -> f32 {
    ours.iter()
        .zip(theirs)
        .filter(|(a, b)| **a > 0.02 || **b > 0.02)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max)
}

/// A square wave one octave below C-5 (period 32 frames at 8363 Hz →
/// ~261 Hz at C-5), long enough to give clean FFT peaks.
fn base_sample() -> ItWriterSample {
    square_sample(64, 16, 12000)
}

fn base_writer() -> ItWriter {
    ItWriter {
        mix_volume: 64,
        samples: vec![base_sample()],
        ..ItWriter::default()
    }
}

struct Case {
    name: &'static str,
    bytes: Vec<u8>,
    rows: usize,
}

fn run_case(oracle: &Oracle, case: &Case) -> Option<(Vec<f32>, Vec<f32>)> {
    let theirs = oracle_render(oracle, case.name, &case.bytes)?;
    let (ours, _) = our_render(&case.bytes, case.rows * ROW_FRAMES + ROW_FRAMES);
    Some((ours, theirs))
}

fn skip(name: &str) {
    eprintln!("SKIP {name}: no black-box IT oracle (openmpt123 / xmp) on PATH");
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Notes across the keyboard: pins the pitch table and the C5Speed
/// anchor.
fn case_scale() -> Case {
    let mut w = base_writer();
    let mut p = ItWriterPattern::new(16);
    for (i, note) in [48u8, 52, 55, 60, 64, 67, 72, 79].iter().enumerate() {
        p.note(i as u16 * 2, 0, *note, 1);
    }
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "scale",
        bytes: w.build(),
        rows: 16,
    }
}

/// Linear portamento up / down and tone portamento.
fn case_slides() -> Case {
    let mut w = base_writer();
    let mut p = ItWriterPattern::new(16);
    p.note(0, 0, 60, 1);
    for r in 1..4 {
        p.effect(r, 0, 'F', 0x08);
    }
    for r in 4..7 {
        p.effect(r, 0, 'E', 0x08);
    }
    p.put(8, 0, with_effect(cell_note(72, 0), 'G', 0x10));
    for r in 9..14 {
        p.effect(r, 0, 'G', 0x00);
    }
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "slides",
        bytes: w.build(),
        rows: 16,
    }
}

/// Volume slides + fine slides + set volume.
fn case_volume() -> Case {
    let mut w = base_writer();
    let mut p = ItWriterPattern::new(16);
    p.put(0, 0, with_volpan(cell_note(60, 1), 64));
    p.effect(1, 0, 'D', 0x04);
    p.effect(2, 0, 'D', 0x00);
    p.put(4, 0, with_volpan(cell_effect('D', 0x00), 16));
    p.effect(6, 0, 'D', 0x40);
    p.effect(7, 0, 'D', 0x40);
    p.effect(9, 0, 'D', 0xF8);
    p.effect(10, 0, 'D', 0x8F);
    p.put(12, 0, with_volpan(cell_effect('D', 0x00), 32));
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "volume",
        bytes: w.build(),
        rows: 16,
    }
}

/// Vibrato depth / rate (H) and fine vibrato (U).
fn case_vibrato() -> Case {
    let mut w = base_writer();
    let mut p = ItWriterPattern::new(16);
    p.put(0, 0, with_effect(cell_note(60, 1), 'H', 0x2F));
    for r in 1..6 {
        p.effect(r, 0, 'H', 0x00);
    }
    p.put(8, 0, with_effect(cell_note(60, 1), 'U', 0x2F));
    for r in 9..14 {
        p.effect(r, 0, 'U', 0x00);
    }
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "vibrato",
        bytes: w.build(),
        rows: 16,
    }
}

/// Tremolo (R) and tremor (I).
fn case_tremolo() -> Case {
    let mut w = base_writer();
    let mut p = ItWriterPattern::new(16);
    p.put(
        0,
        0,
        with_volpan(with_effect(cell_note(60, 1), 'R', 0x4F), 32),
    );
    for r in 1..6 {
        p.effect(r, 0, 'R', 0x00);
    }
    p.put(8, 0, with_effect(cell_note(60, 1), 'I', 0x22));
    for r in 9..14 {
        p.effect(r, 0, 'I', 0x00);
    }
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "tremolo",
        bytes: w.build(),
        rows: 16,
    }
}

/// Instrument mode: volume envelope + fadeout, note off.
fn case_envelope() -> Case {
    let mut w = base_writer();
    w.flags |= oxideav_mod::it::IT_FLAG_INSTRUMENTS;
    w.instruments.push(ItWriterInstrument {
        name: "env".into(),
        fadeout: 64,
        volume_envelope: ItWriterEnvelope {
            flags: IT_ENV_ON | IT_ENV_SUSTAIN_LOOP,
            nodes: vec![(64, 0), (16, 12), (16, 24), (0, 48)],
            sustain_begin: 2,
            sustain_end: 2,
            ..ItWriterEnvelope::default()
        },
        ..ItWriterInstrument::default()
    });
    let mut p = ItWriterPattern::new(16);
    p.note(0, 0, 60, 1);
    p.put(8, 0, cell_note(IT_NOTE_OFF, 0));
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "envelope",
        bytes: w.build(),
        rows: 16,
    }
}

/// Instrument mode: pitch envelope (units) and a looping panning
/// envelope.
fn case_pitch_env() -> Case {
    let mut w = base_writer();
    w.flags |= oxideav_mod::it::IT_FLAG_INSTRUMENTS;
    w.instruments.push(ItWriterInstrument {
        name: "pitch".into(),
        pitch_envelope: ItWriterEnvelope {
            flags: IT_ENV_ON | IT_ENV_LOOP,
            nodes: vec![
                (0, 0),
                (0, 5),
                (12, 6),
                (12, 11),
                (24, 12),
                (24, 17),
                (-12, 18),
                (-12, 23),
            ],
            loop_begin: 7,
            loop_end: 7,
            ..ItWriterEnvelope::default()
        },
        ..ItWriterInstrument::default()
    });
    let mut p = ItWriterPattern::new(8);
    p.note(0, 0, 60, 1);
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "pitch_env",
        bytes: w.build(),
        rows: 8,
    }
}

/// Tempo + speed changes: onset timing.
fn case_tempo() -> Case {
    let mut w = base_writer();
    let mut p = ItWriterPattern::new(8);
    p.note(0, 0, 60, 1);
    p.put(2, 0, with_effect(cell_note(60, 1), 'T', 0xFA));
    p.put(4, 0, with_effect(cell_note(60, 1), 'A', 0x03));
    p.put(6, 0, with_effect(cell_note(60, 1), 'T', 0x7D));
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "tempo",
        bytes: w.build(),
        rows: 8,
    }
}

// ---------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------

#[test]
fn oracle_scale_pitch_and_level() {
    let Some(o) = oracle() else {
        return skip("scale");
    };
    let case = case_scale();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("scale (render failed)");
    };
    let pa = pitch_profile(&ours, case.rows);
    let pb = pitch_profile(&theirs, case.rows);
    report(case.name, "hz ", &pa, &pb);
    assert!(
        max_cents_diff(&pa, &pb) < 15.0,
        "pitch table drift: {pa:?} vs {pb:?}"
    );
    let ra = rms_profile(&ours, case.rows);
    let rb = rms_profile(&theirs, case.rows);
    report(case.name, "rms", &ra, &rb);
    assert!(
        max_rms_diff(&ra, &rb) < 0.15,
        "level profile drift: {ra:?} vs {rb:?}"
    );
    // Absolute level: mix volume 64/128 × FV 128/128 on a ±12000 square
    // (8-bit stored → ±11776) → 0.18 RMS per side before panning.
    let (abs_a, abs_b) = (rms(&ours[..ROW_FRAMES]), rms(&theirs[..ROW_FRAMES]));
    if verbose() {
        eprintln!("[scale] absolute rms ours {abs_a:.4} oracle {abs_b:.4}");
    }
    assert!(
        (abs_a / abs_b - 1.0).abs() < 0.15,
        "absolute level {abs_a} vs {abs_b}"
    );
}

#[test]
fn oracle_slides() {
    let Some(o) = oracle() else {
        return skip("slides");
    };
    let case = case_slides();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("slides (render failed)");
    };
    let pa = pitch_profile(&ours, case.rows);
    let pb = pitch_profile(&theirs, case.rows);
    report(case.name, "hz ", &pa, &pb);
    assert!(
        max_cents_diff(&pa, &pb) < 25.0,
        "slide drift: {pa:?} vs {pb:?}"
    );
}

#[test]
fn oracle_volume_effects() {
    let Some(o) = oracle() else {
        return skip("volume");
    };
    let case = case_volume();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("volume (render failed)");
    };
    let ra = rms_profile(&ours, case.rows);
    let rb = rms_profile(&theirs, case.rows);
    report(case.name, "rms", &ra, &rb);
    assert!(
        max_rms_diff(&ra, &rb) < 0.12,
        "volume drift: {ra:?} vs {rb:?}"
    );
}

#[test]
fn oracle_vibrato_depth() {
    let Some(o) = oracle() else {
        return skip("vibrato");
    };
    let case = case_vibrato();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("vibrato (render failed)");
    };
    // Vibrato smears the spectral peak; compare the spread of the
    // instantaneous pitch instead: per-tick dominant frequency range.
    let spread = |x: &[f32], row: usize| -> (f32, f32) {
        let s = row * ROW_FRAMES;
        let mut lo = f32::MAX;
        let mut hi = 0.0f32;
        for t in 0..6 {
            let a = s + t * 882;
            let b = a + 882;
            if b > x.len() {
                break;
            }
            let f = dominant_hz(&x[a..b], 100.0, 2500.0);
            lo = lo.min(f);
            hi = hi.max(f);
        }
        (lo, hi)
    };
    let ta = tick_pitch_profile(&ours, 0, 2);
    let tb = tick_pitch_profile(&theirs, 0, 2);
    report(case.name, "tick", &ta, &tb);
    let (olo, ohi) = spread(&ours, 3);
    let (tlo, thi) = spread(&theirs, 3);
    if verbose() {
        eprintln!("[vibrato] H2F row3 ours {olo}..{ohi} oracle {tlo}..{thi}");
    }
    let (ulo, uhi) = spread(&ours, 11);
    let (vlo, vhi) = spread(&theirs, 11);
    if verbose() {
        eprintln!("[vibrato] U2F row11 ours {ulo}..{uhi} oracle {vlo}..{vhi}");
    }
    let cents = |a: f32, b: f32| (1200.0 * (a / b).log2()).abs();
    assert!(
        cents(ohi, thi) < 30.0 && cents(olo, tlo) < 30.0,
        "H depth drift"
    );
    assert!(
        cents(uhi, vhi) < 30.0 && cents(ulo, vlo) < 30.0,
        "U depth drift"
    );
}

#[test]
fn oracle_tremolo_and_tremor() {
    let Some(o) = oracle() else {
        return skip("tremolo");
    };
    let case = case_tremolo();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("tremolo (render failed)");
    };
    // Per-tick RMS over rows 0..6 (tremolo) and 8..14 (tremor).
    let ticks = |x: &[f32], from: usize, to: usize| -> Vec<f32> {
        let mut v = Vec::new();
        for row in from..to {
            for t in 0..6 {
                let a = (row * 6 + t) * 882;
                let b = a + 882;
                v.push(if b <= x.len() { rms(&x[a..b]) } else { 0.0 });
            }
        }
        let peak = v.iter().cloned().fold(0.0f32, f32::max).max(1e-9);
        v.iter().map(|r| r / peak).collect()
    };
    let ta = ticks(&ours, 0, 6);
    let tb = ticks(&theirs, 0, 6);
    report(case.name, "trem", &ta, &tb);
    let ia = ticks(&ours, 8, 14);
    let ib = ticks(&theirs, 8, 14);
    report(case.name, "tremor", &ia, &ib);
    assert!(max_rms_diff(&ta, &tb) < 0.25, "tremolo drift");
    assert!(max_rms_diff(&ia, &ib) < 0.25, "tremor drift");
}

#[test]
fn oracle_envelope_and_fadeout() {
    let Some(o) = oracle() else {
        return skip("envelope");
    };
    let case = case_envelope();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("envelope (render failed)");
    };
    let ra = rms_profile(&ours, case.rows);
    let rb = rms_profile(&theirs, case.rows);
    report(case.name, "rms", &ra, &rb);
    assert!(
        max_rms_diff(&ra, &rb) < 0.12,
        "envelope drift: {ra:?} vs {rb:?}"
    );
}

#[test]
fn oracle_pitch_envelope_units() {
    let Some(o) = oracle() else {
        return skip("pitch_env");
    };
    let case = case_pitch_env();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("pitch_env (render failed)");
    };
    let pa = pitch_profile(&ours, case.rows);
    let pb = pitch_profile(&theirs, case.rows);
    report(case.name, "hz ", &pa, &pb);
    assert!(
        max_cents_diff(&pa, &pb) < 25.0,
        "pitch envelope units: {pa:?} vs {pb:?}"
    );
}

#[test]
fn oracle_tempo_timing() {
    let Some(o) = oracle() else {
        return skip("tempo");
    };
    let case = case_tempo();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("tempo (render failed)");
    };
    // Note onsets: first frame of each note where |x| exceeds a
    // threshold after a silent gap is not available (the square loops),
    // so compare the total rendered length and the per-tick RMS
    // envelope alignment instead: rows 0-1 at 125, rows 2-3 at 250 BPM
    // (half length), rows 4-5 at speed 3 (half again), rows 6-7 at
    // 125 / speed 3.
    let expected = 2 * 6 * 882 + 2 * 6 * 441 + 2 * 3 * 441 + 2 * 3 * 882;
    if verbose() {
        eprintln!(
            "[tempo] frames ours {} oracle {} expected {expected}",
            ours.len(),
            theirs.len()
        );
    }
    let ours_len = ours.len() as i64;
    let theirs_len = theirs.len() as i64;
    assert!(
        (ours_len - expected as i64).abs() < 100,
        "our length {ours_len} vs {expected}"
    );
    // The oracle may append a tail (declick / buffer flush); require it
    // to be at least as long and within ~0.25 s.
    assert!(
        theirs_len >= expected as i64 - 100,
        "oracle length {theirs_len} vs {expected}"
    );
    assert!(
        theirs_len < expected as i64 + RATE as i64 / 4,
        "oracle length {theirs_len} vs {expected}"
    );
}

// ---------------------------------------------------------------------------
// Batch 2 — panning, tempo slides, retrig / arpeggio, channel + global
// volume, NNA, sample vibrato, offsets, sample-mode note off, S2x.
// ---------------------------------------------------------------------------

/// Panning commands: Xxx, S8x, Pxy, volume-column pan.
fn case_pan() -> Case {
    let mut w = base_writer();
    let mut p = ItWriterPattern::new(16);
    p.put(0, 0, with_effect(cell_note(60, 1), 'X', 0x00));
    p.put(2, 0, with_effect(cell_note(60, 1), 'X', 0x80));
    p.put(4, 0, with_effect(cell_note(60, 1), 'X', 0xFF));
    p.put(6, 0, with_effect(cell_note(60, 1), 'S', 0x84));
    p.put(8, 0, with_effect(cell_note(60, 1), 'S', 0x8C));
    p.put(10, 0, with_volpan(cell_note(60, 1), 128 + 48));
    p.put(12, 0, with_effect(cell_note(60, 1), 'X', 0x80));
    p.effect(13, 0, 'P', 0x40);
    p.effect(14, 0, 'P', 0x00);
    p.effect(15, 0, 'P', 0x08);
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "pan",
        bytes: w.build(),
        rows: 16,
    }
}

/// Panning envelope model: channel pan 32 / 0 with a +32 / -32 envelope.
fn case_pan_env() -> Case {
    let mut w = base_writer();
    w.flags |= oxideav_mod::it::IT_FLAG_INSTRUMENTS;
    w.instruments.push(ItWriterInstrument {
        name: "penv".into(),
        panning_envelope: ItWriterEnvelope {
            flags: IT_ENV_ON | IT_ENV_LOOP,
            nodes: vec![(32, 0), (32, 11), (-32, 12), (-32, 23), (16, 24), (16, 35)],
            loop_begin: 5,
            loop_end: 5,
            ..ItWriterEnvelope::default()
        },
        ..ItWriterInstrument::default()
    });
    let mut p = ItWriterPattern::new(16);
    p.put(0, 0, with_effect(cell_note(60, 1), 'X', 0x80));
    p.put(8, 0, with_effect(cell_note(60, 1), 'X', 0x00));
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "pan_env",
        bytes: w.build(),
        rows: 16,
    }
}

/// Tempo slides T0x / T1x.
fn case_tempo_slide() -> Case {
    let mut w = base_writer();
    let mut p = ItWriterPattern::new(8);
    p.put(0, 0, with_effect(cell_note(60, 1), 'T', 0x05));
    p.effect(1, 0, 'T', 0x05);
    p.effect(2, 0, 'T', 0x15);
    p.effect(3, 0, 'T', 0x15);
    p.effect(4, 0, 'T', 0x00);
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "tempo_slide",
        bytes: w.build(),
        rows: 8,
    }
}

/// Retrig Qxy with volume change, arpeggio Jxy.
fn case_retrig_arp() -> Case {
    let mut w = base_writer();
    let mut p = ItWriterPattern::new(8);
    p.put(0, 0, with_effect(cell_note(60, 1), 'Q', 0x32));
    p.effect(1, 0, 'Q', 0x00);
    p.put(2, 0, with_effect(cell_note(60, 1), 'Q', 0xB3));
    p.effect(3, 0, 'Q', 0x00);
    p.put(4, 0, with_effect(cell_note(60, 1), 'J', 0x37));
    p.effect(5, 0, 'J', 0x00);
    p.effect(6, 0, 'J', 0xC0);
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "retrig_arp",
        bytes: w.build(),
        rows: 8,
    }
}

/// Channel volume Mxx / Nxy and global volume Vxx / Wxy.
fn case_chan_global() -> Case {
    let mut w = base_writer();
    let mut p = ItWriterPattern::new(16);
    p.put(0, 0, with_effect(cell_note(60, 1), 'M', 0x20));
    p.effect(2, 0, 'N', 0x04);
    p.effect(3, 0, 'N', 0x00);
    p.effect(5, 0, 'N', 0x0F);
    p.effect(6, 0, 'N', 0x8F);
    p.effect(8, 0, 'V', 0x40);
    p.effect(10, 0, 'W', 0x08);
    p.effect(11, 0, 'W', 0x00);
    p.effect(13, 0, 'W', 0xF4);
    p.effect(14, 0, 'W', 0x40);
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "chan_global",
        bytes: w.build(),
        rows: 16,
    }
}

/// NNA: continue / note off / fade, with a sustain-looped volume
/// envelope so note-off is audible, and DCT note + DCA cut.
fn case_nna() -> Case {
    let mut w = base_writer();
    w.flags |= oxideav_mod::it::IT_FLAG_INSTRUMENTS;
    let env = ItWriterEnvelope {
        flags: IT_ENV_ON | IT_ENV_SUSTAIN_LOOP,
        nodes: vec![(64, 0), (64, 6), (0, 18)],
        sustain_begin: 1,
        sustain_end: 1,
        ..ItWriterEnvelope::default()
    };
    for (i, nna) in [1u8, 2, 3].iter().enumerate() {
        w.instruments.push(ItWriterInstrument {
            name: format!("nna{i}"),
            nna: *nna,
            fadeout: 128,
            volume_envelope: env.clone(),
            ..ItWriterInstrument::default()
        });
    }
    w.instruments.push(ItWriterInstrument {
        name: "dct".into(),
        nna: 1,
        dct: 1,
        dca: 0,
        volume_envelope: env.clone(),
        ..ItWriterInstrument::default()
    });
    let mut p = ItWriterPattern::new(24);
    // ch0: continue → two voices sound; ch1: note off → old fades via
    // envelope; ch2: fade → old fades by fadeout; ch3: DCT.
    for ch in 0..3u8 {
        p.note(0, ch, 60, ch + 1);
        p.note(4, ch, 67, ch + 1);
    }
    p.note(12, 3, 60, 4);
    p.note(14, 3, 60, 4);
    p.note(16, 3, 64, 4);
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "nna",
        bytes: w.build(),
        rows: 24,
    }
}

/// Sample vibrato: speed 32, depth 32, rate 16 (sweep).
fn case_sample_vibrato() -> Case {
    let mut w = base_writer();
    // Speed 8 (one table cycle per 32 ticks), full depth, and a fast
    // sweep (rate 255 ≈ +1 depth per tick) so the swing is measurable
    // within the fixture.
    w.samples[0].vibrato_speed = 8;
    w.samples[0].vibrato_depth = 64;
    w.samples[0].vibrato_rate = 255;
    let mut p = ItWriterPattern::new(8);
    p.note(0, 0, 60, 1);
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "sample_vibrato",
        bytes: w.build(),
        rows: 8,
    }
}

/// Sample offset Oxx + SAy over a sample whose first 512 frames are
/// silent, and the volume-column pitch slides.
fn case_offset() -> Case {
    let mut w = base_writer();
    let mut pcm = vec![0i16; 512];
    pcm.extend((0..2048).map(|i| if (i / 16) % 2 == 0 { 12000 } else { -12000 }));
    w.samples[0] = ItWriterSample {
        name: "gap".into(),
        pcm,
        ..ItWriterSample::default()
    };
    let mut p = ItWriterPattern::new(8);
    p.note(0, 0, 60, 1);
    p.put(2, 0, with_effect(cell_note(60, 1), 'O', 0x02));
    p.put(4, 0, with_effect(cell_note(60, 1), 'O', 0x00));
    p.put(6, 0, with_effect(cell_note(60, 1), 'O', 0x09));
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "offset",
        bytes: w.build(),
        rows: 8,
    }
}

/// Sample-mode note off: with a sustain loop (released into the normal
/// loop) and without one (cut).
fn case_sample_noteoff() -> Case {
    let mut w = base_writer();
    let mut sus = base_sample();
    sus.flags |= oxideav_mod::it::IT_SMP_SUSTAIN_LOOP;
    sus.sustain_begin = 0;
    sus.sustain_end = 32;
    w.samples.push(sus);
    let mut p = ItWriterPattern::new(8);
    p.note(0, 0, 60, 1);
    p.put(2, 0, cell_note(IT_NOTE_OFF, 0));
    p.note(4, 1, 60, 2);
    p.put(6, 1, cell_note(IT_NOTE_OFF, 0));
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "sample_noteoff",
        bytes: w.build(),
        rows: 8,
    }
}

/// S2x finetune, volume-column pitch slides, SDx / SCx, Kxy / Lxy.
fn case_misc() -> Case {
    let mut w = base_writer();
    let mut p = ItWriterPattern::new(20);
    p.put(0, 0, with_effect(cell_note(60, 1), 'S', 0x20));
    p.put(2, 0, with_effect(cell_note(60, 1), 'S', 0x2F));
    p.put(4, 0, with_volpan(cell_note(60, 1), 115 + 2)); // pitch up 2
    p.put(5, 0, with_volpan(ItCell::default(), 115 + 2));
    p.put(6, 0, with_volpan(ItCell::default(), 105 + 2)); // down
    p.put(8, 0, with_effect(cell_note(60, 1), 'H', 0x4F));
    p.effect(9, 0, 'K', 0x04);
    p.put(10, 0, with_effect(cell_note(72, 0), 'G', 0x08));
    p.effect(11, 0, 'L', 0x04);
    p.effect(12, 0, 'L', 0x00);
    // S00 memory probe: SC1 cuts at tick 1; does S00 repeat it?
    p.put(14, 0, with_effect(cell_note(60, 1), 'S', 0xC1));
    p.put(16, 0, with_effect(cell_note(60, 1), 'S', 0x00));
    p.note(18, 0, 60, 1);
    w.patterns.push(p);
    w.orders = vec![0, 255];
    Case {
        name: "misc",
        bytes: w.build(),
        rows: 20,
    }
}

#[test]
fn oracle_pan_commands() {
    let Some(o) = oracle() else {
        return skip("pan");
    };
    let case = case_pan();
    let Some((ours, theirs)) = run_case_stereo(&o, &case) else {
        return skip("pan (render failed)");
    };
    let ba = balance_profile(&ours, case.rows);
    let bb = balance_profile(&theirs, case.rows);
    report(case.name, "bal", &ba, &bb);
    let d = ba
        .iter()
        .zip(&bb)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(d < 0.08, "pan drift: {ba:?} vs {bb:?}");
}

#[test]
fn oracle_pan_envelope_model() {
    let Some(o) = oracle() else {
        return skip("pan_env");
    };
    let case = case_pan_env();
    let Some((ours, theirs)) = run_case_stereo(&o, &case) else {
        return skip("pan_env (render failed)");
    };
    let ba = balance_profile(&ours, case.rows);
    let bb = balance_profile(&theirs, case.rows);
    report(case.name, "bal", &ba, &bb);
    // The envelope steps at ticks 0 / 12 / 24 of each note (rows 0, 2,
    // 4 and 8, 10, 12); the oracle's mixer ramps pan across the tick of
    // each step, so only the steady rows are compared.
    let steady = [1usize, 3, 5, 6, 7, 9, 11, 13, 14, 15];
    let d = steady
        .iter()
        .map(|&r| (ba[r] - bb[r]).abs())
        .fold(0.0f32, f32::max);
    assert!(d < 0.08, "pan envelope drift: {ba:?} vs {bb:?}");
}

#[test]
fn oracle_tempo_slides() {
    let Some(o) = oracle() else {
        return skip("tempo_slide");
    };
    let case = case_tempo_slide();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("tempo_slide (render failed)");
    };
    if verbose() {
        eprintln!(
            "[tempo_slide] frames ours {} oracle {}",
            ours.len(),
            theirs.len()
        );
    }
    let (a, b) = (ours.len() as i64, theirs.len() as i64);
    assert!(
        (a - b).abs() < RATE as i64 / 4,
        "tempo slide length {a} vs {b}"
    );
}

#[test]
fn oracle_retrig_and_arpeggio() {
    let Some(o) = oracle() else {
        return skip("retrig_arp");
    };
    let case = case_retrig_arp();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("retrig_arp (render failed)");
    };
    let ta = tick_pitch_profile(&ours, 4, 7);
    let tb = tick_pitch_profile(&theirs, 4, 7);
    report(case.name, "arp", &ta, &tb);
    assert!(max_cents_diff(&ta, &tb) < 30.0, "arpeggio drift");
    let ra = rms_profile(&ours, case.rows);
    let rb = rms_profile(&theirs, case.rows);
    report(case.name, "rms", &ra, &rb);
    assert!(max_rms_diff(&ra, &rb) < 0.15, "retrig volume drift");
}

#[test]
fn oracle_channel_and_global_volume() {
    let Some(o) = oracle() else {
        return skip("chan_global");
    };
    let case = case_chan_global();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("chan_global (render failed)");
    };
    let ra = rms_profile(&ours, case.rows);
    let rb = rms_profile(&theirs, case.rows);
    report(case.name, "rms", &ra, &rb);
    assert!(
        max_rms_diff(&ra, &rb) < 0.12,
        "channel/global volume drift: {ra:?} vs {rb:?}"
    );
}

#[test]
fn oracle_nna_and_dct() {
    let Some(o) = oracle() else {
        return skip("nna");
    };
    let case = case_nna();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("nna (render failed)");
    };
    let ra = rms_profile(&ours, case.rows);
    let rb = rms_profile(&theirs, case.rows);
    report(case.name, "rms", &ra, &rb);
    assert!(max_rms_diff(&ra, &rb) < 0.15, "NNA drift: {ra:?} vs {rb:?}");
}

#[test]
fn oracle_sample_vibrato() {
    let Some(o) = oracle() else {
        return skip("sample_vibrato");
    };
    let case = case_sample_vibrato();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("sample_vibrato (render failed)");
    };
    let ta = tick_pitch_profile(&ours, 0, 8);
    let tb = tick_pitch_profile(&theirs, 0, 8);
    report(case.name, "hz", &ta, &tb);
    assert!(max_cents_diff(&ta, &tb) < 40.0, "sample vibrato drift");
}

#[test]
fn oracle_sample_offset() {
    let Some(o) = oracle() else {
        return skip("offset");
    };
    let case = case_offset();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("offset (render failed)");
    };
    let ra = rms_profile(&ours, case.rows);
    let rb = rms_profile(&theirs, case.rows);
    report(case.name, "rms", &ra, &rb);
    assert!(
        max_rms_diff(&ra, &rb) < 0.15,
        "offset drift: {ra:?} vs {rb:?}"
    );
}

#[test]
fn oracle_sample_mode_note_off() {
    let Some(o) = oracle() else {
        return skip("sample_noteoff");
    };
    let case = case_sample_noteoff();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("sample_noteoff (render failed)");
    };
    let ra = rms_profile(&ours, case.rows);
    let rb = rms_profile(&theirs, case.rows);
    report(case.name, "rms", &ra, &rb);
    assert!(
        max_rms_diff(&ra, &rb) < 0.15,
        "sample-mode note off drift: {ra:?} vs {rb:?}"
    );
}

#[test]
fn oracle_misc_effects() {
    let Some(o) = oracle() else {
        return skip("misc");
    };
    let case = case_misc();
    let Some((ours, theirs)) = run_case(&o, &case) else {
        return skip("misc (render failed)");
    };
    let pa = pitch_profile(&ours, case.rows);
    let pb = pitch_profile(&theirs, case.rows);
    report(case.name, "hz ", &pa, &pb);
    assert!(
        max_cents_diff(&pa, &pb) < 25.0,
        "misc pitch drift: {pa:?} vs {pb:?}"
    );
    let ra = rms_profile(&ours, case.rows);
    let rb = rms_profile(&theirs, case.rows);
    report(case.name, "rms", &ra, &rb);
    assert!(
        max_rms_diff(&ra, &rb) < 0.15,
        "misc rms drift: {ra:?} vs {rb:?}"
    );
}
