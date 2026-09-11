//! FastTracker 2 black-box render comparison.
//!
//! Synthetic `.xm` fixtures (built with the crate's own writer) are
//! rendered by this crate and by an installed command-line player —
//! `openmpt123 --render` or `xmp -o` — invoked strictly as opaque
//! binaries. Only their PCM output is consumed; no player source is
//! read or referenced. When neither binary is on `PATH` every test
//! prints a SKIP line and passes, so CI (which has no oracle) stays
//! green while a developer machine gets the real gate.
//!
//! What is compared, per fixture:
//!
//! - **Pitch**: the dominant frequency of each analysis window (DFT
//!   peak of the mono mix), which pins the period tables, slide maths,
//!   vibrato / autovibrato depth and rate independently of gain.
//! - **Envelope**: the RMS of each window, normalised by the loudest
//!   window, which pins volume slides, envelopes, fadeout, tremolo,
//!   tremor and the global volume independently of absolute gain.
//! - **Balance**: the right-channel share of each window, which pins
//!   panning commands and the panning envelope.
//! - **Flow**: rows carry distinct notes, so the pitch profile doubles
//!   as an order / row trace for the jump, break, loop and delay
//!   effects.
//!
//! Every threshold below is a conformance *floor*; the verbose mode
//! (`OXIDEAV_XM_ORACLE_VERBOSE=1`) prints the per-window numbers.

// The fixture battery grows one oracle-settled unit at a time; analysis
// helpers land ahead of their first fixture.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_mod::xm::{extract_sample_bodies, parse_header, parse_instruments, parse_patterns};
use oxideav_mod::xm_player::XmPlayerState;
use oxideav_mod::xm_writer::{
    cell_effect, cell_note, single_sample_instrument, square_sample, with_effect, with_volume,
    XmWriter, XmWriterEnvelope, XmWriterInstrument, XmWriterPattern, XmWriterSample, XM_ENV_LOOP,
    XM_ENV_ON, XM_ENV_SUSTAIN,
};

const RATE: u32 = 44_100;
/// Analysis window: one row at speed 6 / BPM 125 = 6 × 882 frames.
const TICK_FRAMES: usize = 882;
const ROW_FRAMES: usize = 6 * TICK_FRAMES;
/// RMS floor below which a window counts as silent (the oracle's
/// release ramps leave a faint tail under this).
const SILENCE: f32 = 0.004;

/// XM note numbers (1 = C-0).
const C4: u8 = 49;
const E4: u8 = 53;
const G4: u8 = 56;
const C5: u8 = 61;
const C3: u8 = 37;
const KEY_OFF: u8 = 97;

/// Effect bytes.
const FX_ARP: u8 = 0x00;
const FX_PORTA_UP: u8 = 0x01;
const FX_PORTA_DOWN: u8 = 0x02;
const FX_TONE_PORTA: u8 = 0x03;
const FX_VIBRATO: u8 = 0x04;
const FX_TREMOLO: u8 = 0x07;
const FX_PAN: u8 = 0x08;
const FX_VOL_SLIDE: u8 = 0x0A;
const FX_VOLUME: u8 = 0x0C;
const FX_E: u8 = 0x0E;
const FX_SPEED: u8 = 0x0F;
const FX_GLOBAL_VOL: u8 = 0x10;
const FX_GLOBAL_SLIDE: u8 = 0x11;
const FX_KEY_OFF: u8 = 0x14;
const FX_ENV_POS: u8 = 0x15;
const FX_PAN_SLIDE: u8 = 0x19;
const FX_MULTI_RETRIG: u8 = 0x1B;
const FX_TREMOR: u8 = 0x1D;
const FX_X: u8 = 0x21;

