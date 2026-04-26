//! Libmodplug-as-black-box-oracle per-row comparison harness.
//!
//! Drives both our decoder and a runtime-loaded libmodplug instance over
//! the same MOD file, querying each engine for `(order, pattern, row,
//! speed, tempo)` after every short render chunk. Surfaces the very first
//! row at which the two engines disagree — either on song position or
//! (when position agrees) on PCM content.
//!
//! libmodplug is loaded with `libloading` so this test compiles + links
//! without a system C dependency. If `libmodplug.dylib` /
//! `libmodplug.so` is not present on the host the test prints a clean
//! SKIP message and returns success — useful for CI hosts that don't
//! ship libmodplug. Override the search path with `LIBMODPLUG_PATH`.
//!
//! NOTE: this file only consumes the **public C API** declared in
//! `/opt/homebrew/Cellar/libmodplug/0.8.9.0/include/libmodplug/modplug.h`.
//! No libmodplug source code is read or referenced; the dylib is treated
//! strictly as an opaque oracle for behaviour comparison.
//!
//! libmodplug is configured to **disable** all of its audio-shaping
//! defaults (oversampling, megabass, surround, noise reduction, reverb)
//! and use NEAREST resampling at 44100 Hz / 16-bit / stereo so the
//! comparison isolates the playback-engine bug from libmodplug's mixer
//! colouration.

use std::ffi::c_void;
use std::fs;
use std::path::PathBuf;

use libloading::{Library, Symbol};

use oxideav_core::{
    CodecId, CodecParameters, CodecRegistry, Decoder, Error, Frame, Packet, TimeBase,
};
use oxideav_mod::{container::OUTPUT_SAMPLE_RATE, register_codecs, CODEC_ID_STR};

// ---------- Public-API ABI mirror (from modplug.h) ----------

#[repr(C)]
#[derive(Clone, Copy, Default)]
#[allow(non_snake_case)]
struct ModPlugSettings {
    mFlags: i32,
    mChannels: i32,
    mBits: i32,
    mFrequency: i32,
    mResamplingMode: i32,
    mStereoSeparation: i32,
    mMaxMixChannels: i32,
    mReverbDepth: i32,
    mReverbDelay: i32,
    mBassAmount: i32,
    mBassRange: i32,
    mSurroundDepth: i32,
    mSurroundDelay: i32,
    mLoopCount: i32,
}

// modplug.h: enum _ModPlug_Flags
const MODPLUG_ENABLE_OVERSAMPLING: i32 = 1 << 0;
const MODPLUG_ENABLE_NOISE_REDUCTION: i32 = 1 << 1;
const MODPLUG_ENABLE_REVERB: i32 = 1 << 2;
const MODPLUG_ENABLE_MEGABASS: i32 = 1 << 3;
const MODPLUG_ENABLE_SURROUND: i32 = 1 << 4;

// modplug.h: enum _ModPlug_ResamplingMode
#[allow(dead_code)]
const MODPLUG_RESAMPLE_NEAREST: i32 = 0;
const MODPLUG_RESAMPLE_LINEAR: i32 = 1;

type ModPlugFile = c_void;

type FnModPlugLoad = unsafe extern "C" fn(*const c_void, i32) -> *mut ModPlugFile;
type FnModPlugUnload = unsafe extern "C" fn(*mut ModPlugFile);
type FnModPlugRead = unsafe extern "C" fn(*mut ModPlugFile, *mut c_void, i32) -> i32;
type FnModPlugGetCurrentOrder = unsafe extern "C" fn(*mut ModPlugFile) -> i32;
type FnModPlugGetCurrentPattern = unsafe extern "C" fn(*mut ModPlugFile) -> i32;
type FnModPlugGetCurrentRow = unsafe extern "C" fn(*mut ModPlugFile) -> i32;
type FnModPlugGetCurrentSpeed = unsafe extern "C" fn(*mut ModPlugFile) -> i32;
type FnModPlugGetCurrentTempo = unsafe extern "C" fn(*mut ModPlugFile) -> i32;
type FnModPlugGetSettings = unsafe extern "C" fn(*mut ModPlugSettings);
type FnModPlugSetSettings = unsafe extern "C" fn(*const ModPlugSettings);
type FnModPlugGetMasterVolume = unsafe extern "C" fn(*mut ModPlugFile) -> u32;
type FnModPlugSetMasterVolume = unsafe extern "C" fn(*mut ModPlugFile, u32);

struct ModPlugLib {
    _lib: Library,
    load: unsafe extern "C" fn(*const c_void, i32) -> *mut ModPlugFile,
    unload: unsafe extern "C" fn(*mut ModPlugFile),
    read: unsafe extern "C" fn(*mut ModPlugFile, *mut c_void, i32) -> i32,
    get_order: unsafe extern "C" fn(*mut ModPlugFile) -> i32,
    get_pattern: unsafe extern "C" fn(*mut ModPlugFile) -> i32,
    get_row: unsafe extern "C" fn(*mut ModPlugFile) -> i32,
    get_speed: unsafe extern "C" fn(*mut ModPlugFile) -> i32,
    get_tempo: unsafe extern "C" fn(*mut ModPlugFile) -> i32,
    get_settings: unsafe extern "C" fn(*mut ModPlugSettings),
    set_settings: unsafe extern "C" fn(*const ModPlugSettings),
    get_master_volume: unsafe extern "C" fn(*mut ModPlugFile) -> u32,
    /// Available for diagnostic tests that need to drive libmodplug at
    /// non-default master volumes; the headline gain calibration test
    /// keeps the default 128/512.
    #[allow(dead_code)]
    set_master_volume: unsafe extern "C" fn(*mut ModPlugFile, u32),
}

