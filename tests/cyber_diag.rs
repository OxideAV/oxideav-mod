//! Diagnostic harness for cyber.mod (4-channel ProTracker, 74304 bytes).
//!
//! User report: "I feel some of the effects are a bit off around 12s~14s."
//! This test renders cyber.mod via our decoder and side-by-side with
//! a trace-reference-impl render (black-box, runtime-loaded), localising
//! to the exact (order, pattern, row, tick, channel) where divergence
//! occurs.
//!
//! Opt-in via `--ignored`. Requires:
//!   - cyber.mod cached at $CARGO_TARGET_DIR/test-fixtures/cyber.mod
//!     (or $CARGO_MANIFEST_DIR/../../target/test-fixtures/cyber.mod)
//!   - a reference dylib reachable via the OXIDEAV_TRACKER_REF_PATH or
//!     legacy LIBMODPLUG_PATH env var, or installed at the well-known
//!     brew-cellar location.

use std::ffi::c_void;
use std::fs;
use std::path::PathBuf;

use libloading::{Library, Symbol};

use oxideav_mod::container::OUTPUT_SAMPLE_RATE;
use oxideav_mod::header::parse_header;
use oxideav_mod::player::{parse_patterns, PlayerState};
use oxideav_mod::samples::extract_samples;

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

const MODPLUG_ENABLE_OVERSAMPLING: i32 = 1 << 0;
const MODPLUG_ENABLE_NOISE_REDUCTION: i32 = 1 << 1;
const MODPLUG_ENABLE_REVERB: i32 = 1 << 2;
const MODPLUG_ENABLE_MEGABASS: i32 = 1 << 3;
const MODPLUG_ENABLE_SURROUND: i32 = 1 << 4;
const MODPLUG_RESAMPLE_LINEAR: i32 = 1;

type ModPlugFile = c_void;

struct ModPlugLib {
    _lib: Library,
    load: unsafe extern "C" fn(*const c_void, i32) -> *mut ModPlugFile,
    unload: unsafe extern "C" fn(*mut ModPlugFile),
    read: unsafe extern "C" fn(*mut ModPlugFile, *mut c_void, i32) -> i32,
    get_order: unsafe extern "C" fn(*mut ModPlugFile) -> i32,
    get_pattern: unsafe extern "C" fn(*mut ModPlugFile) -> i32,
    get_row: unsafe extern "C" fn(*mut ModPlugFile) -> i32,
    get_speed: unsafe extern "C" fn(*mut ModPlugFile) -> i32,
    /// Available for diagnostic tests that want tempo (BPM) traces;
    /// the headline cyber_diag harness only logs (order, pat, row, tick).
    #[allow(dead_code)]
    get_tempo: unsafe extern "C" fn(*mut ModPlugFile) -> i32,
    get_settings: unsafe extern "C" fn(*mut ModPlugSettings),
    set_settings: unsafe extern "C" fn(*const ModPlugSettings),
}