fn verbose() -> bool {
    std::env::var_os("OXIDEAV_XM_ORACLE_VERBOSE").is_some()
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
        .join("xm-oracle");
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

/// Render through the oracle; returns interleaved stereo f32.
fn oracle_render_stereo(oracle: &Oracle, name: &str, xm_bytes: &[u8]) -> Option<Vec<f32>> {
    let dir = fixture_dir();
    let xm_path = dir.join(format!("{name}.xm"));
    std::fs::write(&xm_path, xm_bytes).ok()?;
    let wav_path = match oracle {
        Oracle::OpenMpt(bin) => {
            let out = dir.join(format!("{name}.xm.wav"));
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
                .arg(&xm_path)
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
                .arg(&xm_path)
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
    let mut fmt: Option<(u16, u16, u16)> = None;
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
            match (format, bits) {
                (1, 16) => {
                    for frame in body.chunks_exact(2 * ch) {
                        let v = |c: usize| {
                            i16::from_le_bytes([frame[2 * c], frame[2 * c + 1]]) as f32 / 32768.0
                        };
                        out.push(v(0));
                        out.push(v(if ch > 1 { 1 } else { 0 }));
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
                        out.push(v(0));
                        out.push(v(if ch > 1 { 1 } else { 0 }));
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

/// Render through this crate; returns interleaved stereo f32.
fn our_render_stereo(xm_bytes: &[u8], max_frames: usize) -> Vec<f32> {
    let header = parse_header(xm_bytes).expect("fixture header parses");
    let (patterns, off) = parse_patterns(&header, xm_bytes).expect("fixture patterns parse");
    let mut instruments =
        parse_instruments(&header, xm_bytes, off).expect("fixture instruments parse");
    extract_sample_bodies(&mut instruments, xm_bytes);
    let mut p = XmPlayerState::new(&header, instruments, patterns, RATE);
    let mut stereo = Vec::new();
    let mut buf = vec![0i16; 4096];
    while stereo.len() / 2 < max_frames {
        let n = p.render(&mut buf);
        if n == 0 {
            break;
        }
        stereo.extend_from_slice(&buf[..n * 2]);
    }
    stereo.iter().map(|&v| v as f32 / 32768.0).collect()
}

fn mono(st: &[f32]) -> Vec<f32> {
    st.chunks_exact(2).map(|f| (f[0] + f[1]) / 2.0).collect()
}

fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

/// Dominant frequency (Hz) of a window by a plain DFT peak search over
/// `lo..hi` Hz with 1 Hz resolution.
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

/// Refine a coarse dominant-frequency estimate in 0.1 Hz steps.
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

/// Per-tick RMS profile (rows × 6 ticks), normalised to the loudest
/// tick.
fn tick_rms_profile(x: &[f32], rows: usize) -> Vec<f32> {
    let mut out: Vec<f32> = (0..rows * 6)
        .map(|t| {
            let s = t * TICK_FRAMES;
            let e = (s + TICK_FRAMES).min(x.len());
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
/// range (silent rows report 0).
fn pitch_profile(x: &[f32], rows: usize) -> Vec<f32> {
    (0..rows)
        .map(|r| {
            let s = r * ROW_FRAMES;
            let e = (s + ROW_FRAMES).min(x.len());
            if s >= e || rms(&x[s..e]) < SILENCE {
                0.0
            } else {
                let coarse = dominant_hz(&x[s..e], 100.0, 2500.0);
                dominant_hz_fine(&x[s..e], coarse)
            }
        })
        .collect()
}

/// Per-tick pitch profile (rows × 6 ticks).
fn tick_pitch_profile(x: &[f32], from_row: usize, to_row: usize) -> Vec<f32> {
    let mut v = Vec::new();
    for row in from_row..to_row {
        for t in 0..6 {
            let a = (row * 6 + t) * TICK_FRAMES;
            let b = a + TICK_FRAMES;
            v.push(if b <= x.len() && rms(&x[a..b]) > SILENCE {
                dominant_hz(&x[a..b], 100.0, 2500.0)
            } else {
                0.0
            });
        }
    }
    v
}

/// Per-row right-channel share `R / (L + R)` of the RMS (0 = hard
/// left, 0.5 = centre, 1 = hard right); silent rows report 0.5.
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

/// Rows where exactly one side is silent (a pitch-profile 0).
fn silence_mismatches(ours: &[f32], theirs: &[f32]) -> usize {
    ours.iter()
        .zip(theirs)
        .filter(|(a, b)| (**a > 0.0) != (**b > 0.0))
        .count()
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

fn max_abs_diff(ours: &[f32], theirs: &[f32]) -> f32 {
    ours.iter()
        .zip(theirs)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max)
}

/// A square wave one octave below C-4 (period 32 frames at 8363 Hz →
/// ~261 Hz at C-4), long enough to give clean DFT peaks.
/// An empty cell (volume-column-only rows build on it).
fn empty() -> oxideav_mod::xm::XmCell {
    oxideav_mod::xm::XmCell::default()
}

/// A square wave one octave below C-4 (period 32 frames at 8363 Hz →
/// ~261 Hz at C-4), long enough to give clean DFT peaks.
fn base_sample() -> XmWriterSample {
    square_sample(64, 16, 12000)
}

fn base_instrument() -> XmWriterInstrument {
    single_sample_instrument(base_sample())
}

fn base_writer() -> XmWriter {
    XmWriter {
        instruments: vec![base_instrument()],
        ..XmWriter::default()
    }
}

/// A silent 64-row pattern appended as the song's terminal order so a
/// fixture never revisits a position (the oracle stops rendering at
/// the first song loop).
fn silent_pattern() -> XmWriterPattern {
    XmWriterPattern::new(64)
}

struct Case {
    name: &'static str,
    bytes: Vec<u8>,
    rows: usize,
}

/// One pattern followed by the silent terminal pattern.
fn one_pattern(name: &'static str, w: XmWriter, p: XmWriterPattern) -> Case {
    let rows = p.num_rows as usize;
    let mut w = w;
    w.patterns = vec![p, silent_pattern()];
    w.orders = vec![0, 1];
    w.restart_position = 1;
    Case {
        name,
        bytes: w.build(),
        rows,
    }
}

/// Several patterns in order, then the silent terminal pattern.
fn patterns(name: &'static str, w: XmWriter, ps: Vec<XmWriterPattern>, rows: usize) -> Case {
    let mut w = w;
    let n = ps.len() as u8;
    w.patterns = ps;
    w.patterns.push(silent_pattern());
    w.orders = (0..=n).collect();
    w.restart_position = n as u16;
    Case {
        name,
        bytes: w.build(),
        rows,
    }
}

/// Both renders of one fixture.
struct Run {
    case: Case,
    ours: Vec<f32>,
    theirs: Vec<f32>,
    ours_mono: Vec<f32>,
    theirs_mono: Vec<f32>,
    failures: Vec<String>,
}

impl Run {
    fn new(oracle: &Oracle, case: Case) -> Option<Run> {
        let theirs = oracle_render_stereo(oracle, case.name, &case.bytes)?;
        let ours = our_render_stereo(&case.bytes, case.rows * ROW_FRAMES + ROW_FRAMES);
        let ours_mono = mono(&ours);
        let theirs_mono = mono(&theirs);
        Some(Run {
            case,
            ours,
            theirs,
            ours_mono,
            theirs_mono,
            failures: Vec::new(),
        })
    }

    fn rows(&self) -> usize {
        self.case.rows
    }

    /// Per-row pitch: silent-row agreement + cents.
    fn pitch(&mut self, cents: f32) -> &mut Self {
        let pa = pitch_profile(&self.ours_mono, self.rows());
        let pb = pitch_profile(&self.theirs_mono, self.rows());
        report(self.case.name, "hz ", &pa, &pb);
        let sm = silence_mismatches(&pa, &pb);
        if sm != 0 {
            self.failures
                .push(format!("{sm} audible-row mismatches: {pa:?} vs {pb:?}"));
        }
        let d = max_cents_diff(&pa, &pb);
        if d >= cents {
            self.failures
                .push(format!("row pitch drift {d:.1} cents: {pa:?} vs {pb:?}"));
        }
        self
    }

    /// Per-tick pitch in cents (no silence agreement check).
    fn tick_pitch(&mut self, cents: f32) -> &mut Self {
        let pa = tick_pitch_profile(&self.ours_mono, 0, self.rows());
        let pb = tick_pitch_profile(&self.theirs_mono, 0, self.rows());
        report(self.case.name, "thz", &pa, &pb);
        let d = max_cents_diff(&pa, &pb);
        if d >= cents {
            self.failures
                .push(format!("tick pitch drift {d:.1} cents: {pa:?} vs {pb:?}"));
        }
        self
    }

    /// Per-tick pitch trace where each tick must agree on the sounding
    /// note, allowing one tick of slop at every transition.
    fn tick_trace(&mut self) -> &mut Self {
        let pa = tick_pitch_profile(&self.ours_mono, 0, self.rows());
        let pb = tick_pitch_profile(&self.theirs_mono, 0, self.rows());
        report(self.case.name, "thz", &pa, &pb);
        let same = |a: f32, b: f32| {
            (a > 0.0 && b > 0.0 && (1200.0 * (a / b).log2()).abs() < 30.0) || (a == 0.0 && b == 0.0)
        };
        let mut bad = Vec::new();
        for i in 0..pa.len() {
            if !same(pa[i], pb[i]) {
                let prev = i > 0 && same(pa[i - 1], pb[i]);
                let next = i + 1 < pb.len() && same(pa[i], pb[i + 1]);
                if !prev && !next {
                    bad.push(i);
                }
            }
        }
        if !bad.is_empty() {
            self.failures.push(format!(
                "tick trace mismatch at ticks {bad:?}: {pa:?} vs {pb:?}"
            ));
        }
        self
    }

    fn rms(&mut self, tol: f32) -> &mut Self {
        let ra = rms_profile(&self.ours_mono, self.rows());
        let rb = rms_profile(&self.theirs_mono, self.rows());
        report(self.case.name, "rms", &ra, &rb);
        let d = max_rms_diff(&ra, &rb);
        if d >= tol {
            self.failures
                .push(format!("row level drift {d:.3}: {ra:?} vs {rb:?}"));
        }
        self
    }

    /// Per-tick level. The oracle ramps volume changes: a slide or
    /// envelope step is spread across the whole tick (its window reads
    /// as the mean of the previous and the current tick's level) while
    /// a cut decays over about a sixteenth of a tick (≈ 0.125 × the
    /// previous level). Each tick is accepted if it matches ours raw
    /// or under either ramp model.
    fn tick_rms(&mut self, tol: f32) -> &mut Self {
        let ra = tick_rms_profile(&self.ours_mono, self.rows());
        let rb = tick_rms_profile(&self.theirs_mono, self.rows());
        report(self.case.name, "trms", &ra, &rb);
        let prev = |t: usize| if t == 0 { ra[0] } else { ra[t - 1] };
        let d = (0..ra.len())
            .filter(|&t| ra[t] > 0.02 || prev(t) > 0.02 || rb[t] > 0.02)
            .map(|t| {
                let (a, b) = (ra[t], rb[t]);
                let mean = (prev(t) + a) / 2.0;
                let cut = a.max(0.125 * prev(t));
                (a - b).abs().min((mean - b).abs()).min((cut - b).abs())
            })
            .fold(0.0f32, f32::max);
        if d >= tol {
            self.failures
                .push(format!("tick level drift {d:.3}: {ra:?} vs {rb:?}"));
        }
        self
    }

    fn balance(&mut self, tol: f32) -> &mut Self {
        let ba = balance_profile(&self.ours, self.rows());
        let bb = balance_profile(&self.theirs, self.rows());
        report(self.case.name, "bal", &ba, &bb);
        let d = max_abs_diff(&ba, &bb);
        if d >= tol {
            self.failures
                .push(format!("balance drift {d:.3}: {ba:?} vs {bb:?}"));
        }
        self
    }

    /// Absolute level of the first row (mix gain).
    fn absolute(&mut self, tol: f32) -> &mut Self {
        let (a, b) = (
            rms(&self.ours_mono[..ROW_FRAMES]),
            rms(&self.theirs_mono[..ROW_FRAMES]),
        );
        if verbose() {
            eprintln!(
                "[{}] absolute rms ours {a:.4} oracle {b:.4}",
                self.case.name
            );
        }
        if (a / b - 1.0).abs() >= tol {
            self.failures.push(format!("absolute level {a} vs {b}"));
        }
        self
    }

    fn finish(&self) {
        assert!(
            self.failures.is_empty(),
            "[{}] {}",
            self.case.name,
            self.failures.join("\n")
        );
    }
}

fn skip(name: &str) {
    eprintln!("SKIP {name}: no black-box XM oracle (openmpt123 / xmp) on PATH");
}

/// Resolve the oracle and render `case`; `run` is the checked [`Run`].
macro_rules! oracle_run {
    ($run:ident, $case:expr) => {
        let case = $case;
        let Some(o) = oracle() else {
            return skip(case.name);
        };
        let Some(mut $run) = Run::new(&o, case) else {
            panic!("oracle render failed");
        };
    };
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A scale across the keyboard under one frequency table.
fn case_scale(linear: bool) -> Case {
    let mut w = base_writer();
    w.linear = linear;
    let mut p = XmWriterPattern::new(16);
    for (i, note) in [C4, E4, G4, C5, C3, C4 + 19, C4 - 5, C4 + 1]
        .iter()
        .enumerate()
    {
        p.note(i as u16 * 2, 0, *note, 1);
    }
    one_pattern(
        if linear {
            "scale_linear"
        } else {
            "scale_amiga"
        },
        w,
        p,
    )
}

/// Sample finetune + relative note + `E5x` set finetune under one
/// frequency table.
fn case_tuning(linear: bool) -> Case {
    let mut w = base_writer();
    w.linear = linear;
    let mut fine = base_sample();
    fine.finetune = 64; // +half a semitone
    fine.relative_note = 7;
    let mut ins = base_instrument();
    ins.samples.push(fine);
    for n in 0..96 {
        ins.sample_map[n] = if n >= 60 { 1 } else { 0 };
    }
    w.instruments = vec![ins];
    let mut p = XmWriterPattern::new(16);
    p.note(0, 0, C4, 1);
    p.note(2, 0, C5, 1); // fine sample: C-5 + 7 + 50 cents
    p.note(4, 0, C5 + 5, 1);
    p.put(6, 0, with_effect(cell_note(C4, 1), FX_E, 0x58)); // E58
    p.put(8, 0, with_effect(cell_note(C4, 1), FX_E, 0x57)); // E57
    p.put(10, 0, with_effect(cell_note(C5, 1), FX_E, 0x50)); // E50 on fine
    p.put(12, 0, with_effect(cell_note(C4, 1), FX_E, 0x5F)); // E5F
    p.note(14, 0, C4, 1);
    one_pattern(
        if linear {
            "tuning_linear"
        } else {
            "tuning_amiga"
        },
        w,
        p,
    )
}

/// Portamento up / down + tone portamento + fine / extra-fine slides.
fn case_slides(linear: bool) -> Case {
    let mut w = base_writer();
    w.linear = linear;
    let mut p = XmWriterPattern::new(16);
    p.note(0, 0, C4, 1);
    p.effect(1, 0, FX_PORTA_UP, 0x08);
    p.effect(2, 0, FX_PORTA_UP, 0x00);
    p.effect(3, 0, FX_PORTA_DOWN, 0x10);
    p.effect(4, 0, FX_PORTA_DOWN, 0x00);
    p.put(6, 0, with_effect(cell_note(C5, 0), FX_TONE_PORTA, 0x10));
    p.effect(7, 0, FX_TONE_PORTA, 0x00);
    p.effect(8, 0, FX_TONE_PORTA, 0x00);
    p.put(9, 0, with_effect(cell_note(C4, 0), FX_TONE_PORTA, 0x40));
    p.effect(11, 0, FX_E, 0x14); // E14 fine up
    p.effect(12, 0, FX_E, 0x10); // E10 memory
    p.effect(13, 0, FX_X, 0x28); // X28 extra fine down
    p.effect(14, 0, FX_X, 0x20); // X20 memory
    one_pattern(
        if linear {
            "slides_linear"
        } else {
            "slides_amiga"
        },
        w,
        p,
    )
}

/// Envelope with both a sustain point and a loop: the sustain point
/// sits inside the loop, so while the key is held the cursor must park
/// on the sustain point (not run the loop); after key-off the loop
/// runs.
fn case_env_sustain_vs_loop() -> Case {
    let mut w = base_writer();
    w.instruments[0].volume_envelope = XmWriterEnvelope {
        points: vec![(0, 64), (12, 16), (24, 64), (36, 16)],
        sustain_point: 1,
        loop_start_point: 0,
        loop_end_point: 3,
        type_bits: XM_ENV_ON | XM_ENV_SUSTAIN | XM_ENV_LOOP,
    };
    w.instruments[0].volume_fadeout = 0;
    let mut p = XmWriterPattern::new(16);
    p.note(0, 0, C4, 1);
    p.note(6, 0, KEY_OFF, 0);
    one_pattern("env_sustain_vs_loop", w, p)
}

/// Sustain point placed *after* the loop: the loop runs while the key
/// is held and the sustain point is never reached.
fn case_env_loop_before_sustain() -> Case {
    let mut w = base_writer();
    w.instruments[0].volume_envelope = XmWriterEnvelope {
        points: vec![(0, 64), (6, 8), (12, 64), (30, 8), (48, 64)],
        sustain_point: 3,
        loop_start_point: 0,
        loop_end_point: 2,
        type_bits: XM_ENV_ON | XM_ENV_SUSTAIN | XM_ENV_LOOP,
    };
    let mut p = XmWriterPattern::new(16);
    p.note(0, 0, C4, 1);
    p.note(8, 0, KEY_OFF, 0);
    one_pattern("env_loop_before_sustain", w, p)
}

/// Key-off (note 97 and `Kxx`) on an envelope-less instrument, on an
/// instrument whose envelope is present but switched off, and on one
/// with an envelope + fadeout.
fn case_keyoff() -> Case {
    let mut w = base_writer();
    let mut off_env = base_instrument();
    off_env.volume_envelope = XmWriterEnvelope {
        points: vec![(0, 64), (100, 64)],
        type_bits: 0,
        ..XmWriterEnvelope::default()
    };
    off_env.volume_fadeout = 4096;
    let mut fade = base_instrument();
    fade.volume_envelope = XmWriterEnvelope {
        points: vec![(0, 64), (100, 64)],
        type_bits: XM_ENV_ON,
        ..XmWriterEnvelope::default()
    };
    fade.volume_fadeout = 4096;
    w.instruments = vec![base_instrument(), off_env, fade];
    let mut p = XmWriterPattern::new(24);
    p.note(0, 0, C4, 1);
    p.note(2, 0, KEY_OFF, 0);
    p.note(4, 0, E4, 2);
    p.note(6, 0, KEY_OFF, 0);
    p.note(8, 0, G4, 3);
    p.note(10, 0, KEY_OFF, 0);
    p.note(14, 0, C4, 1);
    p.effect(16, 0, FX_KEY_OFF, 0x00);
    p.note(18, 0, G4, 3);
    p.effect(20, 0, FX_KEY_OFF, 0x00);
    one_pattern("keyoff", w, p)
}

/// Key-off inside a sustained envelope with fadeout: the release runs
/// the envelope past the sustain point while the fadeout ramps.
fn case_fadeout() -> Case {
    let mut w = base_writer();
    w.instruments[0].volume_envelope = XmWriterEnvelope {
        points: vec![(0, 64), (6, 64), (30, 32), (60, 32)],
        sustain_point: 1,
        type_bits: XM_ENV_ON | XM_ENV_SUSTAIN,
        ..XmWriterEnvelope::default()
    };
    w.instruments[0].volume_fadeout = 1024;
    let mut p = XmWriterPattern::new(16);
    p.note(0, 0, C4, 1);
    p.note(3, 0, KEY_OFF, 0);
    one_pattern("fadeout", w, p)
}

/// Panning envelope + autovibrato sweep, triggered through a note
/// delay.
fn case_pan_env_autovib_delayed() -> Case {
    let mut w = base_writer();
    w.instruments[0].panning_envelope = XmWriterEnvelope {
        points: vec![(0, 0), (12, 64), (24, 0)],
        loop_start_point: 0,
        loop_end_point: 2,
        type_bits: XM_ENV_ON | XM_ENV_LOOP,
        ..XmWriterEnvelope::default()
    };
    w.instruments[0].vibrato_type = 0;
    w.instruments[0].vibrato_sweep = 24;
    w.instruments[0].vibrato_depth = 15;
    w.instruments[0].vibrato_rate = 24;
    let mut p = XmWriterPattern::new(16);
    p.put(0, 0, with_effect(cell_note(C4, 1), FX_E, 0xD3));
    p.put(8, 0, with_effect(cell_note(C4, 1), FX_E, 0xD0));
    one_pattern("pan_env_autovib_delayed", w, p)
}

/// Volume envelope triggered through a note delay: the envelope must
/// start on the delayed trigger tick.
fn case_vol_env_delayed() -> Case {
    let mut w = base_writer();
    w.instruments[0].volume_envelope = XmWriterEnvelope {
        points: vec![(0, 0), (8, 64), (16, 8), (40, 8)],
        type_bits: XM_ENV_ON,
        ..XmWriterEnvelope::default()
    };
    let mut p = XmWriterPattern::new(16);
    p.put(0, 0, with_effect(cell_note(C4, 1), FX_E, 0xD3));
    p.put(8, 0, with_effect(cell_note(C4, 1), FX_E, 0xD5));
    one_pattern("vol_env_delayed", w, p)
}

/// Autovibrato shapes: the three documented type values (0, 1, 2)
/// plus a retrigger of the sine instrument. Values ≥ 3 are outside
/// the FT2 manual's set and are not gated.
fn case_autovib_shapes() -> Case {
    let mut w = base_writer();
    let mut shapes = Vec::new();
    for ty in [0u8, 1, 2] {
        let mut ins = base_instrument();
        ins.vibrato_type = ty;
        ins.vibrato_sweep = 0;
        ins.vibrato_depth = 12;
        ins.vibrato_rate = 16;
        shapes.push(ins);
    }
    w.instruments = shapes;
    let mut p = XmWriterPattern::new(16);
    p.note(0, 0, C4, 1);
    p.note(4, 0, C4, 2);
    p.note(8, 0, C4, 3);
    p.note(12, 0, C4, 1);
    p.note(14, 0, C4, 1);
    one_pattern("autovib_shapes", w, p)
}

/// Autovibrato depth / rate scale (no sweep).
fn case_autovib_depth() -> Case {
    let mut w = base_writer();
    let mut set = Vec::new();
    for (depth, rate) in [(4u8, 8u8), (15, 8), (8, 32), (15, 2)] {
        let mut ins = base_instrument();
        ins.vibrato_depth = depth;
        ins.vibrato_rate = rate;
        set.push(ins);
    }
    w.instruments = set;
    let mut p = XmWriterPattern::new(16);
    p.note(0, 0, C4, 1);
    p.note(4, 0, C4, 2);
    p.note(8, 0, C4, 3);
    p.note(12, 0, C4, 4);
    one_pattern("autovib_depth", w, p)
}

/// Note delay with a volume column and with an instrument-less note;
/// `EDx` with x ≥ speed.
fn case_note_delay() -> Case {
    let w = base_writer();
    let mut p = XmWriterPattern::new(16);
    p.put(
        0,
        0,
        with_volume(with_effect(cell_note(C4, 1), FX_E, 0xD2), 0x30),
    );
    p.put(2, 0, with_effect(cell_note(E4, 0), FX_E, 0xD3));
    p.put(4, 0, with_effect(cell_note(G4, 1), FX_E, 0xD6));
    p.put(6, 0, with_effect(cell_note(C5, 1), FX_E, 0xD5));
    p.put(
        8,
        0,
        with_volume(with_effect(cell_note(C4, 1), FX_E, 0xD1), 0x74),
    );
    p.put(10, 0, with_effect(cell_note(KEY_OFF, 0), FX_E, 0xD3));
    p.put(12, 0, with_effect(cell_note(E4, 1), FX_E, 0xD0));
    p.put(13, 0, with_effect(cell_note(0, 1), FX_E, 0xD2));
    one_pattern("note_delay", w, p)
}

/// Global volume, global volume slide with memory, and `Hxx` from a
/// second channel.
fn case_global_volume() -> Case {
    let w = base_writer();
    let mut p = XmWriterPattern::new(16);
    p.put(0, 0, with_effect(cell_note(C4, 1), FX_GLOBAL_VOL, 0x20));
    p.effect(2, 0, FX_GLOBAL_SLIDE, 0x04);
    p.effect(3, 0, FX_GLOBAL_SLIDE, 0x00);
    p.effect(4, 0, FX_GLOBAL_SLIDE, 0x00);
    p.effect(6, 0, FX_GLOBAL_SLIDE, 0x08);
    p.effect(7, 0, FX_GLOBAL_SLIDE, 0x00);
    p.effect(8, 1, FX_GLOBAL_SLIDE, 0x00);
    p.effect(10, 0, FX_GLOBAL_VOL, 0x40);
    p.effect(11, 0, FX_GLOBAL_VOL, 0x7F);
    p.effect(12, 0, FX_GLOBAL_SLIDE, 0x0F);
    p.effect(13, 0, FX_GLOBAL_SLIDE, 0x0F);
    p.effect(14, 0, FX_GLOBAL_SLIDE, 0x0F);
    one_pattern("global_volume", w, p)
}

/// Volume slides + fine slides + set volume (standard column).
fn case_volume_effects() -> Case {
    let w = base_writer();
    let mut p = XmWriterPattern::new(16);
    p.put(0, 0, with_effect(cell_note(C4, 1), FX_VOLUME, 0x40));
    p.effect(1, 0, FX_VOL_SLIDE, 0x04);
    p.effect(2, 0, FX_VOL_SLIDE, 0x00);
    p.put(
        4,
        0,
        with_effect(cell_effect(FX_VOL_SLIDE, 0x00), FX_VOLUME, 0x10),
    );
    p.effect(5, 0, FX_VOL_SLIDE, 0x40);
    p.effect(6, 0, FX_VOL_SLIDE, 0x00);
    p.effect(7, 0, FX_VOL_SLIDE, 0x22);
    p.effect(9, 0, FX_E, 0xB8);
    p.effect(10, 0, FX_E, 0xB0);
    p.effect(11, 0, FX_E, 0xA4);
    p.effect(12, 0, FX_E, 0xA0);
    p.effect(13, 0, FX_VOLUME, 0x50);
    one_pattern("volume_effects", w, p)
}

/// Vibrato depth / rate with memory and the vol-column vibrato forms.
fn case_vibrato() -> Case {
    let w = base_writer();
    let mut p = XmWriterPattern::new(16);
    p.put(0, 0, with_effect(cell_note(C4, 1), FX_VIBRATO, 0x2F));
    for r in 1..6 {
        p.effect(r, 0, FX_VIBRATO, 0x00);
    }
    p.put(8, 0, with_volume(cell_note(C4, 1), 0xA4)); // vibrato speed 4
    p.put(9, 0, with_volume(empty(), 0xBF)); // vibrato depth F
    p.put(10, 0, with_volume(empty(), 0xB0));
    p.put(11, 0, with_volume(empty(), 0xB0));
    p.put(13, 0, with_effect(cell_note(C4, 1), FX_E, 0x41)); // ramp
    p.effect(14, 0, FX_VIBRATO, 0x4F);
    one_pattern("vibrato", w, p)
}

/// Vibrato depth scale across the nibble range.
fn case_vibrato_depth() -> Case {
    let w = base_writer();
    let mut p = XmWriterPattern::new(16);
    p.put(0, 0, with_effect(cell_note(C4, 1), FX_VIBRATO, 0x21));
    p.effect(1, 0, FX_VIBRATO, 0x00);
    p.put(3, 0, with_effect(cell_note(C4, 1), FX_VIBRATO, 0x24));
    p.effect(4, 0, FX_VIBRATO, 0x00);
    p.put(6, 0, with_effect(cell_note(C4, 1), FX_VIBRATO, 0x28));
    p.effect(7, 0, FX_VIBRATO, 0x00);
    p.put(9, 0, with_effect(cell_note(C4, 1), FX_VIBRATO, 0x1F));
    p.effect(10, 0, FX_VIBRATO, 0x00);
    p.put(12, 0, with_effect(cell_note(C4, 1), FX_VIBRATO, 0x82));
    p.effect(13, 0, FX_VIBRATO, 0x00);
    one_pattern("vibrato_depth", w, p)
}

/// Tremolo shapes and depth.
fn case_tremolo() -> Case {
    let w = base_writer();
    let mut p = XmWriterPattern::new(16);
    p.put(
        0,
        0,
        with_volume(with_effect(cell_note(C4, 1), FX_TREMOLO, 0x28), 0x30),
    );
    for r in 1..4 {
        p.effect(r, 0, FX_TREMOLO, 0x00);
    }
    p.put(
        5,
        0,
        with_volume(with_effect(cell_note(C4, 1), FX_E, 0x71), 0x30),
    );
    p.effect(6, 0, FX_TREMOLO, 0x2F);
    p.effect(7, 0, FX_TREMOLO, 0x00);
    p.put(
        9,
        0,
        with_volume(with_effect(cell_note(C4, 1), FX_E, 0x72), 0x30),
    );
    p.effect(10, 0, FX_TREMOLO, 0x4F);
    p.effect(11, 0, FX_TREMOLO, 0x00);
    p.put(13, 0, with_volume(cell_note(C4, 1), 0x30));
    one_pattern("tremolo", w, p)
}

/// Tremor with memory, and the gate latch when the effect ends inside
/// its "off" phase: `Cxx` and a volume-column value do not re-open the
/// channel, a new note does.
fn case_tremor() -> Case {
    let w = base_writer();
    let mut p = XmWriterPattern::new(16);
    p.put(0, 0, with_effect(cell_note(C4, 1), FX_TREMOR, 0x21));
    for r in 1..5 {
        p.effect(r, 0, FX_TREMOR, 0x00);
    }
    p.effect(6, 0, FX_VOLUME, 0x40);
    p.put(8, 0, with_effect(cell_note(C4, 1), FX_TREMOR, 0x13));
    p.effect(9, 0, FX_TREMOR, 0x00);
    p.effect(10, 0, FX_TREMOR, 0x00);
    p.put(11, 0, with_volume(empty(), 0x40));
    p.effect(13, 0, FX_VOLUME, 0x30);
    p.note(14, 0, C4, 1);
    one_pattern("tremor", w, p)
}

/// Arpeggio (with memory-less zero param) and the tick-0 semantics.
fn case_arpeggio() -> Case {
    let w = base_writer();
    let mut p = XmWriterPattern::new(16);
    p.put(0, 0, with_effect(cell_note(C4, 1), FX_ARP, 0x47));
    p.effect(1, 0, FX_ARP, 0x47);
    p.effect(2, 0, FX_ARP, 0x30);
    p.put(4, 0, with_effect(cell_note(C4, 1), FX_ARP, 0x0C));
    p.effect(5, 0, FX_ARP, 0x00);
    p.put(7, 0, with_effect(cell_note(C4, 1), FX_SPEED, 0x04));
    p.effect(8, 0, FX_ARP, 0x47);
    p.effect(9, 0, FX_ARP, 0x47);
    p.effect(10, 0, FX_SPEED, 0x06);
    one_pattern("arpeggio", w, p)
}

/// Retrig (`E9x`), multi retrig (`Rxy`) and note cut.
fn case_retrig() -> Case {
    let w = base_writer();
    let mut p = XmWriterPattern::new(16);
    p.put(0, 0, with_effect(cell_note(C4, 1), FX_E, 0x92));
    p.effect(1, 0, FX_E, 0x93);
    p.put(3, 0, with_effect(cell_note(C4, 1), FX_MULTI_RETRIG, 0x32));
    p.effect(4, 0, FX_MULTI_RETRIG, 0x00);
    p.effect(5, 0, FX_MULTI_RETRIG, 0x03);
    p.put(7, 0, with_effect(cell_note(C4, 1), FX_E, 0xC3));
    p.put(9, 0, with_effect(cell_note(C4, 1), FX_E, 0xC0));
    p.put(10, 0, with_effect(cell_note(C4, 1), FX_MULTI_RETRIG, 0xE1));
    p.put(12, 0, with_effect(cell_note(C4, 1), FX_MULTI_RETRIG, 0x71));
    p.effect(13, 0, FX_MULTI_RETRIG, 0x00);
    one_pattern("retrig", w, p)
}

/// `Lxx` set envelope position.
fn case_envpos() -> Case {
    let mut w = base_writer();
    w.instruments[0].volume_envelope = XmWriterEnvelope {
        points: vec![(0, 0), (24, 64), (48, 8), (72, 64)],
        type_bits: XM_ENV_ON,
        ..XmWriterEnvelope::default()
    };
    let mut p = XmWriterPattern::new(16);
    p.put(0, 0, with_effect(cell_note(C4, 1), FX_ENV_POS, 0x30));
    p.put(2, 0, cell_effect(FX_ENV_POS, 0x00));
    p.put(4, 0, cell_effect(FX_ENV_POS, 0x18));
    p.put(6, 0, cell_effect(FX_ENV_POS, 0x60));
    p.put(8, 0, cell_effect(FX_ENV_POS, 0x30));
    one_pattern("envpos", w, p)
}

/// Panning: `8xx`, `Pxy` with memory, volume-column pan + pan slides,
/// sample default pan.
fn case_panning() -> Case {
    let mut w = base_writer();
    w.instruments[0].samples[0].panning = 0x20;
    let mut p = XmWriterPattern::new(16);
    p.note(0, 0, C4, 1);
    p.effect(1, 0, FX_PAN, 0xFF);
    p.effect(2, 0, FX_PAN, 0x00);
    p.effect(3, 0, FX_PAN, 0x80);
    p.effect(4, 0, FX_PAN_SLIDE, 0x80);
    p.effect(5, 0, FX_PAN_SLIDE, 0x00);
    p.effect(6, 0, FX_PAN_SLIDE, 0x0F);
    p.effect(7, 0, FX_PAN_SLIDE, 0x00);
    p.put(9, 0, with_volume(empty(), 0xCF));
    p.put(10, 0, with_volume(empty(), 0xD4));
    p.put(11, 0, with_volume(empty(), 0xC0));
    p.put(12, 0, with_volume(empty(), 0xE8));
    p.note(14, 0, C4, 1);
    one_pattern("panning", w, p)
}

/// Speed / BPM, pattern delay and `EEx` with a note delay under it,
/// single channel so the tick trace is unambiguous.
fn case_timing() -> Case {
    let w = base_writer();
    let mut p = XmWriterPattern::new(12);
    p.note(0, 0, C4, 1);
    p.put(1, 0, with_effect(cell_note(E4, 1), FX_E, 0xE1));
    p.put(2, 0, with_effect(cell_note(G4, 1), FX_SPEED, 0x03));
    p.note(3, 0, C5, 1);
    p.put(4, 0, with_effect(cell_note(C4, 1), FX_SPEED, 0x06));
    p.put(5, 0, with_effect(cell_note(E4, 1), FX_E, 0xE2));
    p.put(5, 1, cell_effect(FX_E, 0xD2));
    p.put(7, 0, with_effect(cell_note(G4, 1), FX_SPEED, 0xFA));
    p.note(8, 0, C5, 1);
    p.note(9, 0, C4, 1);
    p.put(10, 0, with_effect(cell_note(E4, 1), FX_SPEED, 0x7D));
    p.note(11, 0, G4, 1);
    one_pattern("timing", w, p)
}

/// Glissando + tone portamento with the vol-column `Mx` form.
fn case_glissando() -> Case {
    let w = base_writer();
    let mut p = XmWriterPattern::new(16);
    p.note(0, 0, C4, 1);
    p.put(1, 0, with_effect(cell_note(C5, 0), FX_E, 0x31));
    p.put(2, 0, with_effect(cell_note(C5, 0), FX_TONE_PORTA, 0x08));
    p.effect(3, 0, FX_TONE_PORTA, 0x00);
    p.effect(4, 0, FX_TONE_PORTA, 0x00);
    p.effect(5, 0, FX_TONE_PORTA, 0x00);
    p.put(8, 0, with_effect(cell_note(C4, 0), FX_E, 0x30));
    p.put(9, 0, with_volume(cell_note(C4, 0), 0xF2));
    p.put(10, 0, with_volume(empty(), 0xF0));
    p.put(11, 0, with_volume(empty(), 0xF0));
    one_pattern("glissando", w, p)
}

// ---------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------

#[test]
fn oracle_scale_amiga() {
    oracle_run!(r, case_scale(false));
    r.pitch(6.0).rms(0.12).finish();
}

#[test]
fn oracle_tuning_linear() {
    oracle_run!(r, case_tuning(true));
    r.pitch(6.0).finish();
}

#[test]
fn oracle_tuning_amiga() {
    oracle_run!(r, case_tuning(false));
    r.pitch(6.0).finish();
}

#[test]
fn oracle_slides_linear() {
    oracle_run!(r, case_slides(true));
    r.pitch(12.0).tick_pitch(25.0).finish();
}

#[test]
fn oracle_slides_amiga() {
    oracle_run!(r, case_slides(false));
    r.pitch(12.0).tick_pitch(25.0).finish();
}

#[test]
fn oracle_env_sustain_vs_loop() {
    oracle_run!(r, case_env_sustain_vs_loop());
    r.pitch(6.0).tick_rms(0.12).finish();
}

#[test]
fn oracle_env_loop_before_sustain() {
    oracle_run!(r, case_env_loop_before_sustain());
    r.pitch(6.0).tick_rms(0.12).finish();
}

#[test]
fn oracle_keyoff() {
    oracle_run!(r, case_keyoff());
    r.pitch(6.0).tick_rms(0.12).finish();
}

#[test]
fn oracle_fadeout() {
    oracle_run!(r, case_fadeout());
    r.pitch(6.0).tick_rms(0.12).finish();
}

#[test]
fn oracle_pan_env_autovib_delayed() {
    oracle_run!(r, case_pan_env_autovib_delayed());
    r.balance(0.08).tick_pitch(12.0).tick_rms(0.12).finish();
}

#[test]
fn oracle_vol_env_delayed() {
    oracle_run!(r, case_vol_env_delayed());
    r.tick_rms(0.12).finish();
}

#[test]
fn oracle_autovib_shapes() {
    oracle_run!(r, case_autovib_shapes());
    r.tick_pitch(15.0).finish();
}

#[test]
fn oracle_autovib_depth() {
    oracle_run!(r, case_autovib_depth());
    r.tick_pitch(15.0).finish();
}

#[test]
fn oracle_note_delay() {
    oracle_run!(r, case_note_delay());
    r.tick_trace().tick_rms(0.12).finish();
}

#[test]
fn oracle_global_volume() {
    oracle_run!(r, case_global_volume());
    r.pitch(6.0).tick_rms(0.12).finish();
}

#[test]
fn oracle_volume_effects() {
    oracle_run!(r, case_volume_effects());
    r.pitch(6.0).tick_rms(0.12).finish();
}

#[test]
fn oracle_vibrato() {
    oracle_run!(r, case_vibrato());
    r.tick_pitch(15.0).finish();
}

#[test]
fn oracle_vibrato_depth() {
    oracle_run!(r, case_vibrato_depth());
    r.tick_pitch(15.0).finish();
}

#[test]
fn oracle_tremolo() {
    oracle_run!(r, case_tremolo());
    r.tick_rms(0.15).finish();
}

#[test]
fn oracle_tremor() {
    oracle_run!(r, case_tremor());
    r.tick_rms(0.15).finish();
}

#[test]
fn oracle_arpeggio() {
    oracle_run!(r, case_arpeggio());
    r.tick_pitch(15.0).finish();
}

#[test]
fn oracle_retrig() {
    oracle_run!(r, case_retrig());
    r.tick_rms(0.15).tick_pitch(15.0).finish();
}

#[test]
fn oracle_envpos() {
    oracle_run!(r, case_envpos());
    r.tick_rms(0.12).finish();
}

#[test]
fn oracle_panning() {
    oracle_run!(r, case_panning());
    r.balance(0.08).finish();
}

#[test]
fn oracle_timing() {
    oracle_run!(r, case_timing());
    r.tick_trace().finish();
}

#[test]
fn oracle_glissando() {
    oracle_run!(r, case_glissando());
    r.pitch(6.0).tick_pitch(12.0).finish();
}