impl ModPlugLib {
    fn try_open() -> Option<Self> {
        let candidates: Vec<PathBuf> = {
            let mut v: Vec<PathBuf> = Vec::new();
            if let Ok(p) = std::env::var("LIBMODPLUG_PATH") {
                v.push(PathBuf::from(p));
            }
            v.push(PathBuf::from(
                "/opt/homebrew/Cellar/libmodplug/0.8.9.0/lib/libmodplug.dylib",
            ));
            // Also probe for any 0.8.9.x cellar version.
            if let Ok(entries) = fs::read_dir("/opt/homebrew/Cellar/libmodplug/") {
                for entry in entries.flatten() {
                    let candidate = entry.path().join("lib/libmodplug.dylib");
                    if candidate.exists() {
                        v.push(candidate);
                    }
                }
            }
            v.push(PathBuf::from("/opt/homebrew/lib/libmodplug.dylib"));
            v.push(PathBuf::from("/usr/local/lib/libmodplug.dylib"));
            v.push(PathBuf::from("libmodplug.dylib"));
            v.push(PathBuf::from("libmodplug.so.1"));
            v.push(PathBuf::from("libmodplug.so"));
            v
        };

        let lib = candidates
            .iter()
            .find_map(|p| unsafe { Library::new(p) }.ok())?;

        // Resolve every symbol once, then bind into the struct. If any
        // symbol is missing the test skips cleanly.
        unsafe {
            let load: Symbol<FnModPlugLoad> = lib.get(b"ModPlug_Load\0").ok()?;
            let unload: Symbol<FnModPlugUnload> = lib.get(b"ModPlug_Unload\0").ok()?;
            let read: Symbol<FnModPlugRead> = lib.get(b"ModPlug_Read\0").ok()?;
            let get_order: Symbol<FnModPlugGetCurrentOrder> =
                lib.get(b"ModPlug_GetCurrentOrder\0").ok()?;
            let get_pattern: Symbol<FnModPlugGetCurrentPattern> =
                lib.get(b"ModPlug_GetCurrentPattern\0").ok()?;
            let get_row: Symbol<FnModPlugGetCurrentRow> =
                lib.get(b"ModPlug_GetCurrentRow\0").ok()?;
            let get_speed: Symbol<FnModPlugGetCurrentSpeed> =
                lib.get(b"ModPlug_GetCurrentSpeed\0").ok()?;
            let get_tempo: Symbol<FnModPlugGetCurrentTempo> =
                lib.get(b"ModPlug_GetCurrentTempo\0").ok()?;
            let get_settings: Symbol<FnModPlugGetSettings> =
                lib.get(b"ModPlug_GetSettings\0").ok()?;
            let set_settings: Symbol<FnModPlugSetSettings> =
                lib.get(b"ModPlug_SetSettings\0").ok()?;
            let get_master_volume: Symbol<FnModPlugGetMasterVolume> =
                lib.get(b"ModPlug_GetMasterVolume\0").ok()?;
            let set_master_volume: Symbol<FnModPlugSetMasterVolume> =
                lib.get(b"ModPlug_SetMasterVolume\0").ok()?;

            // Convert each Symbol into a raw fn pointer so we don't have
            // to keep Symbols alive (Library is the lifetime root).
            let load = *load;
            let unload = *unload;
            let read = *read;
            let get_order = *get_order;
            let get_pattern = *get_pattern;
            let get_row = *get_row;
            let get_speed = *get_speed;
            let get_tempo = *get_tempo;
            let get_settings = *get_settings;
            let set_settings = *set_settings;
            let get_master_volume = *get_master_volume;
            let set_master_volume = *set_master_volume;

            Some(ModPlugLib {
                _lib: lib,
                load,
                unload,
                read,
                get_order,
                get_pattern,
                get_row,
                get_speed,
                get_tempo,
                get_settings,
                set_settings,
                get_master_volume,
                set_master_volume,
            })
        }
    }

    /// Configure libmodplug to disable every audio-shaping option and
    /// use NEAREST resampling at 44100 / 16-bit / stereo. This isolates
    /// the playback-engine output from libmodplug's mixer colouration
    /// so any divergence is attributable to per-row state, not to
    /// filter/oversampler choices.
    fn configure_clean(&self) {
        unsafe {
            let mut s = ModPlugSettings::default();
            (self.get_settings)(&mut s);
            // Wipe every flag — no oversampling, no megabass, no
            // surround, no reverb, no noise reduction.
            s.mFlags &= !(MODPLUG_ENABLE_OVERSAMPLING
                | MODPLUG_ENABLE_NOISE_REDUCTION
                | MODPLUG_ENABLE_REVERB
                | MODPLUG_ENABLE_MEGABASS
                | MODPLUG_ENABLE_SURROUND);
            s.mChannels = 2;
            s.mBits = 16;
            s.mFrequency = OUTPUT_SAMPLE_RATE as i32;
            // Match our mixer's linear interpolation. Nearest gives a
            // 4× sample-and-hold stairstep at 11 kHz / 44.1 kHz output
            // which dominates the per-window RMS difference and masks
            // any real per-row content divergence.
            s.mResamplingMode = MODPLUG_RESAMPLE_LINEAR;
            // Stereo separation 128 = 50% (libmodplug scale 1..256), to
            // match our DEFAULT_PAN_SEPARATION = 0.5.
            s.mStereoSeparation = 128;
            s.mMaxMixChannels = 64;
            s.mLoopCount = 0;
            (self.set_settings)(&s);
        }
    }
}

// ---------- Fixture loaders ----------

fn cache_path(name: &str) -> Option<PathBuf> {
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let crate_dir = std::env::var("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .expect("CARGO_MANIFEST_DIR set during cargo test");
            crate_dir.join("..").join("..").join("target")
        });
    let path = target_dir.join("test-fixtures").join(name);
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

/// Minimal RIFF/WAVE writer for S16LE PCM.
fn write_wav_s16le(
    path: &PathBuf,
    pcm: &[i16],
    sample_rate: u32,
    channels: u16,
) -> std::io::Result<()> {
    let bps: u16 = 16;
    let byte_rate = sample_rate * (channels as u32) * (bps as u32) / 8;
    let block_align = channels * bps / 8;
    let data_bytes = pcm.len() * 2;
    let mut buf = Vec::with_capacity(44 + data_bytes);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&bps.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for &s in pcm {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    fs::write(path, buf)
}

// ---------- Our-decoder rendering helper ----------

/// Render N stereo S16 frames from our decoder into a Vec.
fn decode_ours(bytes: Vec<u8>, max_frames: usize) -> Vec<i16> {
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    let params = CodecParameters::audio(CodecId::new(CODEC_ID_STR));
    let mut dec: Box<dyn Decoder> = reg.make_decoder(&params).expect("make_decoder");
    let pkt = Packet::new(0, TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64), bytes);
    dec.send_packet(&pkt).expect("send_packet");

    let mut pcm = Vec::with_capacity(max_frames * 2);
    loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(a)) => {
                for chunk in a.data[0].chunks_exact(2) {
                    pcm.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                }
                if pcm.len() / 2 >= max_frames {
                    break;
                }
            }
            Ok(_) => unreachable!(),
            Err(Error::Eof) => break,
            Err(e) => panic!("decode error: {e:?}"),
        }
    }
    pcm.truncate(max_frames * 2);
    pcm
}

/// Drive our decoder forward in chunks via a `PlayerState` so we can read
/// per-row state after each chunk.
struct OurStepper {
    player: oxideav_mod::player::PlayerState,
}

impl OurStepper {
    fn new(bytes: &[u8]) -> Self {
        use oxideav_mod::header::parse_header;
        use oxideav_mod::player::{parse_patterns, PlayerState};
        use oxideav_mod::samples::extract_samples;
        let header = parse_header(bytes).expect("header");
        let samples = extract_samples(&header, bytes);
        let patterns = parse_patterns(&header, bytes);
        let player = PlayerState::new(&header, samples, patterns, OUTPUT_SAMPLE_RATE);
        Self { player }
    }

    fn render(&mut self, dst: &mut [i16]) -> usize {
        self.player.render(dst)
    }

    fn order(&self) -> i32 {
        self.player.order_index as i32
    }
    fn row(&self) -> i32 {
        self.player.row as i32
    }
    fn pattern(&self) -> i32 {
        let oi = self.player.order_index as usize;
        self.player.order.get(oi).copied().unwrap_or(0) as i32
    }
    fn speed(&self) -> i32 {
        self.player.speed as i32
    }
    fn tempo(&self) -> i32 {
        self.player.bpm as i32
    }
    fn ended(&self) -> bool {
        self.player.ended
    }
    fn tick(&self) -> u8 {
        self.player.tick
    }
    fn tick_cursor(&self) -> u32 {
        self.player.tick_sample_cursor
    }
}