impl ModPlugLib {
    fn try_open() -> Option<Self> {
        // The literal dylib filenames + brew-cellar path components
        // below are the on-disk identity of the published-ABI black-box
        // binary this test dlopens. They are not citations to source
        // code — no source is consulted.
        let mut paths: Vec<PathBuf> = Vec::new();
        if let Ok(p) = std::env::var("OXIDEAV_TRACKER_REF_PATH") {
            paths.push(PathBuf::from(p));
        }
        if let Ok(p) = std::env::var("LIBMODPLUG_PATH") {
            paths.push(PathBuf::from(p));
        }
        paths.push(PathBuf::from(
            "/opt/homebrew/Cellar/libmodplug/0.8.9.0/lib/libmodplug.dylib",
        ));
        if let Ok(entries) = fs::read_dir("/opt/homebrew/Cellar/libmodplug/") {
            for e in entries.flatten() {
                let p = e.path().join("lib/libmodplug.dylib");
                if p.exists() {
                    paths.push(p);
                }
            }
        }
        paths.push(PathBuf::from("/opt/homebrew/lib/libmodplug.dylib"));
        paths.push(PathBuf::from("/usr/local/lib/libmodplug.dylib"));
        paths.push(PathBuf::from("libmodplug.dylib"));
        let lib = paths.iter().find_map(|p| unsafe { Library::new(p) }.ok())?;
        unsafe {
            macro_rules! sym {
                ($n:expr, $t:ty) => {{
                    let s: Symbol<$t> = lib.get($n).ok()?;
                    *s
                }};
            }
            let load = sym!(
                b"ModPlug_Load\0",
                unsafe extern "C" fn(*const c_void, i32) -> *mut ModPlugFile
            );
            let unload = sym!(b"ModPlug_Unload\0", unsafe extern "C" fn(*mut ModPlugFile));
            let read = sym!(
                b"ModPlug_Read\0",
                unsafe extern "C" fn(*mut ModPlugFile, *mut c_void, i32) -> i32
            );
            let get_order = sym!(
                b"ModPlug_GetCurrentOrder\0",
                unsafe extern "C" fn(*mut ModPlugFile) -> i32
            );
            let get_pattern = sym!(
                b"ModPlug_GetCurrentPattern\0",
                unsafe extern "C" fn(*mut ModPlugFile) -> i32
            );
            let get_row = sym!(
                b"ModPlug_GetCurrentRow\0",
                unsafe extern "C" fn(*mut ModPlugFile) -> i32
            );
            let get_speed = sym!(
                b"ModPlug_GetCurrentSpeed\0",
                unsafe extern "C" fn(*mut ModPlugFile) -> i32
            );
            let get_tempo = sym!(
                b"ModPlug_GetCurrentTempo\0",
                unsafe extern "C" fn(*mut ModPlugFile) -> i32
            );
            let get_settings = sym!(
                b"ModPlug_GetSettings\0",
                unsafe extern "C" fn(*mut ModPlugSettings)
            );
            let set_settings = sym!(
                b"ModPlug_SetSettings\0",
                unsafe extern "C" fn(*const ModPlugSettings)
            );
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
            })
        }
    }

    fn configure_clean(&self) {
        unsafe {
            let mut s = ModPlugSettings::default();
            (self.get_settings)(&mut s);
            s.mFlags &= !(MODPLUG_ENABLE_OVERSAMPLING
                | MODPLUG_ENABLE_NOISE_REDUCTION
                | MODPLUG_ENABLE_REVERB
                | MODPLUG_ENABLE_MEGABASS
                | MODPLUG_ENABLE_SURROUND);
            s.mChannels = 2;
            s.mBits = 16;
            s.mFrequency = OUTPUT_SAMPLE_RATE as i32;
            s.mResamplingMode = MODPLUG_RESAMPLE_LINEAR;
            s.mStereoSeparation = 128;
            s.mMaxMixChannels = 64;
            s.mLoopCount = 0;
            (self.set_settings)(&s);
        }
    }
}

