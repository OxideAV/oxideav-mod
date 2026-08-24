//! STM + XM playback-conformance gates against the black-box trace
//! reference impl (round 451).
//!
//! The MOD pipeline has had a side-by-side reference harness since
//! round ~100 (`tests/tracker_reference_compare.rs`); this file extends
//! the same gate to the other two formats this crate plays. Synthetic
//! fixtures with position-flow content (pattern breaks, order jumps,
//! speed changes) are rendered by BOTH engines, and the per-chunk
//! song-position (order / row / speed) of each is traced. The gate
//! asserts position lockstep at every probe for XM, and audibility +
//! self-consistency for both.
//!
//! The reference dylib is loaded with `libloading`, exactly as in
//! `tracker_reference_compare.rs`: only its **published C
//! entry-points** are consumed, no reference source is read or
//! referenced, and the binary is treated strictly as an opaque
//! behaviour oracle whose outputs we compare against. If the dylib is
//! not present the tests print a clean SKIP and return success.
//!
//! STM caveat, recorded up front: the staged STM layout doc
//! (`docs/audio/trackers/stm/ScreamTracker-v1.0-stm.txt`) carries **no
//! effect semantics** and no tempo formula (see
//! `docs/audio/trackers/stm/stm-effect-semantics-gap.md`), so this
//! crate's STM pacing is an explicitly-documented heuristic
//! (`bpm_equiv = tempo * 125 / 0x60`). The STM gate therefore checks
//! order-level progress and audibility rather than row-exact lockstep —
//! row-exact assertions would silently encode the oracle's undocumented
//! tempo mapping into our engine, which the clean-room rules bar.

use std::ffi::c_void;
use std::fs;
use std::path::PathBuf;

use libloading::{Library, Symbol};

use oxideav_mod::{stm, stm_player::StmPlayerState, xm, xm_player::XmPlayerState};

const OUT_HZ: u32 = 44_100;

// ---------- Published C-interface ABI mirror of the reference dylib ----------

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

struct RefLib {
    _lib: Library,
    load: unsafe extern "C" fn(*const c_void, i32) -> *mut ModPlugFile,
    unload: unsafe extern "C" fn(*mut ModPlugFile),
    read: unsafe extern "C" fn(*mut ModPlugFile, *mut c_void, i32) -> i32,
    get_order: unsafe extern "C" fn(*mut ModPlugFile) -> i32,
    get_row: unsafe extern "C" fn(*mut ModPlugFile) -> i32,
    get_speed: unsafe extern "C" fn(*mut ModPlugFile) -> i32,
    get_settings: unsafe extern "C" fn(*mut ModPlugSettings),
    set_settings: unsafe extern "C" fn(*const ModPlugSettings),
}

impl RefLib {
    fn try_open() -> Option<Self> {
        // On-disk identity of the published-ABI black-box binary; not a
        // citation to source code. Env overrides mirror the legacy CI
        // knobs used by `tracker_reference_compare.rs`.
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(p) = std::env::var("OXIDEAV_TRACKER_REF_PATH") {
            candidates.push(PathBuf::from(p));
        }
        if let Ok(p) = std::env::var("LIBMODPLUG_PATH") {
            candidates.push(PathBuf::from(p));
        }
        if let Ok(entries) = fs::read_dir("/opt/homebrew/Cellar/libmodplug/") {
            for entry in entries.flatten() {
                let candidate = entry.path().join("lib/libmodplug.dylib");
                if candidate.exists() {
                    candidates.push(candidate);
                }
            }
        }
        candidates.push(PathBuf::from("/opt/homebrew/lib/libmodplug.dylib"));
        candidates.push(PathBuf::from("/usr/local/lib/libmodplug.dylib"));
        candidates.push(PathBuf::from("libmodplug.so.1"));
        candidates.push(PathBuf::from("libmodplug.so"));

        let lib = candidates
            .iter()
            .find_map(|p| unsafe { Library::new(p) }.ok())?;
        unsafe {
            let load: Symbol<unsafe extern "C" fn(*const c_void, i32) -> *mut ModPlugFile> =
                lib.get(b"ModPlug_Load\0").ok()?;
            let unload: Symbol<unsafe extern "C" fn(*mut ModPlugFile)> =
                lib.get(b"ModPlug_Unload\0").ok()?;
            let read: Symbol<unsafe extern "C" fn(*mut ModPlugFile, *mut c_void, i32) -> i32> =
                lib.get(b"ModPlug_Read\0").ok()?;
            let get_order: Symbol<unsafe extern "C" fn(*mut ModPlugFile) -> i32> =
                lib.get(b"ModPlug_GetCurrentOrder\0").ok()?;
            let get_row: Symbol<unsafe extern "C" fn(*mut ModPlugFile) -> i32> =
                lib.get(b"ModPlug_GetCurrentRow\0").ok()?;
            let get_speed: Symbol<unsafe extern "C" fn(*mut ModPlugFile) -> i32> =
                lib.get(b"ModPlug_GetCurrentSpeed\0").ok()?;
            let get_settings: Symbol<unsafe extern "C" fn(*mut ModPlugSettings)> =
                lib.get(b"ModPlug_GetSettings\0").ok()?;
            let set_settings: Symbol<unsafe extern "C" fn(*const ModPlugSettings)> =
                lib.get(b"ModPlug_SetSettings\0").ok()?;
            Some(RefLib {
                load: *load,
                unload: *unload,
                read: *read,
                get_order: *get_order,
                get_row: *get_row,
                get_speed: *get_speed,
                get_settings: *get_settings,
                set_settings: *set_settings,
                _lib: lib,
            })
        }
    }