// ---------- Per-row trace + comparison ----------

#[derive(Debug, Clone, Copy)]
struct TraceRow {
    sample_idx: usize,
    our_order: i32,
    our_pattern: i32,
    our_row: i32,
    our_speed: i32,
    our_tempo: i32,
    mp_order: i32,
    mp_pattern: i32,
    mp_row: i32,
    mp_speed: i32,
    mp_tempo: i32,
    /// rms over the chunk for our render
    our_rms: f64,
    /// rms over the chunk for libmodplug render
    mp_rms: f64,
    /// per-sample rms diff (over the chunk)
    diff_rms: f64,
    /// per-sample rms diff after best linear scale (so a constant
    /// volume scaler is removed). Near-zero means content matches
    /// up to a global gain.
    scaled_diff_rms: f64,
    /// the best linear scale that takes b → a (i.e. our/mp).
    best_scale: f64,
    /// peak |sample| over the chunk for our render
    our_peak: i16,
    /// peak |sample| over the chunk for libmodplug render
    mp_peak: i16,
    /// number of clip-rail samples over the chunk (our)
    our_clip: usize,
    /// number of clip-rail samples over the chunk (mp)
    mp_clip: usize,
}

fn rms_i16(buf: &[i16]) -> f64 {
    if buf.is_empty() {
        return 0.0;
    }
    let s: f64 = buf.iter().map(|&v| (v as f64) * (v as f64)).sum();
    (s / buf.len() as f64).sqrt()
}

fn peak_abs_i16(buf: &[i16]) -> i16 {
    buf.iter().map(|&v| v.saturating_abs()).max().unwrap_or(0)
}

fn clip_count_i16(buf: &[i16]) -> usize {
    buf.iter()
        .filter(|&&v| v == i16::MAX || v == i16::MIN)
        .count()
}

fn diff_rms_i16(a: &[i16], b: &[i16]) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let mut s = 0.0f64;
    for i in 0..n {
        let d = a[i] as f64 - b[i] as f64;
        s += d * d;
    }
    (s / n as f64).sqrt()
}

/// Compute the best-scaled diff RMS: find the optimal `g` that
/// minimises `sum((a - g*b)^2)`, then return that minimum's RMS. Used
/// to ablate "ours is louder by a constant factor" from "ours has
/// content divergence" — if scaled_diff_rms is near zero, the only
/// difference is volume; if it's still large, something else is wrong.
fn scaled_diff_rms_i16(a: &[i16], b: &[i16]) -> (f64, f64) {
    let n = a.len().min(b.len());
    if n == 0 {
        return (0.0, 1.0);
    }
    let mut sa: f64 = 0.0;
    let mut sb: f64 = 0.0;
    let mut sab: f64 = 0.0;
    let mut sbb: f64 = 0.0;
    for i in 0..n {
        let av = a[i] as f64;
        let bv = b[i] as f64;
        sa += av * av;
        sb += bv * bv;
        sab += av * bv;
        sbb += bv * bv;
    }
    let _ = (sa, sb);
    // g = <a, b> / <b, b>
    let g = if sbb > 0.0 { sab / sbb } else { 1.0 };
    // residual rms after subtracting g*b from a
    let mut s = 0.0f64;
    for i in 0..n {
        let d = a[i] as f64 - g * (b[i] as f64);
        s += d * d;
    }
    ((s / n as f64).sqrt(), g)
}

/// Render the full trace into two PCM buffers (our + libmodplug) so we
/// can run cross-correlation / lag analysis on the raw samples.
fn render_both_full(bytes: &[u8], mp: &ModPlugLib, total_frames: usize) -> (Vec<i16>, Vec<i16>) {
    mp.configure_clean();
    let mp_file = unsafe { (mp.load)(bytes.as_ptr() as *const c_void, bytes.len() as i32) };
    if mp_file.is_null() {
        panic!("ModPlug_Load returned NULL");
    }
    mp.configure_clean();
    let mut mp_buf = vec![0u8; total_frames * 2 * 2];
    let mp_n = unsafe {
        (mp.read)(
            mp_file,
            mp_buf.as_mut_ptr() as *mut c_void,
            mp_buf.len() as i32,
        )
    };
    let mp_n = (mp_n as usize).min(mp_buf.len());
    let mp_pcm: Vec<i16> = mp_buf[..mp_n]
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    unsafe { (mp.unload)(mp_file) };

    let our_pcm = decode_ours(bytes.to_vec(), total_frames);
    (our_pcm, mp_pcm)
}

/// Find the integer lag `k` (in samples, signed) that maximises the
/// normalised cross-correlation of `a` (our render) against `b`
/// (libmodplug). Search range: ±max_lag samples.
fn find_lag(a: &[i16], b: &[i16], max_lag: i32, max_eval_len: usize) -> (i32, f64) {
    let n = a.len().min(b.len()).min(max_eval_len);
    if n < 2 * max_lag as usize + 100 {
        return (0, 0.0);
    }
    let mid = n / 2;
    let win = (max_eval_len.min(n / 2)).min(44_100);
    // pre-cache values in f64 for speed
    let a_f: Vec<f64> = a[..n].iter().map(|&v| v as f64).collect();
    let b_f: Vec<f64> = b[..n].iter().map(|&v| v as f64).collect();

    let mut best = (0i32, f64::NEG_INFINITY);
    for k in -max_lag..=max_lag {
        let lo = mid - win / 2;
        let hi = mid + win / 2;
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for (i, &av) in a_f.iter().enumerate().take(hi).skip(lo) {
            let bi = i as i32 + k;
            if bi < 0 || (bi as usize) >= n {
                continue;
            }
            sum += av * b_f[bi as usize];
            count += 1;
        }
        let xcorr = if count > 0 { sum / count as f64 } else { 0.0 };
        if xcorr > best.1 {
            best = (k, xcorr);
        }
    }
    best
}