fn write_wav_s16(
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
    buf.extend_from_slice(&1u16.to_le_bytes());
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

fn cache_path() -> Option<PathBuf> {
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let crate_dir = std::env::var("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .expect("CARGO_MANIFEST_DIR set during cargo test");
            crate_dir.join("..").join("..").join("target")
        });
    let p = target_dir.join("test-fixtures").join("cyber.mod");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

#[test]
#[ignore = "requires cached cyber.mod + reference dylib; opt-in via --ignored"]
fn cyber_diag_full_trace() {
    let lib = match ModPlugLib::try_open() {
        Some(l) => l,
        None => {
            eprintln!("[cyber_diag] SKIP: reference dylib not found");
            return;
        }
    };
    let path = match cache_path() {
        Some(p) => p,
        None => {
            eprintln!("[cyber_diag] SKIP: cyber.mod not cached");
            return;
        }
    };
    let bytes = fs::read(&path).expect("read");
    println!(
        "[cyber_diag] file: {} bytes, sha already verified at fixture provisioning",
        bytes.len()
    );

    let header = parse_header(&bytes).expect("parse header");
    println!(
        "title={:?} sig={:?} ch={} song_len={} n_pat={}",
        header.title,
        std::str::from_utf8(&header.signature).unwrap_or("?"),
        header.channels,
        header.song_length,
        header.n_patterns
    );
    let order_used: Vec<u8> = header.order[..header.song_length as usize].to_vec();
    println!("order list: {:?}", order_used);

    for (i, s) in header.samples.iter().enumerate() {
        if s.length > 0 {
            println!(
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

    // Load both engines.
    lib.configure_clean();
    let mp_file = unsafe { (lib.load)(bytes.as_ptr() as *const c_void, bytes.len() as i32) };
    if mp_file.is_null() {
        eprintln!("[cyber_diag] mp load NULL — SKIP");
        return;
    }
    lib.configure_clean();

    let samples = extract_samples(&header, &bytes);
    let patterns = parse_patterns(&header, &bytes);
    let mut player = PlayerState::new(&header, samples, patterns.clone(), OUTPUT_SAMPLE_RATE);

    // Render in 256-frame chunks for ~16 seconds (cover 11-15s window).
    let total_secs = 16usize;
    let total_frames = OUTPUT_SAMPLE_RATE as usize * total_secs;
    let chunk = 256usize;

    let mut our_pcm: Vec<i16> = Vec::with_capacity(total_frames * 2);
    let mut mp_pcm: Vec<i16> = Vec::with_capacity(total_frames * 2);
    let mut our_buf = vec![0i16; chunk * 2];
    let mut mp_buf = vec![0u8; chunk * 4];

    // Per-chunk trace: (sample_idx, our_order, our_pattern, our_row,
    // our_tick, mp_order, mp_pattern, mp_row, mp_speed).
    type StateRow = (usize, i32, i32, i32, i32, i32, i32, i32, i32);
    let mut state_log: Vec<StateRow> = Vec::new();

    let mut sample_idx = 0usize;
    while sample_idx < total_frames {
        let want = chunk.min(total_frames - sample_idx);
        let n = player.render(&mut our_buf[..want * 2]);
        if n == 0 {
            break;
        }
        let mp_n = unsafe {
            (lib.read)(
                mp_file,
                mp_buf.as_mut_ptr() as *mut c_void,
                (want * 4) as i32,
            )
        };
        if mp_n <= 0 {
            break;
        }
        let mp_n = mp_n as usize;
        our_pcm.extend_from_slice(&our_buf[..n * 2]);
        for c in mp_buf[..mp_n].chunks_exact(2) {
            mp_pcm.push(i16::from_le_bytes([c[0], c[1]]));
        }
        let our_order = player.order_index as i32;
        let our_pat = player
            .order
            .get(player.order_index as usize)
            .copied()
            .unwrap_or(0) as i32;
        let our_row = player.row as i32;
        let our_tick = player.tick as i32;
        let mp_order = unsafe { (lib.get_order)(mp_file) };
        let mp_pat = unsafe { (lib.get_pattern)(mp_file) };
        let mp_row = unsafe { (lib.get_row)(mp_file) };
        let mp_speed = unsafe { (lib.get_speed)(mp_file) };
        state_log.push((
            sample_idx, our_order, our_pat, our_row, our_tick, mp_order, mp_pat, mp_row, mp_speed,
        ));
        sample_idx += n.max(mp_n / 4);
    }
    unsafe { (lib.unload)(mp_file) };

    // Find first (order, row) divergence.
    let mut first_div: Option<usize> = None;
    for (i, e) in state_log.iter().enumerate() {
        if e.1 != e.5 || e.3 != e.7 {
            first_div = Some(i);
            break;
        }
    }
    if let Some(i) = first_div {
        let e = state_log[i];
        println!(
            "[cyber_diag] FIRST (order,row) DIVERGENCE @ sample {} t={:.3}s | ours O{}P{}R{:02}T{} | mp O{}P{}R{:02} sp={}",
            e.0,
            e.0 as f64 / OUTPUT_SAMPLE_RATE as f64,
            e.1, e.2, e.3, e.4, e.5, e.6, e.7, e.8
        );
    } else {
        println!("[cyber_diag] (order,row) AGREES across the trace");
    }

    // Per-second window comparison: max |diff|, mean |diff|, also for the
    // 11-15s "complaint" window in 250 ms slices.
    let n = our_pcm.len().min(mp_pcm.len());
    let sr = OUTPUT_SAMPLE_RATE as usize;

    println!("[cyber_diag] per-second sample-level diff (whole-trace):");
    for sec in 0..(n / 2 / sr) {
        let s = sec * sr * 2;
        let e = (s + sr * 2).min(n);
        let mut max_d: i32 = 0;
        let mut sum_d: u64 = 0;
        let mut cnt: u64 = 0;
        let mut our_rms: f64 = 0.0;
        let mut mp_rms: f64 = 0.0;
        for i in s..e {
            let d = (our_pcm[i] as i32 - mp_pcm[i] as i32).abs();
            if d > max_d {
                max_d = d;
            }
            sum_d += d as u64;
            cnt += 1;
            our_rms += (our_pcm[i] as f64).powi(2);
            mp_rms += (mp_pcm[i] as f64).powi(2);
        }
        let mean = sum_d as f64 / cnt as f64;
        our_rms = (our_rms / cnt as f64).sqrt();
        mp_rms = (mp_rms / cnt as f64).sqrt();
        // find row state at this t
        let target = sec * sr;
        let st = state_log
            .iter()
            .rev()
            .find(|e| e.0 <= target)
            .copied()
            .unwrap_or((0, -1, -1, -1, -1, -1, -1, -1, -1));
        println!(
            "  t={:>2}s  max|d|={:>5} mean|d|={:>5.0} our_rms={:>5.0} mp_rms={:>5.0}  ours O{}P{}R{:02}  mp O{}P{}R{:02}",
            sec, max_d, mean, our_rms, mp_rms,
            st.1, st.2, st.3, st.5, st.6, st.7
        );
    }

    // Optionally dump WAVs.
    if std::env::var("CYBER_DUMP_WAV").is_ok() {
        let dump_dir = PathBuf::from("/tmp/cargo-target-mod");
        let _ = fs::create_dir_all(&dump_dir);
        write_wav_s16(
            &dump_dir.join("cyber-ours.wav"),
            &our_pcm[..n],
            OUTPUT_SAMPLE_RATE,
            2,
        )
        .expect("ours wav");
        write_wav_s16(
            &dump_dir.join("cyber-mp.wav"),
            &mp_pcm[..n],
            OUTPUT_SAMPLE_RATE,
            2,
        )
        .expect("mp wav");
        println!("[cyber_diag] dumped: /tmp/cargo-target-mod/cyber-ours.wav and cyber-mp.wav");
    }

    // ---- Render in sub-tick steps to dump channel-2 period at every tick ----
    // We need to look at rows 32-58 of pattern 1. At speed 6, that's 27 rows × 6
    // ticks = 162 ticks. Render 162 ticks worth of samples one tick at a time
    // and report ch2.period right after each tick fires.
    {
        let header2 = parse_header(&bytes).expect("parse header");
        let samples2 = extract_samples(&header2, &bytes);
        let patterns2 = parse_patterns(&header2, &bytes);
        let mut player2 = PlayerState::new(&header2, samples2, patterns2, OUTPUT_SAMPLE_RATE);
        // Render until row 32 of pattern 1.
        // Each tick is ~882 samples at default speed/BPM; render in 882-sample chunks
        // so we can read per-tick state.
        let mut buf = vec![0i16; 882 * 2];
        let mut prev_tick = 0u8;
        let mut prev_row = 0u8;
        let mut prev_ord = 0u8;
        println!("[cyber_diag] per-tick ch2 trace (showing only rows 28-50 in pattern 1):");
        for _ in 0..900 {
            let _ = player2.render(&mut buf);
            let now_tick = player2.tick;
            let now_row = player2.row;
            let now_ord = player2.order_index;
            // log when we've just transitioned to a new tick
            if (now_tick != prev_tick || now_row != prev_row || now_ord != prev_ord)
                && now_ord == 1
                && (28..=50).contains(&now_row)
            {
                let ch2 = &player2.channels[2];
                println!(
                    "  O{}R{:02}T{} ch2: period={} sample={} active={} eff={:X}{:02X} arp_base={} vol={}",
                    now_ord, now_row, now_tick,
                    ch2.period, ch2.sample_index, ch2.active,
                    ch2.effect, ch2.effect_param, ch2.arp_base_period, ch2.volume,
                );
            }
            prev_tick = now_tick;
            prev_row = now_row;
            prev_ord = now_ord;
            if player2.ended || (now_ord >= 2) {
                break;
            }
        }
    }

    // ---- Surface effect-0 arpeggio cells in patterns played in 11-15s window ----
    for pat_idx in [0u8, 1, 2] {
        if (pat_idx as usize) >= patterns.len() {
            continue;
        }
        println!(
            "[cyber_diag] Pattern {} arpeggio cells (effect 0, non-zero param):",
            pat_idx
        );
        let pat_p = &patterns[pat_idx as usize];
        for r in 0..64 {
            for (c, n) in pat_p.rows[r].iter().enumerate() {
                if n.effect == 0 && n.effect_param != 0 {
                    println!(
                        "  pat{} row {:02} ch{}: per={:>3} smp={} arp x={} y={}",
                        pat_idx,
                        r,
                        c,
                        n.period,
                        n.sample,
                        n.effect_param >> 4,
                        n.effect_param & 0xF
                    );
                }
            }
        }
    }
    {
        println!("[cyber_diag] Pattern 1 channel-2 arpeggio rows (effect 0, non-zero param):");
        let pat = &patterns[1];
        for r in 0..64 {
            let n = pat.rows[r][2];
            if n.effect == 0 && n.effect_param != 0 {
                println!(
                    "  pat1 row {:02} ch2: per={:>3} smp={} arp x={} y={}",
                    r,
                    n.period,
                    n.sample,
                    n.effect_param >> 4,
                    n.effect_param & 0xF
                );
            }
        }
    }

    println!("[cyber_diag] 250ms slices around 11-15s:");
    let slice_frames = sr / 4; // 250ms
    for slice_idx in (11 * 4)..(15 * 4) {
        let frame_start = slice_idx * slice_frames;
        let s = frame_start * 2;
        let e = (s + slice_frames * 2).min(n);
        if e <= s {
            break;
        }
        let mut max_d: i32 = 0;
        let mut sum_d: u64 = 0;
        let mut cnt: u64 = 0;
        for i in s..e {
            let d = (our_pcm[i] as i32 - mp_pcm[i] as i32).abs();
            if d > max_d {
                max_d = d;
            }
            sum_d += d as u64;
            cnt += 1;
        }
        let mean = sum_d as f64 / cnt as f64;
        let target = frame_start;
        let st = state_log
            .iter()
            .rev()
            .find(|e| e.0 <= target)
            .copied()
            .unwrap_or((0, -1, -1, -1, -1, -1, -1, -1, -1));
        println!(
            "  t={:.3}s max|d|={:>5} mean|d|={:>5.0} | ours O{}P{}R{:02}T{} | mp O{}P{}R{:02}",
            frame_start as f64 / sr as f64,
            max_d,
            mean,
            st.1,
            st.2,
            st.3,
            st.4,
            st.5,
            st.6,
            st.7
        );
    }

    // Dump pattern rows for orders/patterns played in the 11-15s region.
    let order_at_11s = state_log
        .iter()
        .rev()
        .find(|e| e.0 <= 11 * sr)
        .map(|e| e.1)
        .unwrap_or(0);
    let order_at_15s = state_log
        .iter()
        .rev()
        .find(|e| e.0 <= 15 * sr)
        .map(|e| e.1)
        .unwrap_or(0);
    println!(
        "[cyber_diag] orders played in 11-15s: {}..={}",
        order_at_11s, order_at_15s
    );

    let mut shown_pats: Vec<i32> = Vec::new();
    for o in (order_at_11s as usize)..=(order_at_15s as usize + 1) {
        if o >= header.song_length as usize {
            break;
        }
        let pat_idx = header.order[o] as i32;
        if shown_pats.contains(&pat_idx) {
            continue;
        }
        shown_pats.push(pat_idx);
        if pat_idx as usize >= patterns.len() {
            continue;
        }
        let pat = &patterns[pat_idx as usize];
        let nch = header.channels as usize;
        println!(
            "[cyber_diag] order {} -> pattern {}: rows 0..64",
            o, pat_idx
        );
        for r in 0..64 {
            let row = &pat.rows[r];
            let mut s = format!("  row {:02}: ", r);
            let mut interesting = false;
            for (c, n) in row.iter().take(nch).enumerate() {
                if n.period != 0 || n.sample != 0 || n.effect != 0 || n.effect_param != 0 {
                    interesting = true;
                }
                s += &format!(
                    "[c{} per={:>3} smp={:>2} {:X}{:02X}] ",
                    c, n.period, n.sample, n.effect, n.effect_param
                );
            }
            if interesting {
                println!("{}", s);
            }
        }
    }
}