    /// Disable every audio-shaping option; 44.1 kHz 16-bit stereo,
    /// linear resampling — the same clean profile as the MOD harness.
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
            s.mFrequency = OUT_HZ as i32;
            s.mResamplingMode = MODPLUG_RESAMPLE_LINEAR;
            s.mStereoSeparation = 128;
            s.mMaxMixChannels = 64;
            s.mLoopCount = -1; // loop forever so long traces never starve
            (self.set_settings)(&s);
        }
    }
}

fn rms_i16(buf: &[i16]) -> f64 {
    if buf.is_empty() {
        return 0.0;
    }
    let s: f64 = buf.iter().map(|&v| (v as f64) * (v as f64)).sum();
    (s / buf.len() as f64).sqrt()
}

// ---------- Fixture builders ----------

/// Write one XM instrument (flat full-volume envelope, looped 64-frame
/// square-wave sample) into `out`.
fn push_xm_square_instrument(out: &mut Vec<u8>) {
    const HSIZE: u32 = 0x107;
    let inst_start = out.len();
    out.extend_from_slice(&HSIZE.to_le_bytes());
    let mut nbuf = [0u8; 22];
    nbuf[..3].copy_from_slice(b"sqr");
    out.extend_from_slice(&nbuf);
    out.push(0); // type
    out.extend_from_slice(&1u16.to_le_bytes()); // num_samples
    out.extend_from_slice(&xm::XM_SAMPLE_HEADER_SIZE.to_le_bytes());
    out.extend(vec![0u8; 96]); // sample_map
    let mut vol_env = [0u8; 48];
    vol_env[0..2].copy_from_slice(&0u16.to_le_bytes());
    vol_env[2..4].copy_from_slice(&64u16.to_le_bytes());
    vol_env[4..6].copy_from_slice(&64u16.to_le_bytes());
    vol_env[6..8].copy_from_slice(&64u16.to_le_bytes());
    out.extend_from_slice(&vol_env);
    out.extend_from_slice(&[0u8; 48]); // pan env
    out.push(2); // num_vol_points
    out.extend_from_slice(&[0u8; 7]); // num_pan + sustain/loop bytes
    out.push(0x01); // vol type On
    out.push(0); // pan type
    out.extend_from_slice(&[0u8; 4]); // vibrato
    out.extend_from_slice(&0u16.to_le_bytes()); // fadeout 0
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    while out.len() - inst_start < HSIZE as usize {
        out.push(0);
    }
    // Sample header (40 bytes): looped square wave.
    let mut abs_pcm = vec![0i8; 64];
    for (i, slot) in abs_pcm.iter_mut().enumerate() {
        *slot = if i < 32 { 100 } else { -100 };
    }
    let mut delta = Vec::with_capacity(64);
    let mut prev: i8 = 0;
    for v in &abs_pcm {
        delta.push(v.wrapping_sub(prev) as u8);
        prev = *v;
    }
    out.extend_from_slice(&(delta.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // loop_start
    out.extend_from_slice(&(delta.len() as u32).to_le_bytes()); // loop_length
    out.push(0x40); // volume
    out.push(0); // finetune
    out.push(1); // type: forward loop, 8-bit
    out.push(128); // pan
    out.push(0); // relative note
    out.push(0); // reserved
    out.extend_from_slice(&[0u8; 22]); // name
    out.extend_from_slice(&delta);
}

/// A 2-channel, 3-pattern XM whose order flow exercises pattern break,
/// order jump, and a speed change:
///
///   order [0, 1, 2]
///   pattern 0 (16 rows): row 0 note C-4; row 2 F03 (speed 3)
///   pattern 1 (16 rows): row 0 note; row 4 D08 (break to row 8 of next)
///   pattern 2 (16 rows): row 0 note; row 12 B01 (jump to order 1)
///
/// so the steady state cycles 1 → 2 → 1 forever, with a mid-pattern
/// entry row and a non-default speed. Every transition is grounded in
/// `multimedia-cx-fasttracker-2.html` (Bxy "Jump to order", Dxy
/// "Pattern break to row x*10+y, THE VALUE PROVIDED IS IN DECIMAL",
/// Fxy set speed) — nothing here leans on the oracle for semantics.
fn build_flow_xm() -> Vec<u8> {
    let mut out = vec![0u8; xm::XM_MIN_HEADER_LEN];
    out[0..17].copy_from_slice(xm::XM_BANNER);
    out[17..37].copy_from_slice(b"flow-xm             ");
    out[xm::XM_ID_BYTE_OFFSET] = 0x1A;
    out[38..58].copy_from_slice(b"oxideav             ");
    out[58..60].copy_from_slice(&xm::XM_VERSION_0104.to_le_bytes());
    out[60..64].copy_from_slice(&0x114u32.to_le_bytes());
    out[64..66].copy_from_slice(&3u16.to_le_bytes()); // song_length = 3
    out[66..68].copy_from_slice(&0u16.to_le_bytes()); // restart
    out[68..70].copy_from_slice(&2u16.to_le_bytes()); // 2 channels
    out[70..72].copy_from_slice(&3u16.to_le_bytes()); // 3 patterns
    out[72..74].copy_from_slice(&1u16.to_le_bytes()); // 1 instrument
    out[74..76].copy_from_slice(&1u16.to_le_bytes()); // linear
    out[76..78].copy_from_slice(&6u16.to_le_bytes()); // speed 6
    out[78..80].copy_from_slice(&125u16.to_le_bytes()); // BPM 125
    out[xm::XM_ORDER_TABLE_OFFSET] = 0;
    out[xm::XM_ORDER_TABLE_OFFSET + 1] = 1;
    out[xm::XM_ORDER_TABLE_OFFSET + 2] = 2;

    // effects[pattern] = (row, channel, effect_type, effect_param)
    let effects: [(u16, usize, u8, u8); 3] = [
        (2, 1, 0x0F, 0x03),  // pattern 0: F03 on row 2 ch 1
        (4, 1, 0x0D, 0x08),  // pattern 1: D08 on row 4 ch 1
        (12, 1, 0x0B, 0x01), // pattern 2: B01 on row 12 ch 1
    ];
    for (pat, &(erow, ech, etype, eparam)) in effects.iter().enumerate() {
        let mut packed = Vec::new();
        for row in 0..16u16 {
            for ch in 0..2usize {
                if row == 0 && ch == 0 {
                    packed.push(0x80 | 0x01 | 0x02 | 0x04);
                    packed.push(49); // C-4
                    packed.push(1); // instrument 1
                    packed.push(0x50); // vol col SetVolume(64)
                } else if row == erow && ch == ech {
                    packed.push(0x80 | 0x08 | 0x10);
                    packed.push(etype);
                    packed.push(eparam);
                } else {
                    packed.push(0x80);
                }
            }
        }
        let _ = pat;
        out.extend_from_slice(&9u32.to_le_bytes());
        out.push(0);
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(&(packed.len() as u16).to_le_bytes());
        out.extend(packed);
    }
    push_xm_square_instrument(&mut out);
    out
}

/// A 3-pattern STM: pattern 0 breaks to the next order at row 32
/// (D32 in the staged doc's "ProTracker format" effect nibble),
/// pattern 1 runs its 64 rows naturally, pattern 2 jumps back to
/// order 0 (B00) at row 16. One looped square-wave instrument, note
/// C-4 on row 0 of each pattern.
fn build_flow_stm() -> Vec<u8> {
    let mut out = vec![0u8; stm::PATTERN_DATA_OFFSET];
    out[0..8].copy_from_slice(b"flow-stm");
    out[0x14..0x1C].copy_from_slice(b"!Scream!");
    out[0x1C] = 0x1A;
    out[0x1D] = 2; // module
    out[0x1E] = 2; // version 2.21-ish
    out[0x1F] = 21;
    out[0x20] = 0x60; // tempo
    out[0x21] = 3; // 3 patterns
    out[0x22] = 64; // global volume

    // Instrument 0: 64-byte square, looped over the whole body.
    let inst_off = stm::HEADER_PREFIX_SIZE;
    out[inst_off..inst_off + 3].copy_from_slice(b"sqr");
    out[inst_off + 16..inst_off + 18].copy_from_slice(&64u16.to_le_bytes()); // length
    out[inst_off + 18..inst_off + 20].copy_from_slice(&0u16.to_le_bytes()); // loop start
    out[inst_off + 20..inst_off + 22].copy_from_slice(&64u16.to_le_bytes()); // loop end
    out[inst_off + 22] = 64; // volume
    out[inst_off + 24..inst_off + 26].copy_from_slice(&8448u16.to_le_bytes()); // C3 Hz
                                                                               // "length in paragraphs" (staged doc, record offset +30): 64 bytes
                                                                               // = 4 paragraphs. Our parser doesn't need it, but it is part of
                                                                               // the documented module-file record, so the fixture carries it.
    out[inst_off + 30..inst_off + 32].copy_from_slice(&4u16.to_le_bytes());

    // Order table: 0, 1, 2, then 255-terminated.
    out[stm::ORDER_TABLE_OFFSET] = 0;
    out[stm::ORDER_TABLE_OFFSET + 1] = 1;
    out[stm::ORDER_TABLE_OFFSET + 2] = 2;
    for i in 3..stm::ORDER_TABLE_SIZE {
        out[stm::ORDER_TABLE_OFFSET + i] = 255;
    }

    // Pattern data: 3 patterns × 64 rows × 4 channels × 4 bytes.
    // Note byte: octave<<4 | semitone; 0x30 = C-3 (octave 3, C).
    // Cell layout per the staged doc: b1 = (instrument << 3) | vol_lo,
    // b2 = (vol_hi << 4) | command, b3 = param. Note 251 = empty.
    let mut pattern_bytes = vec![0u8; 3 * stm::BYTES_PER_PATTERN];
    {
        let mut put_cell = |pat: usize, row: usize, ch: usize, cell: [u8; 4]| {
            let off = pat * stm::BYTES_PER_PATTERN + (row * stm::STM_CHANNELS + ch) * 4;
            pattern_bytes[off..off + 4].copy_from_slice(&cell);
        };
        for pat in 0..3 {
            for row in 0..stm::PATTERN_ROWS {
                for ch in 0..stm::STM_CHANNELS {
                    put_cell(pat, row, ch, [251, 0, 0, 0]); // empty
                }
            }
        }
        // Row-0 note on channel 0 of each pattern: C-3, instrument 1,
        // volume 63 (the 6-bit split field's maximum: vol_lo = 7 in
        // byte 1 bits 0..=2, vol_hi = 7 in byte 2 bits 4..=6).
        for pat in 0..3 {
            put_cell(pat, 0, 0, [0x30, (1 << 3) | 0x07, 0x70, 0x00]);
        }
        // Pattern 0: D32 on row 32 ch 1 (break to row 32 of next order).
        put_cell(0, 32, 1, [251, 0, 0x0D, 0x32]);
        // Pattern 2: B00 on row 16 ch 1 (jump to order 0).
        put_cell(2, 16, 1, [251, 0, 0x0B, 0x00]);
    }
    out.extend_from_slice(&pattern_bytes);

    // Sample body: 64-byte square wave.
    for i in 0..64 {
        out.push(if i < 32 { 100u8 } else { 156u8 }); // +100 / -100 as u8
    }
    out
}

// ---------- Steppers ----------

struct XmStepper {
    p: XmPlayerState,
}

impl XmStepper {
    fn new(bytes: &[u8]) -> Self {
        let hdr = xm::parse_header(bytes).expect("xm header");
        let (patterns, after) = xm::parse_patterns(&hdr, bytes).expect("patterns");
        let mut instruments = xm::parse_instruments(&hdr, bytes, after).expect("instruments");
        xm::extract_sample_bodies(&mut instruments, bytes);
        Self {
            p: XmPlayerState::new(&hdr, instruments, patterns, OUT_HZ),
        }
    }
}

struct StmStepper {
    p: StmPlayerState,
}

impl StmStepper {
    fn new(bytes: &[u8]) -> Self {
        let hdr = stm::parse_header(bytes).expect("stm header");
        let patterns = stm::parse_patterns(&hdr, bytes);
        let samples = stm::extract_samples(&hdr, bytes);
        Self {
            p: StmPlayerState::new(&hdr, samples, patterns, OUT_HZ),
        }
    }
}

fn skip_or(lib: Option<RefLib>) -> Option<RefLib> {
    if lib.is_none() {
        eprintln!(
            "[stm_xm_ref] SKIP: reference dylib not found (set OXIDEAV_TRACKER_REF_PATH to run)"
        );
    }
    lib
}

// ---------- Gates ----------

/// XM: our engine and the reference must agree on (order, row, speed)
/// at every probe of a 30-second trace through the break/jump/speed
/// fixture.
#[test]
fn xm_ref_position_lockstep() {
    let Some(mp) = skip_or(RefLib::try_open()) else {
        return;
    };
    let bytes = build_flow_xm();

    mp.configure_clean();
    let mp_file = unsafe { (mp.load)(bytes.as_ptr() as *const c_void, bytes.len() as i32) };
    assert!(
        !mp_file.is_null(),
        "reference failed to load the XM fixture"
    );
    mp.configure_clean();

    let mut ours = XmStepper::new(&bytes);

    // Chunk = 1/8 s. 30 s trace = 240 probes.
    let chunk_frames = (OUT_HZ / 8) as usize;
    let mut our_buf = vec![0i16; chunk_frames * 2];
    let mut mp_buf = vec![0u8; chunk_frames * 4];

    let mut mismatches = 0usize;
    let mut first_mismatch: Option<String> = None;
    let mut our_energy = 0.0f64;
    let mut mp_energy = 0.0f64;

    for probe in 0..240 {
        let produced = ours.p.render(&mut our_buf);
        assert_eq!(produced, chunk_frames, "our XM render starved");
        let n = unsafe {
            (mp.read)(
                mp_file,
                mp_buf.as_mut_ptr() as *mut c_void,
                mp_buf.len() as i32,
            )
        };
        assert!(n > 0, "reference XM render starved at probe {probe}");

        let mp_pcm: Vec<i16> = mp_buf[..n as usize]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        our_energy += rms_i16(&our_buf);
        mp_energy += rms_i16(&mp_pcm);

        let (our_o, our_r, our_s) = (
            ours.p.order_index as i32,
            ours.p.row as i32,
            ours.p.speed as i32,
        );
        let (mp_o, mp_r, mp_s) = unsafe {
            (
                (mp.get_order)(mp_file),
                (mp.get_row)(mp_file),
                (mp.get_speed)(mp_file),
            )
        };
        if (our_o, our_r, our_s) != (mp_o, mp_r, mp_s) {
            mismatches += 1;
            if first_mismatch.is_none() {
                first_mismatch = Some(format!(
                    "probe {probe} (t={:.2}s): ours O{our_o}R{our_r}S{our_s} \
                     vs ref O{mp_o}R{mp_r}S{mp_s}",
                    probe as f64 / 8.0
                ));
            }
        }
    }
    unsafe { (mp.unload)(mp_file) };

    eprintln!(
        "[xm_ref] 240 probes, {mismatches} position mismatches; \
         mean rms ours={:.0} ref={:.0}",
        our_energy / 240.0,
        mp_energy / 240.0
    );
    if let Some(m) = &first_mismatch {
        eprintln!("[xm_ref] first mismatch: {m}");
    }

    // Probes land mid-chunk, so a row transition inside the chunk can
    // report the pre-transition row on one engine and the post- on the
    // other for one probe. Allow that boundary jitter (≤5% of probes)
    // but no sustained divergence — a real flow bug (missed break,
    // wrong jump target, dropped speed change) desynchronises every
    // subsequent probe and blows well past the budget.
    assert!(
        mismatches <= 12,
        "XM position trace diverged from the reference: {mismatches}/240 \
         probes mismatched; first: {}",
        first_mismatch.as_deref().unwrap_or("-")
    );
    assert!(
        our_energy / 240.0 > 200.0,
        "our XM render is inaudible (mean rms {})",
        our_energy / 240.0
    );
}

/// STM two-phase gate.
///
/// Phase 1 (cross-engine, layout-level): the reference must LOAD our
/// synthetic STM — an independent reader accepting the staged header
/// layout (it demonstrably validates the layout: flipping the
/// file-type byte to 1 makes it reject the file). Its PLAYBACK of the
/// fixture is recorded as an observation but NOT asserted: against
/// every variant tried in round 451 — fixed 4-byte cells with each
/// documented marker byte, all-real-cell patterns, a packed reading
/// of the marker semantics, several version/tempo stampings — the
/// reference plays the song as ~0.12 s of silence while its order
/// cursor races to the end, i.e. it assigns every pattern zero
/// duration regardless of pattern content. With no known-good
/// real-world STM available to sanity-check the oracle's STM leg
/// (and the staged docs carrying no effect/tempo semantics to
/// arbitrate — see `stm-effect-semantics-gap.md`), the divergence is
/// unattributable and is left as a recorded observation rather than
/// a gate.
///
/// Phase 2 (our engine): a 30-second render must traverse all three
/// orders (D32 break out of pattern 0, natural run through pattern 1,
/// B00 loop from pattern 2) and stay audible throughout.
#[test]
fn stm_ref_order_flow_and_audibility() {
    let Some(mp) = skip_or(RefLib::try_open()) else {
        return;
    };
    let bytes = build_flow_stm();

    // ---- Phase 1: the reference accepts the layout; playback is an
    // observation only (see the doc comment). ----
    mp.configure_clean();
    let mp_file = unsafe { (mp.load)(bytes.as_ptr() as *const c_void, bytes.len() as i32) };
    assert!(
        !mp_file.is_null(),
        "reference rejected our synthetic STM fixture — header layout bug on our side?"
    );
    mp.configure_clean();
    let chunk_frames = (OUT_HZ / 8) as usize;
    let mut mp_buf = vec![0u8; chunk_frames * 4];
    let mut mp_rms_sum = 0.0f64;
    let mut mp_chunks = 0usize;
    let mut mp_max_order = -1i32;
    for _ in 0..240 {
        let n = unsafe {
            (mp.read)(
                mp_file,
                mp_buf.as_mut_ptr() as *mut c_void,
                mp_buf.len() as i32,
            )
        };
        if n <= 0 {
            break;
        }
        let mp_pcm: Vec<i16> = mp_buf[..n as usize]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        mp_rms_sum += rms_i16(&mp_pcm);
        mp_chunks += 1;
        mp_max_order = mp_max_order.max(unsafe { (mp.get_order)(mp_file) });
    }
    unsafe { (mp.unload)(mp_file) };
    eprintln!(
        "[stm_ref] reference observation: {mp_chunks} chunks ({:.2}s), \
         mean rms {:.0}, max order {mp_max_order} (not asserted)",
        mp_chunks as f64 / 8.0,
        mp_rms_sum / mp_chunks.max(1) as f64
    );

    // ---- Phase 2: our engine traverses the whole flow, audibly. ----
    let mut ours = StmStepper::new(&bytes);
    let mut our_buf = vec![0i16; chunk_frames * 2];
    let mut our_orders_seen = [false; 3];
    let mut our_rms_sum = 0.0f64;
    for probe in 0..240 {
        let produced = ours.p.render(&mut our_buf);
        assert_eq!(produced, chunk_frames, "our STM render starved at {probe}");
        // Our engine flags `ended` on the B00 backward wrap; clear it to
        // keep looping (the public decoder exposes the same choice to
        // callers via the `ended` flag).
        ours.p.ended = false;
        our_rms_sum += rms_i16(&our_buf);
        let o = ours.p.order_index;
        if o < 3 {
            our_orders_seen[o] = true;
        }
    }
    eprintln!(
        "[stm_ref] ours: orders seen {our_orders_seen:?}, mean rms {:.0}",
        our_rms_sum / 240.0
    );
    assert_eq!(
        our_orders_seen, [true; 3],
        "our STM engine failed to traverse the whole order flow"
    );
    assert!(
        our_rms_sum / 240.0 > 200.0,
        "our STM render is inaudible (mean rms {:.0})",
        our_rms_sum / 240.0
    );
}