/// Run the side-by-side trace. Returns the trace plus an option for the
/// first row at which `(order, row)` diverges.
fn trace(
    bytes: &[u8],
    mp: &ModPlugLib,
    chunk_frames: usize,
    total_frames: usize,
) -> (Vec<TraceRow>, Option<usize>) {
    // libmodplug load.
    mp.configure_clean();
    let mp_file = unsafe { (mp.load)(bytes.as_ptr() as *const c_void, bytes.len() as i32) };
    if mp_file.is_null() {
        panic!("ModPlug_Load returned NULL");
    }
    // Re-apply the clean settings AFTER load (some flags only take effect
    // on the next load — applying both around it is the safe pattern).
    mp.configure_clean();
    let mp_master_vol = unsafe { (mp.get_master_volume)(mp_file) };
    eprintln!(
        "[libmodplug_compare] mp master volume default = {} (range 1..512)",
        mp_master_vol
    );

    let mut ours = OurStepper::new(bytes);
    let mut trace = Vec::new();
    let mut first_div: Option<usize> = None;

    let mut sample_idx: usize = 0;
    let mut our_buf = vec![0i16; chunk_frames * 2];
    let mut mp_buf_bytes = vec![0u8; chunk_frames * 2 * 2];

    while sample_idx < total_frames {
        let want = chunk_frames.min(total_frames - sample_idx);
        // Render our chunk.
        let our_n = ours.render(&mut our_buf[..want * 2]);
        let our_chunk = &our_buf[..our_n * 2];

        // Render libmodplug's chunk.
        let mp_n = unsafe {
            (mp.read)(
                mp_file,
                mp_buf_bytes.as_mut_ptr() as *mut c_void,
                (want * 2 * 2) as i32,
            )
        };
        if mp_n <= 0 {
            // libmodplug ran out before us. Stop the trace.
            break;
        }
        let mp_n_frames = (mp_n as usize) / 4; // 2 ch * 2 bytes
        let mut mp_chunk: Vec<i16> = Vec::with_capacity(mp_n_frames * 2);
        for c in mp_buf_bytes[..(mp_n as usize)].chunks_exact(2) {
            mp_chunk.push(i16::from_le_bytes([c[0], c[1]]));
        }

        // Read state from both AFTER the chunk has been rendered.
        let row = TraceRow {
            sample_idx,
            our_order: ours.order(),
            our_pattern: ours.pattern(),
            our_row: ours.row(),
            our_speed: ours.speed(),
            our_tempo: ours.tempo(),
            mp_order: unsafe { (mp.get_order)(mp_file) },
            mp_pattern: unsafe { (mp.get_pattern)(mp_file) },
            mp_row: unsafe { (mp.get_row)(mp_file) },
            mp_speed: unsafe { (mp.get_speed)(mp_file) },
            mp_tempo: unsafe { (mp.get_tempo)(mp_file) },
            our_rms: rms_i16(our_chunk),
            mp_rms: rms_i16(&mp_chunk),
            diff_rms: diff_rms_i16(our_chunk, &mp_chunk),
            scaled_diff_rms: scaled_diff_rms_i16(our_chunk, &mp_chunk).0,
            best_scale: scaled_diff_rms_i16(our_chunk, &mp_chunk).1,
            our_peak: peak_abs_i16(our_chunk),
            mp_peak: peak_abs_i16(&mp_chunk),
            our_clip: clip_count_i16(our_chunk),
            mp_clip: clip_count_i16(&mp_chunk),
        };
        let diverged = row.our_order != row.mp_order || row.our_row != row.mp_row;
        if diverged && first_div.is_none() {
            first_div = Some(trace.len());
        }
        trace.push(row);

        sample_idx += our_n.max(mp_n_frames);
        if our_n == 0 || ours.ended() {
            break;
        }
    }

    unsafe { (mp.unload)(mp_file) };
    (trace, first_div)
}

// ---------- Tests ----------

fn skip_unless<T>(opt: Option<T>, msg: &str) -> Option<T> {
    if opt.is_none() {
        eprintln!("[libmodplug_compare] SKIP: {msg}");
    }
    opt
}

fn run_one(name: &str) {
    let lib = match ModPlugLib::try_open() {
        Some(l) => l,
        None => {
            eprintln!(
                "[libmodplug_compare] SKIP: libmodplug not found \
                 (set LIBMODPLUG_PATH or install libmodplug)"
            );
            return;
        }
    };

    let path = match skip_unless(
        cache_path(name),
        &format!("fixture {name} not cached under target/test-fixtures/"),
    ) {
        Some(p) => p,
        None => return,
    };
    let bytes = fs::read(&path).expect("read fixture");

    // Render ~60 s with 256-frame chunks to catch row transitions cleanly
    // and to capture the user-reported scrambling around 4.5s and any
    // downstream clipping events.
    let total_frames = OUTPUT_SAMPLE_RATE as usize * 60;
    let chunk = 256usize;
    let (trace, first_div) = trace(&bytes, &lib, chunk, total_frames);

    eprintln!(
        "[libmodplug_compare:{name}] traced {} chunks ({} frames each) over {} s",
        trace.len(),
        chunk,
        (trace.last().map(|r| r.sample_idx).unwrap_or(0)) as f64 / OUTPUT_SAMPLE_RATE as f64
    );

    // Aggregate clipping + peak summary across the whole trace.
    let total_our_clip: usize = trace.iter().map(|r| r.our_clip).sum();
    let total_mp_clip: usize = trace.iter().map(|r| r.mp_clip).sum();
    let max_our_peak: i16 = trace.iter().map(|r| r.our_peak).max().unwrap_or(0);
    let max_mp_peak: i16 = trace.iter().map(|r| r.mp_peak).max().unwrap_or(0);
    let our_total_rms_sum: f64 = trace.iter().map(|r| r.our_rms).sum();
    let mp_total_rms_sum: f64 = trace.iter().map(|r| r.mp_rms).sum();
    let global_loudness_ratio = if mp_total_rms_sum > 0.0 {
        our_total_rms_sum / mp_total_rms_sum
    } else {
        0.0
    };
    eprintln!(
        "[libmodplug_compare:{name}] clip totals: ours={} mp={} | peak: ours={} mp={} | \
         global RMS ratio (ours/mp) = {:.3}x",
        total_our_clip, total_mp_clip, max_our_peak, max_mp_peak, global_loudness_ratio,
    );

    // Find chunks with very high our_peak (>= 30000) — clipping risk.
    let mut at_risk: Vec<(f64, i16, i16)> = trace
        .iter()
        .filter(|r| r.our_peak >= 30_000)
        .map(|r| {
            (
                r.sample_idx as f64 / OUTPUT_SAMPLE_RATE as f64,
                r.our_peak,
                r.mp_peak,
            )
        })
        .collect();
    if !at_risk.is_empty() {
        eprintln!(
            "[libmodplug_compare:{name}] chunks at clipping risk (our_peak>=30000): {} chunks",
            at_risk.len()
        );
        for (t, op, mp) in at_risk.drain(..).take(20) {
            eprintln!("    t={:.3}s  our_peak={} mp_peak={}", t, op, mp);
        }
    }

    // Bulk render both engines into PCM buffers, then run lag detection
    // to see whether ours is "ahead" or "behind" libmodplug. If the lag
    // changes sign across the song, then there's clock-drift inside one
    // engine — a strong signal of a per-tick rounding bug.
    let (our_full, mp_full) = render_both_full(&bytes, &lib, total_frames);
    eprintln!(
        "[libmodplug_compare:{name}] bulk render lengths: ours={} samples ({:.3}s)  mp={} samples ({:.3}s)",
        our_full.len(),
        our_full.len() as f64 / OUTPUT_SAMPLE_RATE as f64 / 2.0,
        mp_full.len(),
        mp_full.len() as f64 / OUTPUT_SAMPLE_RATE as f64 / 2.0,
    );
    // Optional WAV dump for offline listening / inspection. Enabled
    // when LIBMODPLUG_DUMP_WAV is set.
    if std::env::var("LIBMODPLUG_DUMP_WAV").is_ok() {
        let dump_dir = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/cargo-target-mod"));
        let _ = fs::create_dir_all(&dump_dir);
        let our_path = dump_dir.join(format!("{name}-ours.wav"));
        let mp_path = dump_dir.join(format!("{name}-mp.wav"));
        write_wav_s16le(&our_path, &our_full, OUTPUT_SAMPLE_RATE, 2)
            .unwrap_or_else(|e| eprintln!("WAV write failed for ours: {e}"));
        write_wav_s16le(&mp_path, &mp_full, OUTPUT_SAMPLE_RATE, 2)
            .unwrap_or_else(|e| eprintln!("WAV write failed for mp: {e}"));
        eprintln!(
            "[libmodplug_compare:{name}] dumped wav files: {} {}",
            our_path.display(),
            mp_path.display()
        );
    }

    // Per-second lag scan over the L channel only.
    let our_l: Vec<i16> = our_full.chunks_exact(2).map(|c| c[0]).collect();
    let mp_l: Vec<i16> = mp_full.chunks_exact(2).map(|c| c[0]).collect();
    let total_secs = our_l.len() / OUTPUT_SAMPLE_RATE as usize;
    eprintln!("[libmodplug_compare:{name}] per-second lag (L channel, ±200 samples search):");
    for sec in 0..total_secs.min(15) {
        let s = sec * OUTPUT_SAMPLE_RATE as usize;
        let e = s + OUTPUT_SAMPLE_RATE as usize;
        if e > our_l.len() || e > mp_l.len() {
            break;
        }
        let (lag, _xc) = find_lag(&our_l[s..e], &mp_l[s..e], 200, OUTPUT_SAMPLE_RATE as usize);
        eprintln!(
            "  sec={sec:>2}  lag={lag:>4} samples  ({:.2} ms)",
            lag as f64 * 1000.0 / OUTPUT_SAMPLE_RATE as f64
        );
    }

    // Headline: first divergence in (order, row).
    if let Some(idx) = first_div {
        let row = trace[idx];
        eprintln!(
            "[libmodplug_compare:{name}] FIRST (order,row) DIVERGENCE @ chunk {idx} \
             (sample {} ≈ t={:.3}s):\n  ours:  order={} pattern={} row={} speed={} tempo={}\n  \
             libmp: order={} pattern={} row={} speed={} tempo={}",
            row.sample_idx,
            row.sample_idx as f64 / OUTPUT_SAMPLE_RATE as f64,
            row.our_order,
            row.our_pattern,
            row.our_row,
            row.our_speed,
            row.our_tempo,
            row.mp_order,
            row.mp_pattern,
            row.mp_row,
            row.mp_speed,
            row.mp_tempo,
        );
    } else {
        eprintln!("[libmodplug_compare:{name}] (order,row) AGREES across the whole trace");
    }

    // Find the first big PCM divergence (chunks with diff_rms > 4000) once
    // (order, row) has been entered for at least a couple of chunks.
    let mut first_pcm_div: Option<usize> = None;
    let pcm_threshold = 4000.0;
    for (i, r) in trace.iter().enumerate() {
        if r.diff_rms > pcm_threshold && r.our_rms > 200.0 && r.mp_rms > 200.0 {
            first_pcm_div = Some(i);
            break;
        }
    }
    if let Some(i) = first_pcm_div {
        let r = trace[i];
        eprintln!(
            "[libmodplug_compare:{name}] FIRST PCM DIVERGENCE > {} @ chunk {i} (t={:.3}s):\n  \
             ours rms={:.0} mp rms={:.0} diff_rms={:.0}\n  \
             ours: ord={} pat={} row={} sp={} bpm={} | mp: ord={} pat={} row={} sp={} bpm={}",
            pcm_threshold,
            r.sample_idx as f64 / OUTPUT_SAMPLE_RATE as f64,
            r.our_rms,
            r.mp_rms,
            r.diff_rms,
            r.our_order,
            r.our_pattern,
            r.our_row,
            r.our_speed,
            r.our_tempo,
            r.mp_order,
            r.mp_pattern,
            r.mp_row,
            r.mp_speed,
            r.mp_tempo,
        );
    }

    // Print a coarse per-second summary table — useful when debugging
    // by eye, not asserted.
    eprintln!(
        "[libmodplug_compare:{name}] per-second summary (t  our_rms  mp_rms  diff_rms  scaled_diff  scale  pos):"
    );
    let mut next_log = 0usize;
    for r in &trace {
        let t = r.sample_idx / OUTPUT_SAMPLE_RATE as usize;
        if t >= next_log {
            eprintln!(
                "  t={:>2}s  our={:>5.0} mp={:>5.0} diff={:>5.0} scaled={:>5.0} scale={:.3}  our=O{}P{}R{:02}  mp=O{}P{}R{:02}",
                t,
                r.our_rms,
                r.mp_rms,
                r.diff_rms,
                r.scaled_diff_rms,
                r.best_scale,
                r.our_order,
                r.our_pattern,
                r.our_row,
                r.mp_order,
                r.mp_pattern,
                r.mp_row,
            );
            next_log = t + 1;
        }
    }

    // Report every speed/tempo change we observe, in either engine.
    eprintln!("[libmodplug_compare:{name}] speed/tempo changes:");
    let mut last_our_sp = -1i32;
    let mut last_our_bpm = -1i32;
    let mut last_mp_sp = -1i32;
    let mut last_mp_bpm = -1i32;
    for r in &trace {
        let our_changed = r.our_speed != last_our_sp || r.our_tempo != last_our_bpm;
        let mp_changed = r.mp_speed != last_mp_sp || r.mp_tempo != last_mp_bpm;
        if our_changed || mp_changed {
            eprintln!(
                "  t={:.3}s O{}P{}R{:02}  ours sp={}/bpm={} mp sp={}/bpm={}{}{}",
                r.sample_idx as f64 / OUTPUT_SAMPLE_RATE as f64,
                r.our_order,
                r.our_pattern,
                r.our_row,
                r.our_speed,
                r.our_tempo,
                r.mp_speed,
                r.mp_tempo,
                if our_changed { "  [ours-changed]" } else { "" },
                if mp_changed { "  [mp-changed]" } else { "" },
            );
            last_our_sp = r.our_speed;
            last_our_bpm = r.our_tempo;
            last_mp_sp = r.mp_speed;
            last_mp_bpm = r.mp_tempo;
        }
    }

    // Also dump a tighter trace around the user-reported 4.5 s mark so we
    // can read row-by-row what was happening either side of the breakage.
    eprintln!("[libmodplug_compare:{name}] tight trace 9.5..12 s (chunk by chunk):");
    let lo = (OUTPUT_SAMPLE_RATE as usize) * 95 / 10;
    let hi = (OUTPUT_SAMPLE_RATE as usize) * 12;
    for r in trace
        .iter()
        .filter(|r| r.sample_idx >= lo && r.sample_idx <= hi)
    {
        eprintln!(
            "  t={:.3}s  our O{}P{}R{:02}  mp O{}P{}R{:02}  our_rms={:>5.0} mp_rms={:>5.0} diff={:>5.0} scaled={:>5.0} scale={:.3}",
            r.sample_idx as f64 / OUTPUT_SAMPLE_RATE as f64,
            r.our_order,
            r.our_pattern,
            r.our_row,
            r.mp_order,
            r.mp_pattern,
            r.mp_row,
            r.our_rms,
            r.mp_rms,
            r.diff_rms,
            r.scaled_diff_rms,
            r.best_scale,
        );
    }
}

#[test]
#[ignore = "requires libmodplug runtime + cached MOD fixtures; opt-in via cargo test --ignored"]
fn libmodplug_compare_halluc() {
    run_one("halluc.mod");
}

#[test]
#[ignore = "requires libmodplug runtime + cached MOD fixtures; opt-in via cargo test --ignored"]
fn libmodplug_compare_rhmst() {
    run_one("rhmst.mod");
}

/// Dump the first ~30 rows of pattern 1 for either fixture so we can
/// see what's at the divergence locus (per the prior INVESTIGATION_SCRAMBLED.md
/// notes: "rhmst rows 28-33"). No assertions — diagnostic only.
fn dump_pattern_rows(name: &str, pattern_idx: usize, row_lo: usize, row_hi: usize) {
    let path = match cache_path(name) {
        Some(p) => p,
        None => {
            eprintln!("[dump_pattern_rows] SKIP: fixture {name} not cached");
            return;
        }
    };
    let bytes = fs::read(&path).expect("read fixture");
    let header = oxideav_mod::header::parse_header(&bytes).expect("header");
    let patterns = oxideav_mod::player::parse_patterns(&header, &bytes);
    if pattern_idx >= patterns.len() {
        eprintln!("[dump_pattern_rows] pattern {pattern_idx} OOR");
        return;
    }
    eprintln!(
        "[dump_pattern_rows:{name}] title={:?} ch={} song_len={} n_pat={}",
        header.title, header.channels, header.song_length, header.n_patterns
    );
    eprintln!(
        "[dump_pattern_rows:{name}] order list: {:?}",
        &header.order[..header.song_length as usize]
    );
    for (i, s) in header.samples.iter().enumerate() {
        if s.length > 0 {
            eprintln!(
                "  sample {}: {:?} len={} loop=({},{}) vol={} ft={}",
                i + 1,
                s.name,
                s.length,
                s.repeat_start,
                s.repeat_length,
                s.volume,
                s.finetune
            );
        }
    }
    eprintln!("[dump_pattern_rows:{name}] pattern {pattern_idx} rows {row_lo}..{row_hi}:");
    let pat = &patterns[pattern_idx];
    let nch = header.channels as usize;
    for r in row_lo..row_hi.min(64) {
        let row = &pat.rows[r];
        let mut s = format!("  row {:02}: ", r);
        for (c, n) in row.iter().take(nch).enumerate() {
            s += &format!(
                "[ch{}: per={:>3} smp={} eff={:X}{:02X}] ",
                c, n.period, n.sample, n.effect, n.effect_param
            );
        }
        eprintln!("{}", s);
    }
}

#[test]
#[ignore = "diagnostic dump; opt-in via --ignored"]
fn dump_rhmst_pattern_1() {
    dump_pattern_rows("rhmst.mod", 1, 25, 40);
}

#[test]
#[ignore = "diagnostic dump; opt-in via --ignored"]
fn dump_halluc_pattern_5() {
    dump_pattern_rows("halluc.mod", 5, 0, 64);
}

#[test]
#[ignore = "diagnostic dump; opt-in via --ignored"]
fn dump_halluc_pattern_0() {
    dump_pattern_rows("halluc.mod", 0, 0, 64);
}

/// Confirm gain calibration on a *single-channel single-sample* MOD —
/// the simplest possible signal. If both engines agree on a single
/// hard-pan voice's peak / RMS, then any divergence on real-world
/// fixtures is in voice mixing or pan, not in the basic per-voice
/// playback.
#[test]
#[ignore = "requires libmodplug runtime"]
fn libmodplug_calibration_single_channel_loud_voice() {
    let lib = match ModPlugLib::try_open() {
        Some(l) => l,
        None => {
            eprintln!("[libmodplug_calibration] SKIP: libmodplug not found");
            return;
        }
    };
    // Simple 4-channel MOD: trigger sample 1 on channel 0 only at row 0,
    // period 428 (C-2). Sample 1 is a 32-frame square wave amplitude 100
    // (i8). Header sets the sample volume to max (64).
    let mut bytes = vec![0u8; 1084];
    bytes[0..4].copy_from_slice(b"cal1");
    bytes[20 + 22..20 + 24].copy_from_slice(&16u16.to_be_bytes());
    bytes[20 + 24] = 0;
    bytes[20 + 25] = 64; // max volume
    bytes[20 + 26..20 + 28].copy_from_slice(&0u16.to_be_bytes());
    bytes[20 + 28..20 + 30].copy_from_slice(&16u16.to_be_bytes()); // full-loop
    bytes[950] = 1;
    bytes[951] = 0x7F;
    bytes[952] = 0;
    bytes[1080..1084].copy_from_slice(b"M.K.");
    let mut pat = vec![0u8; 64 * 4 * 4];
    let period: u16 = 428;
    let p_hi = ((period >> 8) & 0x0F) as u8;
    let p_lo = (period & 0xFF) as u8;
    pat[0] = p_hi;
    pat[1] = p_lo;
    pat[2] = 1u8 << 4;
    pat[3] = 0;
    bytes.extend(pat);
    for i in 0..32 {
        let v: i8 = if i < 16 { 100 } else { -100 };
        bytes.push(v as u8);
    }
    let n_frames = 22_050usize;
    // For the calibration we want to bypass the LED filter, the
    // per-trigger ramp, and run with full hard pan = 1.0 to mirror
    // libmodplug's no-mixer-colouration setup. Build the player by hand.
    use oxideav_mod::header::parse_header;
    use oxideav_mod::player::{parse_patterns, PlayerState};
    use oxideav_mod::samples::extract_samples;
    let header = parse_header(&bytes).expect("header");
    let samples = extract_samples(&header, &bytes);
    let patterns = parse_patterns(&header, &bytes);
    let mut player = PlayerState::new(&header, samples, patterns, OUTPUT_SAMPLE_RATE);
    // Match libmodplug's stereo_separation = 128 = 0.5.
    player.set_pan_separation(0.5);
    let mut our_pcm = vec![0i16; n_frames * 2];
    let n = player.render(&mut our_pcm);
    our_pcm.truncate(n * 2);

    // Probe libmodplug at multiple stereo_separation values to see its
    // pan-formula behaviour.
    for &sep in &[64i32, 128, 192, 256] {
        unsafe {
            let mut s = ModPlugSettings::default();
            (lib.get_settings)(&mut s);
            s.mFlags &= !(MODPLUG_ENABLE_OVERSAMPLING
                | MODPLUG_ENABLE_NOISE_REDUCTION
                | MODPLUG_ENABLE_REVERB
                | MODPLUG_ENABLE_MEGABASS
                | MODPLUG_ENABLE_SURROUND);
            s.mChannels = 2;
            s.mBits = 16;
            s.mFrequency = OUTPUT_SAMPLE_RATE as i32;
            s.mResamplingMode = MODPLUG_RESAMPLE_LINEAR;
            s.mStereoSeparation = sep;
            s.mMaxMixChannels = 64;
            s.mLoopCount = 0;
            (lib.set_settings)(&s);
        }
        let mp_file2 = unsafe { (lib.load)(bytes.as_ptr() as *const c_void, bytes.len() as i32) };
        if mp_file2.is_null() {
            continue;
        }
        let mut mp_buf2 = vec![0u8; n_frames * 2 * 2];
        let mp_n2 = unsafe {
            (lib.read)(
                mp_file2,
                mp_buf2.as_mut_ptr() as *mut c_void,
                mp_buf2.len() as i32,
            )
        };
        let mp_pcm2: Vec<i16> = mp_buf2[..(mp_n2 as usize)]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        unsafe { (lib.unload)(mp_file2) };
        let l: Vec<i16> = mp_pcm2[2_000..].chunks_exact(2).map(|c| c[0]).collect();
        let r: Vec<i16> = mp_pcm2[2_000..].chunks_exact(2).map(|c| c[1]).collect();
        eprintln!(
            "[libmodplug_calibration] sep={sep:>3}  mp L peak={} rms={:.0} | R peak={} rms={:.0}",
            peak_abs_i16(&l),
            rms_i16(&l),
            peak_abs_i16(&r),
            rms_i16(&r),
        );
    }
    lib.configure_clean();
    let mp_file = unsafe { (lib.load)(bytes.as_ptr() as *const c_void, bytes.len() as i32) };
    if mp_file.is_null() {
        eprintln!("[libmodplug_calibration] ModPlug_Load NULL — SKIP");
        return;
    }
    lib.configure_clean();
    // Probe libmodplug's master volume default + try a +1.5x.
    let default_mv = unsafe { (lib.get_master_volume)(mp_file) };
    eprintln!("[libmodplug_calibration] mp default master volume = {default_mv}");
    let mut mp_buf = vec![0u8; n_frames * 2 * 2];
    let mp_n = unsafe {
        (lib.read)(
            mp_file,
            mp_buf.as_mut_ptr() as *mut c_void,
            mp_buf.len() as i32,
        )
    };
    let mp_pcm: Vec<i16> = mp_buf[..(mp_n as usize)]
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    unsafe { (lib.unload)(mp_file) };

    // Skip the first 1000 frames to settle ramps.
    let our_settled = &our_pcm[2_000..];
    let mp_settled = &mp_pcm[2_000..];
    let our_peak = peak_abs_i16(our_settled);
    let mp_peak = peak_abs_i16(mp_settled);
    let our_rms = rms_i16(our_settled);
    let mp_rms = rms_i16(mp_settled);
    let peak_ratio = our_peak as f64 / mp_peak.max(1) as f64;
    let rms_ratio = our_rms / mp_rms.max(1.0);
    eprintln!(
        "[libmodplug_calibration] 1-channel ch0 (LEFT) hard-loop: \
         ours peak={our_peak} rms={:.0} | mp peak={mp_peak} rms={:.0}\n  \
         peak ratio (ours/mp) = {:.3}x | rms ratio = {:.3}x",
        our_rms, mp_rms, peak_ratio, rms_ratio,
    );
    // After the round-19 headroom fix (`n_ch/2 + 1` divisor in the
    // mix bus, not `n_ch/2`) the calibration ratio collapses from
    // ~1.5x to ~1.0x. Pin it loosely so a future mixer change that
    // re-introduces gain inflation gets caught here. The threshold is
    // set generously (0.85..1.15x) because libmodplug's tiny
    // implementation differences (interpolation, ramping) leave
    // residual content divergence that can swing per-window stats by
    // a few percent even after a perfect gain match.
    assert!(
        (0.85..=1.15).contains(&peak_ratio),
        "peak gain calibration drifted: ratio={peak_ratio:.3}x \
         (expected ~1.0x post round-19 headroom fix)"
    );
    assert!(
        (0.85..=1.15).contains(&rms_ratio),
        "rms gain calibration drifted: ratio={rms_ratio:.3}x \
         (expected ~1.0x post round-19 headroom fix)"
    );
}

/// Finest-grained probe: render exactly 1 frame at a time and print
/// our internal `(row, tick, tick_sample_cursor)` after every frame
/// until first row transition. Diagnoses the off-by-N bug where rows
/// transition at sample N*7040 instead of N*7056 in our trace.
#[test]
#[ignore = "diagnostic; opt-in via --ignored"]
fn frame_by_frame_row_transition_probe() {
    let path = match cache_path("halluc.mod") {
        Some(p) => p,
        None => {
            eprintln!("[frame_probe] SKIP: halluc.mod not cached");
            return;
        }
    };
    let bytes = fs::read(&path).expect("read");
    let mut ours = OurStepper::new(&bytes);

    let mut buf = [0i16; 2];
    let mut prev_row = -1i32;
    let mut prev_tick = 255u8;
    eprintln!(
        "[frame_probe] sample_idx, row, tick, tick_cursor, speed, bpm; logging row + tick changes only:"
    );
    for sample in 0..14_000usize {
        let _ = ours.render(&mut buf);
        let row = ours.row();
        let tick = ours.tick();
        if row != prev_row || tick != prev_tick {
            eprintln!(
                "  after sample={:>6} row={:>2} tick={} cursor={:>4} sp={} bpm={}",
                sample + 1,
                row,
                tick,
                ours.tick_cursor(),
                ours.speed(),
                ours.tempo()
            );
            prev_row = row;
            prev_tick = tick;
        }
    }
}

/// Find the first sample at which each engine transitions from row R to
/// row R+1 (or to a new pattern), at high time resolution. If our and
/// libmodplug's row-transition samples diverge, the per-tick
/// accumulator math is the bug. 1-frame chunks are slow but precise.
#[test]
#[ignore = "requires libmodplug runtime; opt-in via cargo test --ignored"]
fn libmodplug_row_transition_alignment() {
    let lib = match ModPlugLib::try_open() {
        Some(l) => l,
        None => {
            eprintln!("[row_align] SKIP: libmodplug not found");
            return;
        }
    };
    let path = match cache_path("halluc.mod") {
        Some(p) => p,
        None => {
            eprintln!("[row_align] SKIP: halluc.mod not cached");
            return;
        }
    };
    let bytes = fs::read(&path).expect("read fixture");

    lib.configure_clean();
    let mp_file = unsafe { (lib.load)(bytes.as_ptr() as *const c_void, bytes.len() as i32) };
    if mp_file.is_null() {
        eprintln!("[row_align] mp load NULL — SKIP");
        return;
    }
    lib.configure_clean();

    let mut ours = OurStepper::new(&bytes);
    let total_frames = OUTPUT_SAMPLE_RATE as usize * 12; // ~12 s — enough to span sec 10 transition
                                                         // Render one frame at a time on each engine and watch for row
                                                         // transitions.
    let mut our_buf = [0i16; 64 * 2];
    let mut mp_buf = [0u8; 64 * 4];
    let mut prev_our = (-1i32, -1i32, -1i32); // (order, pat, row)
    let mut prev_mp = (-1i32, -1i32, -1i32);
    let chunk_frames = 64usize;
    let mut sample_idx = 0usize;
    while sample_idx < total_frames {
        let _ = ours.render(&mut our_buf);
        let _ = unsafe {
            (lib.read)(
                mp_file,
                mp_buf.as_mut_ptr() as *mut c_void,
                mp_buf.len() as i32,
            )
        };
        let our_state = (ours.order(), ours.pattern(), ours.row());
        let mp_state = (
            unsafe { (lib.get_order)(mp_file) },
            unsafe { (lib.get_pattern)(mp_file) },
            unsafe { (lib.get_row)(mp_file) },
        );
        if our_state != prev_our || mp_state != prev_mp {
            // Either changed — log it.
            let our_changed = our_state != prev_our;
            let mp_changed = mp_state != prev_mp;
            if our_changed && mp_changed {
                eprintln!(
                    "  sample={:>7} t={:.4}s  our={:?} mp={:?}",
                    sample_idx,
                    sample_idx as f64 / OUTPUT_SAMPLE_RATE as f64,
                    our_state,
                    mp_state,
                );
            } else if our_changed {
                eprintln!(
                    "  sample={:>7} t={:.4}s  OUR-> {:?}    (mp still {:?})",
                    sample_idx,
                    sample_idx as f64 / OUTPUT_SAMPLE_RATE as f64,
                    our_state,
                    mp_state,
                );
            } else {
                eprintln!(
                    "  sample={:>7} t={:.4}s  (our still {:?})  MP-> {:?}",
                    sample_idx,
                    sample_idx as f64 / OUTPUT_SAMPLE_RATE as f64,
                    our_state,
                    mp_state,
                );
            }
            prev_our = our_state;
            prev_mp = mp_state;
        }
        sample_idx += chunk_frames;
    }
    unsafe { (lib.unload)(mp_file) };
}

/// Synthetic regression for the round-19 headroom calibration. Runs
/// without libmodplug — uses our decoder directly and asserts the
/// peak amplitude on a single-channel max-volume hard-loop matches the
/// libmodplug-derived target value within a small margin. If the
/// `n_ch/2 + 1` divisor in `PlayerState::sample_all_channels` is ever
/// reverted to the legacy `n_ch/2`, ours peak rises to ~9600 and this
/// assert fires.
#[test]
fn headroom_calibration_pin_no_libmodplug_required() {
    // Build the same single-channel single-sample MOD used by the
    // libmodplug calibration test.
    let mut bytes = vec![0u8; 1084];
    bytes[0..4].copy_from_slice(b"calp");
    bytes[20 + 22..20 + 24].copy_from_slice(&16u16.to_be_bytes());
    bytes[20 + 24] = 0;
    bytes[20 + 25] = 64;
    bytes[20 + 26..20 + 28].copy_from_slice(&0u16.to_be_bytes());
    bytes[20 + 28..20 + 30].copy_from_slice(&16u16.to_be_bytes());
    bytes[950] = 1;
    bytes[951] = 0x7F;
    bytes[952] = 0;
    bytes[1080..1084].copy_from_slice(b"M.K.");
    let mut pat = vec![0u8; 64 * 4 * 4];
    let period: u16 = 428;
    let p_hi = ((period >> 8) & 0x0F) as u8;
    let p_lo = (period & 0xFF) as u8;
    pat[0] = p_hi;
    pat[1] = p_lo;
    pat[2] = 1u8 << 4;
    pat[3] = 0;
    bytes.extend(pat);
    for i in 0..32 {
        let v: i8 = if i < 16 { 100 } else { -100 };
        bytes.push(v as u8);
    }

    use oxideav_mod::header::parse_header;
    use oxideav_mod::player::{parse_patterns, PlayerState};
    use oxideav_mod::samples::extract_samples;
    let header = parse_header(&bytes).expect("header");
    let samples = extract_samples(&header, &bytes);
    let patterns = parse_patterns(&header, &bytes);
    let mut player = PlayerState::new(&header, samples, patterns, OUTPUT_SAMPLE_RATE);
    player.set_pan_separation(0.5);
    let mut pcm = vec![0i16; 22_050 * 2];
    let n = player.render(&mut pcm);
    pcm.truncate(n * 2);
    // Skip the LED-filter settling window.
    let settled = &pcm[2_000..];
    let peak = peak_abs_i16(settled);

    // The libmodplug-derived target: 6375 (mp peak) ± a small margin.
    // Pre-fix ours measured 9599 (1.506x mp). Post-fix ours measures
    // ~6399 (1.004x mp). Allow a generous ±15 % window so the pin
    // catches a regression to the legacy divisor (which would land
    // at 9599) without flagging tiny per-render variations.
    assert!(
        (5_500..=7_500).contains(&peak),
        "single-channel headroom calibration drifted: peak={peak} \
         (expected ~6400 post round-19 fix; pre-fix value ~9600 indicates \
         the `n_ch/2 + 1` divisor in PlayerState::sample_all_channels \
         was reverted to `n_ch/2`)"
    );
}

/// Cheap baseline: confirms our `decode_ours` plumbing path is healthy
/// even when no libmodplug is available (and so the comparator can't
/// run). This test never skips and protects the comparator's plumbing
/// against a CI host where neither libmodplug nor the cached fixtures
/// exist.
#[test]
fn decode_ours_path_is_alive() {
    // Build a minimal MOD via the same shape `realworld_harness` uses.
    let mut bytes = vec![0u8; 1084];
    bytes[0..4].copy_from_slice(b"alve");
    // Sample 1: 32 frames (16 words), full-length loop, vol 64.
    bytes[20 + 22..20 + 24].copy_from_slice(&16u16.to_be_bytes());
    bytes[20 + 24] = 0;
    bytes[20 + 25] = 64;
    bytes[20 + 26..20 + 28].copy_from_slice(&0u16.to_be_bytes());
    bytes[20 + 28..20 + 30].copy_from_slice(&16u16.to_be_bytes());
    // Song length 1 pattern.
    bytes[950] = 1;
    bytes[951] = 0x7F;
    bytes[952] = 0;
    bytes[1080..1084].copy_from_slice(b"M.K.");
    // Pattern 0: trigger sample 1 at row 0 channel 0, period 428.
    let mut pat = vec![0u8; 64 * 4 * 4];
    let period: u16 = 428;
    let p_hi = ((period >> 8) & 0x0F) as u8;
    let p_lo = (period & 0xFF) as u8;
    pat[0] = p_hi; // sample hi nibble = 0
    pat[1] = p_lo;
    pat[2] = 1u8 << 4; // sample lo nibble = 1, effect 0
    pat[3] = 0;
    bytes.extend(pat);
    // Sample body: square wave.
    for i in 0..32 {
        let v: i8 = if i < 16 { 100 } else { -100 };
        bytes.push(v as u8);
    }
    let pcm = decode_ours(bytes, 22_050);
    assert!(!pcm.is_empty(), "decode_ours produced no audio");
}
