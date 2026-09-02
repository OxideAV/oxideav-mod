//! Impulse Tracker (`.it`) playback engine.
//!
//! Truth source: `docs/audio/trackers/it/ImpulseTracker-it.txt` —
//! §"Mathematics" (volume formula, linear slides), §"Internal Tables"
//! (pitch table, fine sine / ramp / square tables), §"Effect Info"
//! (the processing flow chart and the `Axx`…`Wxx` pseudocode), §"General
//! Info" (virtual-channel allocation and NNA), §"Impulse Pattern
//! Format" (volume-column equivalences), plus the effect letters IT
//! inherits from Scream Tracker 3 as listed in
//! `docs/audio/trackers/s3m/ScreamTracker-v3.20-effects.txt` (the IT
//! text itself only documents "Notes about effects (as compared to
//! other module formats)" for the letters whose meaning changed).
//!
//! Engine shape (mirrors the crate's XM player): a row/tick scheduler
//! drives per-host-channel effect state; every sounding note lives in
//! a *virtual channel* ([`ItVoice`]) over the shared
//! [`crate::mixer::MixerVoice`]. In sample mode the virtual channel is
//! the host channel; in instrument mode a new note can push the old
//! voice into the background per its New Note Action, so the voice pool
//! grows beyond the host channel count.
//!
//! Pitch is carried as a frequency in Hz. Under "Linear slides" a slide
//! of `n` units multiplies it by `2^(n/768)` (§"Mathematics"); under
//! "Amiga slides" the same `n` is added to the Scream Tracker period
//! `14317056 / Hz` (the ST3 period system IT inherits — see
//! `ScreamTracker-v3.20-s3m.txt` §"What is C2SPD?").

use crate::it::{
    ItCell, ItDca, ItDct, ItEnvelope, ItInstrument, ItLoopView, ItModule, ItNna, ItPattern,
    ItSample, ItVibratoWave, ItVolumeColumn, IT_FADEOUT_COUNT, IT_MAX_NOTE, IT_ORDER_END,
    IT_ORDER_SKIP, IT_PAN_SURROUND, IT_VOLCOL_PORTA_SLIDE_TABLE,
};
use crate::mixer::MixerVoice;

// ============================================================================
// Tables — §"Internal Tables"
// ============================================================================

/// `PitchTable` from §"Internal Tables", "Values are 16.16 bit",
/// transcribed as the printed `(lo, hi)` word pairs, one entry per
/// note C-0 … B-9. Entry 60 (C-5) is `1.0`, so
/// `Hz = C5Speed * PITCH_TABLE[note] / 65536`.
#[rustfmt::skip]
const PITCH_TABLE_WORDS: [(u16, u16); 120] = [
    (2048, 0), (2170, 0), (2299, 0), (2435, 0), (2580, 0), (2734, 0),
    (2896, 0), (3069, 0), (3251, 0), (3444, 0), (3649, 0), (3866, 0),
    (4096, 0), (4340, 0), (4598, 0), (4871, 0), (5161, 0), (5468, 0),
    (5793, 0), (6137, 0), (6502, 0), (6889, 0), (7298, 0), (7732, 0),
    (8192, 0), (8679, 0), (9195, 0), (9742, 0), (10321, 0), (10935, 0),
    (11585, 0), (12274, 0), (13004, 0), (13777, 0), (14596, 0), (15464, 0),
    (16384, 0), (17358, 0), (18390, 0), (19484, 0), (20643, 0), (21870, 0),
    (23170, 0), (24548, 0), (26008, 0), (27554, 0), (29193, 0), (30929, 0),
    (32768, 0), (34716, 0), (36781, 0), (38968, 0), (41285, 0), (43740, 0),
    (46341, 0), (49097, 0), (52016, 0), (55109, 0), (58386, 0), (61858, 0),
    (0, 1), (3897, 1), (8026, 1), (12400, 1), (17034, 1), (21944, 1),
    (27146, 1), (32657, 1), (38496, 1), (44682, 1), (51236, 1), (58179, 1),
    (0, 2), (7794, 2), (16051, 2), (24800, 2), (34068, 2), (43888, 2),
    (54292, 2), (65314, 2), (11456, 3), (23828, 3), (36936, 3), (50823, 3),
    (0, 4), (15588, 4), (32103, 4), (49600, 4), (2601, 5), (22240, 5),
    (43048, 5), (65092, 5), (22912, 6), (47656, 6), (8336, 7), (36110, 7),
    (0, 8), (31176, 8), (64205, 8), (33663, 9), (5201, 10), (44481, 10),
    (20559, 11), (64648, 11), (45823, 12), (29776, 13), (16671, 14), (6684, 15),
    (0, 16), (62352, 16), (62875, 17), (1790, 19), (10403, 20), (23425, 21),
    (41118, 22), (63761, 23), (26111, 25), (59552, 26), (33342, 28), (13368, 30),
];

const fn build_pitch_table() -> [u32; 120] {
    let mut out = [0u32; 120];
    let mut i = 0;
    while i < 120 {
        let (lo, hi) = PITCH_TABLE_WORDS[i];
        out[i] = ((hi as u32) << 16) | lo as u32;
        i += 1;
    }
    out
}

/// `PitchTable` as 16.16 fixed-point values indexed by note `0..=119`.
pub const IT_PITCH_TABLE: [u32; 120] = build_pitch_table();

/// `FineSineData` — 256 entries, `-64..=64`, used by vibrato and by
/// sample vibrato ("Sample vibrato uses a table 256-bytes long").
#[rustfmt::skip]
pub const IT_FINE_SINE: [i8; 256] = [
      0,  2,  3,  5,  6,  8,  9, 11, 12, 14, 16, 17, 19, 20, 22, 23,
     24, 26, 27, 29, 30, 32, 33, 34, 36, 37, 38, 39, 41, 42, 43, 44,
     45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 56, 57, 58, 59,
     59, 60, 60, 61, 61, 62, 62, 62, 63, 63, 63, 64, 64, 64, 64, 64,
     64, 64, 64, 64, 64, 64, 63, 63, 63, 62, 62, 62, 61, 61, 60, 60,
     59, 59, 58, 57, 56, 56, 55, 54, 53, 52, 51, 50, 49, 48, 47, 46,
     45, 44, 43, 42, 41, 39, 38, 37, 36, 34, 33, 32, 30, 29, 27, 26,
     24, 23, 22, 20, 19, 17, 16, 14, 12, 11,  9,  8,  6,  5,  3,  2,
      0, -2, -3, -5, -6, -8, -9,-11,-12,-14,-16,-17,-19,-20,-22,-23,
    -24,-26,-27,-29,-30,-32,-33,-34,-36,-37,-38,-39,-41,-42,-43,-44,
    -45,-46,-47,-48,-49,-50,-51,-52,-53,-54,-55,-56,-56,-57,-58,-59,
    -59,-60,-60,-61,-61,-62,-62,-62,-63,-63,-63,-64,-64,-64,-64,-64,
    -64,-64,-64,-64,-64,-64,-63,-63,-63,-62,-62,-62,-61,-61,-60,-60,
    -59,-59,-58,-57,-56,-56,-55,-54,-53,-52,-51,-50,-49,-48,-47,-46,
    -45,-44,-43,-42,-41,-39,-38,-37,-36,-34,-33,-32,-30,-29,-27,-26,
    -24,-23,-22,-20,-19,-17,-16,-14,-12,-11, -9, -8, -6, -5, -3, -2,
];

/// `FineRampDownData` — 256 entries, `64` down to `-64`.
#[rustfmt::skip]
pub const IT_FINE_RAMP_DOWN: [i8; 256] = [
     64, 63, 63, 62, 62, 61, 61, 60, 60, 59, 59, 58, 58, 57, 57, 56,
     56, 55, 55, 54, 54, 53, 53, 52, 52, 51, 51, 50, 50, 49, 49, 48,
     48, 47, 47, 46, 46, 45, 45, 44, 44, 43, 43, 42, 42, 41, 41, 40,
     40, 39, 39, 38, 38, 37, 37, 36, 36, 35, 35, 34, 34, 33, 33, 32,
     32, 31, 31, 30, 30, 29, 29, 28, 28, 27, 27, 26, 26, 25, 25, 24,
     24, 23, 23, 22, 22, 21, 21, 20, 20, 19, 19, 18, 18, 17, 17, 16,
     16, 15, 15, 14, 14, 13, 13, 12, 12, 11, 11, 10, 10,  9,  9,  8,
      8,  7,  7,  6,  6,  5,  5,  4,  4,  3,  3,  2,  2,  1,  1,  0,
      0, -1, -1, -2, -2, -3, -3, -4, -4, -5, -5, -6, -6, -7, -7, -8,
     -8, -9, -9,-10,-10,-11,-11,-12,-12,-13,-13,-14,-14,-15,-15,-16,
    -16,-17,-17,-18,-18,-19,-19,-20,-20,-21,-21,-22,-22,-23,-23,-24,
    -24,-25,-25,-26,-26,-27,-27,-28,-28,-29,-29,-30,-30,-31,-31,-32,
    -32,-33,-33,-34,-34,-35,-35,-36,-36,-37,-37,-38,-38,-39,-39,-40,
    -40,-41,-41,-42,-42,-43,-43,-44,-44,-45,-45,-46,-46,-47,-47,-48,
    -48,-49,-49,-50,-50,-51,-51,-52,-52,-53,-53,-54,-54,-55,-55,-56,
    -56,-57,-57,-58,-58,-59,-59,-60,-60,-61,-61,-62,-62,-63,-63,-64,
];

/// `FineSquareWave` — "128 Dup (64), 128 Dup (0)".
pub fn it_fine_square(pos: u8) -> i8 {
    if pos < 128 {
        64
    } else {
        0
    }
}

/// Waveform lookup shared by vibrato / tremolo / panbrello / sample
/// vibrato: `0` sine, `1` ramp down, `2` square, `3` random. The random
/// shape has no PRNG pinned by the staged text; the engine's own
/// deterministic generator supplies it (see [`ItPlayerState::rand`]).
fn waveform(kind: u8, pos: u8, random: i8) -> i32 {
    match kind & 3 {
        1 => IT_FINE_RAMP_DOWN[pos as usize] as i32,
        2 => it_fine_square(pos) as i32,
        3 => random as i32,
        _ => IT_FINE_SINE[pos as usize] as i32,
    }
}

/// `S2x` finetune → C-speed table, from
/// `ScreamTracker-v3.20-effects.txt` §"S2x Set finetune (=C4Spd)".
pub const IT_S2X_FINETUNE_TABLE: [u32; 16] = [
    7895, 7941, 7985, 8046, 8107, 8169, 8232, 8280, 8363, 8413, 8463, 8529, 8581, 8651, 8723, 8757,
];

/// Scream Tracker period constant: `note_herz = 14317056 / note_st3period`
/// (`ScreamTracker-v3.20-s3m.txt` §"What is C2SPD?").
pub const IT_AMIGA_PERIOD_CLOCK: f64 = 14_317_056.0;

/// Note number sounding at `C5Speed`.
pub const IT_C5_NOTE: u8 = 60;

/// Frequency in Hz of `note` (`0..=119`) for a sample whose C-5 rate is
/// `c5_speed`: `Hz = C5Speed * PitchTable[note] / 65536`.
pub fn note_frequency(note: u8, c5_speed: u32) -> f64 {
    let n = note.min(IT_MAX_NOTE) as usize;
    c5_speed as f64 * IT_PITCH_TABLE[n] as f64 / 65536.0
}

/// §"Mathematics": "Final frequency = Original frequency *
/// 2^(SlideValue/768)".
pub fn linear_slide(freq: f64, units: f64) -> f64 {
    freq * (units / 768.0).exp2()
}

/// Amiga-slide counterpart: add `units` to the ST3 period. Periods are
/// clamped to stay positive.
pub fn amiga_slide(freq: f64, units: f64) -> f64 {
    if freq <= 0.0 {
        return 0.0;
    }
    let period = (IT_AMIGA_PERIOD_CLOCK / freq + units).max(1.0);
    IT_AMIGA_PERIOD_CLOCK / period
}

/// Slide `freq` by `units` under the song's slide mode. Positive units
/// raise the pitch in both modes (an Amiga slide *up* subtracts from
/// the period).
pub fn slide(freq: f64, units: f64, linear: bool) -> f64 {
    if linear {
        linear_slide(freq, units)
    } else {
        amiga_slide(freq, -units)
    }
}

/// Clamp a frequency into a range the mixer can use.
fn clamp_freq(freq: f64) -> f64 {
    freq.clamp(1.0, 4_000_000.0)
}

// ============================================================================
// Effect numbering (1 = A … 26 = Z)
// ============================================================================

const FX_A: u8 = 1;
const FX_B: u8 = 2;
const FX_C: u8 = 3;
const FX_D: u8 = 4;
const FX_E: u8 = 5;
const FX_F: u8 = 6;
const FX_G: u8 = 7;
const FX_H: u8 = 8;
const FX_I: u8 = 9;
const FX_J: u8 = 10;
const FX_K: u8 = 11;
const FX_L: u8 = 12;
const FX_M: u8 = 13;
const FX_N: u8 = 14;
const FX_O: u8 = 15;
const FX_P: u8 = 16;
const FX_Q: u8 = 17;
const FX_R: u8 = 18;
const FX_S: u8 = 19;
const FX_T: u8 = 20;
const FX_U: u8 = 21;
const FX_V: u8 = 22;
const FX_W: u8 = 23;
const FX_X: u8 = 24;
const FX_Y: u8 = 25;
const FX_Z: u8 = 26;

/// Upper bound on simultaneously sounding virtual channels. Beyond it
/// the §"General Info" congestion rule kicks in (the quietest
/// background voice is stolen).
pub const IT_MAX_VOICES: usize = 256;

// ============================================================================
// Envelope playback state
// ============================================================================

/// Per-voice playback cursor over one [`ItEnvelope`].
#[derive(Clone, Copy, Debug, Default)]
pub struct EnvState {
    /// Current tick position.
    pub tick: u16,
    /// Envelope enabled for this voice (instrument flag, overridable
    /// by `S7x`).
    pub enabled: bool,
    /// The cursor ran past the last node with no loop to take.
    pub ended: bool,
    /// Last evaluated value × 256.
    pub value_x256: i32,
}

impl EnvState {
    fn start(env: &ItEnvelope) -> Self {
        EnvState {
            tick: 0,
            enabled: env.is_on(),
            ended: !env.is_on(),
            value_x256: env.nodes.first().map_or(0, |n| n.0 as i32 * 256),
        }
    }

    /// Evaluate at the current tick, then advance one tick applying the
    /// sustain loop (while `held`) or the normal loop. Returns the
    /// value × 256 at the tick that was just played.
    fn step(&mut self, env: &ItEnvelope, held: bool) -> i32 {
        if !self.enabled || env.nodes.is_empty() {
            return self.value_x256;
        }
        let v = env.value_at_x256(self.tick);
        self.value_x256 = v;
        let node_tick = |i: u8| env.nodes.get(i as usize).map_or(0, |n| n.1);
        let next = self.tick.saturating_add(1);
        if held && env.has_sustain_loop() && next > node_tick(env.sustain_end) {
            self.tick = node_tick(env.sustain_begin);
        } else if env.has_loop() && next > node_tick(env.loop_end) {
            self.tick = node_tick(env.loop_begin);
        } else if next > env.end_tick() {
            self.tick = env.end_tick();
            self.ended = true;
        } else {
            self.tick = next;
        }
        v
    }
}

// ============================================================================
// Voices (virtual channels)
// ============================================================================

/// One virtual channel: a sounding note over the shared mixer voice.
#[derive(Clone, Debug, Default)]
pub struct ItVoice {
    pub active: bool,
    /// No longer controlled by its host channel (NNA continue / off /
    /// fade moved it here).
    pub background: bool,
    /// Host channel index.
    pub host: u8,
    pub mixer: MixerVoice,
    /// 1-based sample number (0 = none).
    pub sample: u8,
    /// 1-based instrument number (0 = none / sample mode).
    pub instrument: u8,
    /// Sounding note `0..=119` (after the keymap).
    pub note: u8,
    /// Pattern note that triggered this voice (before the keymap), used
    /// for duplicate-note checks.
    pub pattern_note: u8,
    /// Base frequency (Hz) before per-tick vibrato / arpeggio / pitch
    /// envelope.
    pub freq: f64,
    /// Note volume `0..=64`.
    pub volume: u8,
    /// Note pan `0..=64`, or [`IT_PAN_SURROUND`].
    pub pan: u8,
    /// Note released (note off): sustain loops open, envelopes leave
    /// their sustain loops.
    pub released: bool,
    /// Note fade active: `fade` counts down by the instrument fadeout.
    pub fading: bool,
    /// Note-fade component `NFC`, starts at [`IT_FADEOUT_COUNT`].
    pub fade: u16,
    pub vol_env: EnvState,
    pub pan_env: EnvState,
    pub pitch_env: EnvState,
    /// Sample-vibrato sweep accumulator (`AX` in the §"Impulse Sample
    /// Format" pseudocode) and table position.
    pub sample_vib_sweep: u16,
    pub sample_vib_pos: u8,
    /// Random-volume factor × 256 applied at trigger (`RV`).
    pub random_vol_x256: u16,
    /// Output amplitude for the current tick (post volume formula, pre
    /// mix volume) and effective pan.
    pub amp: f32,
    pub final_pan: u8,
    /// Frequency actually mixed this tick.
    pub play_freq: f64,
}

// ============================================================================
// Host channels
// ============================================================================

/// Per-host-channel effect state.
#[derive(Clone, Debug, Default)]
pub struct ItChannel {
    /// Index of the foreground voice.
    pub voice: Option<usize>,
    /// Last note / instrument seen in the pattern (for cells that
    /// omit one of them).
    pub last_note: u8,
    pub last_instrument: u8,
    /// Current note volume `0..=64` (affects the foreground voice).
    pub volume: u8,
    /// Channel volume `Mxx` / `Nxx`, `0..=64`.
    pub channel_volume: u8,
    /// Channel pan `0..=64` or surround.
    pub pan: u8,
    /// Muted (`+128` in the header pan byte).
    pub muted: bool,
    /// Current base frequency for the foreground voice.
    pub freq: f64,
    /// Tone-portamento target.
    pub porta_target: f64,
    /// C5 speed of the sample currently sounding (for glissando and
    /// arpeggio steps).
    pub c5_speed: u32,
    // ---- parameter memories ----
    pub mem_d: u8,
    /// `Exx` / `Fxx` (and `Gxx` under compatible Gxx).
    pub mem_ef: u8,
    pub mem_g: u8,
    /// `Hxy` / `Uxy` speed + depth (the vibrato memory is shared).
    pub mem_h_speed: u8,
    pub mem_h_depth: u8,
    pub mem_i: u8,
    pub mem_j: u8,
    pub mem_n: u8,
    pub mem_o: u8,
    pub mem_p: u8,
    pub mem_q: u8,
    pub mem_r: u8,
    pub mem_s: u8,
    pub mem_t: u8,
    pub mem_w: u8,
    pub mem_y: u8,
    /// Volume-column fine / normal volume slide memory ("(Fine) Volume
    /// up/down all share the same memory").
    pub mem_volcol_slide: u8,
    // ---- LFOs ----
    pub vib_pos: u8,
    pub vib_wave: u8,
    pub trem_pos: u8,
    pub trem_wave: u8,
    pub panb_pos: u16,
    pub panb_wave: u8,
    pub panb_random: i8,
    pub panb_delay: u8,
    // ---- per-row effect scratch ----
    /// Effect + param in force for this row (after memory resolution).
    pub cmd: u8,
    pub param: u8,
    /// Volume column in force for this row.
    pub volcol: ItVolumeColumn,
    /// Tremor position within the on+off cycle (advances every tick).
    pub tremor_pos: u8,
    pub tremor_off: bool,
    pub retrig_count: u8,
    pub glissando: bool,
    pub high_offset: u8,
    pub surround: bool,
    /// Pattern-loop row + remaining count (`SBx`).
    pub loop_row: u16,
    pub loop_count: u8,
    /// Scheduled note cut / delay ticks for the current row.
    pub note_cut_tick: Option<u8>,
    pub note_delay_tick: Option<u8>,
    pub delayed_cell: ItCell,
    /// `S7x` NNA override for the *next* note on this channel.
    pub nna_override: Option<ItNna>,
    /// Vibrato / tremolo / panbrello offsets computed this tick.
    pub vib_delta: f64,
    pub trem_delta: i32,
    pub panb_delta: i32,
    pub arp_offset: u8,
    /// Set when a cell on this row started a tone portamento (note not
    /// retriggered).
    pub porta_this_row: bool,
    /// A note was triggered on the current tick (suppresses the retrig
    /// counter on that tick).
    pub note_triggered: bool,
}

impl ItChannel {
    fn with_defaults(pan: u8, muted: bool, channel_volume: u8) -> Self {
        ItChannel {
            volume: 64,
            channel_volume,
            pan,
            muted,
            volcol: ItVolumeColumn::None,
            ..ItChannel::default()
        }
    }
}

// ============================================================================
// Player
// ============================================================================

/// Top-level Impulse Tracker player state.
pub struct ItPlayerState {
    pub module: ItModule,
    pub sample_rate: u32,
    pub channels: Vec<ItChannel>,
    pub voices: Vec<ItVoice>,
    /// Ticks per row (`Axx`).
    pub speed: u8,
    /// Tempo (`Txx`), `32..=255`.
    pub tempo: u8,
    /// Global volume `0..=128` (`Vxx` / `Wxx`).
    pub global_volume: u8,
    pub order_index: usize,
    pub row: u16,
    pub tick: u8,
    pub tick_sample_cursor: u32,
    pub ended: bool,
    /// Song wrapped to order 0 this many times.
    pub loops: u16,
    pub pending_order_jump: Option<usize>,
    pub pending_break_row: Option<u16>,
    /// `SEx` row repeats remaining.
    pub pattern_delay: u8,
    pub in_pattern_delay_replay: bool,
    /// `S6x` extra ticks appended to the current row.
    pub tick_delay: u8,
    pub pending_loop_row: Option<u16>,
    pub linear_slides: bool,
    pub old_effects: bool,
    pub compat_gxx: bool,
    pub instrument_mode: bool,
    rng: u32,
}

impl ItPlayerState {
    pub fn new(module: ItModule, sample_rate: u32) -> Self {
        let h = &module.header;
        let n_ch = module.num_channels.max(1) as usize;
        let channels = (0..n_ch)
            .map(|i| {
                let raw = h.channel_pan.get(i).copied().unwrap_or(32);
                let pan = raw & 0x7F;
                let pan = if pan == IT_PAN_SURROUND {
                    IT_PAN_SURROUND
                } else {
                    pan.min(64)
                };
                ItChannel::with_defaults(
                    pan,
                    raw & 0x80 != 0,
                    h.channel_volume.get(i).copied().unwrap_or(64).min(64),
                )
            })
            .collect();
        let linear_slides = h.linear_slides();
        let old_effects = h.old_effects();
        let compat_gxx = h.compatible_gxx();
        let instrument_mode = h.uses_instruments() && !module.instruments.is_empty();
        ItPlayerState {
            speed: h.initial_speed.max(1),
            tempo: h.initial_tempo.max(32),
            global_volume: h.global_volume.min(128),
            module,
            sample_rate,
            channels,
            voices: Vec::new(),
            order_index: 0,
            row: 0,
            tick: 0,
            tick_sample_cursor: 0,
            ended: false,
            loops: 0,
            pending_order_jump: None,
            pending_break_row: None,
            pattern_delay: 0,
            in_pattern_delay_replay: false,
            tick_delay: 0,
            pending_loop_row: None,
            linear_slides,
            old_effects,
            compat_gxx,
            instrument_mode,
            rng: 0x2545_F491,
        }
    }

    /// Output frames per tick: `2.5 / tempo` seconds.
    pub fn samples_per_tick(&self) -> u32 {
        ((self.sample_rate as f64) * 2.5 / self.tempo as f64).max(1.0) as u32
    }

    /// Deterministic xorshift generator (random waveform + `RV`).
    pub fn rand(&mut self) -> u32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        x
    }

    /// Random waveform sample in `-64..=64`.
    fn rand_wave(&mut self) -> i8 {
        ((self.rand() % 129) as i32 - 64) as i8
    }

    fn current_pattern(&self) -> Option<&ItPattern> {
        let pat = *self.module.header.orders.get(self.order_index)? as usize;
        self.module.patterns.get(pat)
    }

    fn pattern_rows(&self) -> u16 {
        self.current_pattern().map_or(64, |p| p.num_rows.max(1))
    }

    fn cell_at(&self, row: u16, ch: usize) -> ItCell {
        self.current_pattern()
            .map_or_else(ItCell::default, |p| p.cell(row as usize, ch))
    }

    /// Skip `+++` entries and handle `---` / end-of-list starting at
    /// `self.order_index`. Returns false when the song has ended.
    fn settle_order(&mut self) -> bool {
        let orders = &self.module.header.orders;
        loop {
            match orders.get(self.order_index) {
                Some(&o) if o == IT_ORDER_SKIP => self.order_index += 1,
                Some(&o) if o == IT_ORDER_END => return false,
                Some(_) => return true,
                None => return false,
            }
        }
    }

    /// Move to the next order (or the pending jump target), wrapping the
    /// song to order 0 once and flagging `ended` on the wrap.
    fn advance_order(&mut self, target: Option<usize>) {
        self.order_index = target.unwrap_or(self.order_index + 1);
        if !self.settle_order() {
            // "if Order[ProcessOrder] = 0xFFh, ProcessOrder = 0"
            self.order_index = 0;
            self.loops = self.loops.saturating_add(1);
            self.ended = true;
            if !self.settle_order() {
                self.order_index = 0;
            }
        }
        for ch in self.channels.iter_mut() {
            ch.loop_row = 0;
            ch.loop_count = 0;
        }
    }

    /// Row-advance logic run when the row's last tick has elapsed.
    fn next_row(&mut self) {
        if let Some(loop_row) = self.pending_loop_row.take() {
            self.row = loop_row;
            self.pending_order_jump = None;
            self.pending_break_row = None;
            return;
        }
        let jump = self.pending_order_jump.take();
        let brk = self.pending_break_row.take();
        if jump.is_some() || brk.is_some() {
            self.advance_order(jump);
            let rows = self.pattern_rows();
            let r = brk.unwrap_or(0);
            self.row = if r >= rows { 0 } else { r };
            return;
        }
        self.row += 1;
        if self.row >= self.pattern_rows() {
            self.advance_order(None);
            self.row = 0;
        }
    }

    // ------------------------------------------------------------------
    // Voice allocation — §"General Info"
    // ------------------------------------------------------------------

    fn alloc_voice(&mut self) -> usize {
        if let Some(i) = self.voices.iter().position(|v| !v.active) {
            return i;
        }
        if self.voices.len() < IT_MAX_VOICES {
            self.voices.push(ItVoice::default());
            return self.voices.len() - 1;
        }
        // "find the channel of lowest volume that is in the background"
        let mut best: Option<(usize, f32)> = None;
        for (i, v) in self.voices.iter().enumerate() {
            if v.background && best.map_or(true, |(_, a)| v.amp < a) {
                best = Some((i, v.amp));
            }
        }
        match best {
            Some((i, _)) => i,
            // No background voice to steal: reuse the last slot.
            None => IT_MAX_VOICES - 1,
        }
    }

    fn sample_for(&self, sample: u8) -> Option<&ItSample> {
        if sample == 0 {
            return None;
        }
        self.module.samples.get(sample as usize - 1)
    }

    fn instrument_for(&self, instrument: u8) -> Option<&ItInstrument> {
        if instrument == 0 {
            return None;
        }
        self.module.instruments.get(instrument as usize - 1)
    }

    /// Resolve `(pattern_note, instrument)` → `(sounding_note, sample)`.
    fn resolve(&self, note: u8, instrument: u8) -> (u8, u8) {
        if self.instrument_mode {
            match self.instrument_for(instrument) {
                Some(ins) => ins.map_note(note),
                None => (note, 0),
            }
        } else {
            (note, instrument)
        }
    }

    /// Apply the New Note Action to a channel's foreground voice before
    /// a new note takes the channel. Returns true when the old voice
    /// slot may be reused directly (NNA cut).
    fn apply_nna(&mut self, ch: usize, nna: ItNna) -> Option<usize> {
        let vi = self.channels[ch].voice?;
        let v = &mut self.voices[vi];
        if !v.active {
            return Some(vi);
        }
        match nna {
            ItNna::Cut => {
                v.active = false;
                Some(vi)
            }
            ItNna::Continue => {
                v.background = true;
                None
            }
            ItNna::NoteOff => {
                v.background = true;
                Self::release_voice(v, self.instrument_mode, &self.module);
                None
            }
            ItNna::NoteFade => {
                v.background = true;
                v.fading = true;
                None
            }
        }
    }

    /// Note off: open sustain loops; without a volume envelope (or with
    /// a looping one) start the fade.
    fn release_voice(v: &mut ItVoice, instrument_mode: bool, module: &ItModule) {
        if v.released {
            return;
        }
        v.released = true;
        if instrument_mode {
            let env_on = module
                .instruments
                .get(v.instrument as usize - 1)
                .is_some_and(|i| i.volume_envelope.is_on() && v.vol_env.enabled);
            let env_loops = module
                .instruments
                .get(v.instrument as usize - 1)
                .is_some_and(|i| i.volume_envelope.has_loop());
            if !env_on || env_loops {
                v.fading = true;
            }
        }
        // Sample mode: a note off only opens the sustain loop; a sample
        // without one keeps playing (verified against the black-box
        // oracle — the note is neither cut nor faded).
        let _ = module;
    }

    /// Duplicate check (`DCT`) against every voice on host channel `ch`
    /// that matches the incoming note, applying `DCA`.
    fn duplicate_check(
        &mut self,
        ch: usize,
        ins: &ItInstrument,
        instr_no: u8,
        note: u8,
        sample: u8,
    ) {
        if ins.dct == ItDct::Off {
            return;
        }
        let instrument_mode = self.instrument_mode;
        let module = &self.module;
        for v in self.voices.iter_mut() {
            if !v.active || v.host as usize != ch {
                continue;
            }
            let dup = match ins.dct {
                ItDct::Note => v.pattern_note == note && v.instrument == instr_no,
                ItDct::Sample => v.sample == sample,
                ItDct::Instrument => v.instrument == instr_no,
                ItDct::Off => false,
            };
            if !dup {
                continue;
            }
            match ins.dca {
                ItDca::Cut => v.active = false,
                ItDca::NoteOff => Self::release_voice(v, instrument_mode, module),
                ItDca::NoteFade => v.fading = true,
            }
        }
    }

    /// Trigger a note on channel `ch`. `offset` is the sample-offset
    /// frame position (`Oxx` / `SAy`) to start from.
    fn trigger_note(&mut self, ch: usize, pattern_note: u8, instrument: u8, offset: usize) {
        let (note, sample_no) = self.resolve(pattern_note, instrument);
        let Some(sample) = self.sample_for(sample_no) else {
            // No sample: the note is silent, but the channel state
            // still updates.
            self.channels[ch].last_note = pattern_note;
            return;
        };
        if sample.pcm.is_empty() {
            self.channels[ch].last_note = pattern_note;
            return;
        }
        let c5 = sample.c5_speed.max(1);
        let sample_pan = sample.pan();
        let sample_vol = sample.default_volume;
        let sample_frames = sample.pcm.len();

        // NNA on the voice currently owned by this channel.
        let vi = if self.instrument_mode {
            let ins_nna = self.channels[ch]
                .voice
                .and_then(|vi| self.voices.get(vi))
                .filter(|v| v.active)
                .and_then(|v| self.instrument_for(v.instrument))
                .map_or(ItNna::Cut, |i| i.nna);
            let nna = self.channels[ch].nna_override.take().unwrap_or(ins_nna);
            if let Some(ins) = self.instrument_for(instrument).cloned() {
                self.duplicate_check(ch, &ins, instrument, pattern_note, sample_no);
            }
            match self.apply_nna(ch, nna) {
                Some(slot) => slot,
                None => self.alloc_voice(),
            }
        } else {
            match self.channels[ch].voice {
                Some(vi) => vi,
                None => self.alloc_voice(),
            }
        };

        let freq = note_frequency(note, c5);
        let mut pan = self.channels[ch].pan;
        let mut random_vol_x256 = 256u16;
        let ins = self.instrument_for(instrument).cloned();
        if self.instrument_mode {
            if let Some(ins) = &ins {
                // "NotePan = ChannelPan; if InstrumentPan=On then NotePan
                // = InstrumentPan; NotePan += (InstrumentNote -
                // PPCenter) * PPSeparation / 8".
                if let Some(p) = ins.pan() {
                    pan = p;
                }
                if pan != IT_PAN_SURROUND {
                    let pps = (note as i32 - ins.pitch_pan_center as i32)
                        * ins.pitch_pan_separation as i32
                        / 8;
                    pan = (pan as i32 + pps).clamp(0, 64) as u8;
                }
                if ins.random_volume > 0 {
                    // ±RV percent around unity.
                    let r = (self.rand() % (2 * ins.random_volume as u32 + 1)) as i32
                        - ins.random_volume as i32;
                    random_vol_x256 = ((256 + 256 * r / 100).clamp(0, 512)) as u16;
                }
            }
        }
        if let Some(p) = sample_pan {
            pan = p;
        }

        let chan = &mut self.channels[ch];
        chan.voice = Some(vi);
        chan.last_note = pattern_note;
        chan.freq = freq;
        chan.porta_target = freq;
        chan.c5_speed = c5;
        chan.volume = sample_vol;
        chan.pan = pan;
        chan.vib_pos = 0;
        chan.trem_pos = 0;
        chan.panb_pos = 0;
        chan.tremor_pos = 0;
        chan.tremor_off = false;
        chan.retrig_count = 0;
        chan.note_triggered = true;

        let v = &mut self.voices[vi];
        *v = ItVoice {
            active: true,
            background: false,
            host: ch as u8,
            sample: sample_no,
            instrument: if self.instrument_mode { instrument } else { 0 },
            note,
            pattern_note,
            freq,
            volume: sample_vol,
            pan,
            fade: IT_FADEOUT_COUNT,
            random_vol_x256,
            ..ItVoice::default()
        };
        v.mixer.trigger(freq as f32, 1.0);
        if offset > 0 {
            if offset < sample_frames {
                v.mixer.pos = offset as f32;
            } else if self.old_effects {
                // "Oxx past the sample end … 'Old Effects' is ON, in
                // which case the Oxx will play from the end of the
                // sample."
                v.mixer.pos = sample_frames.saturating_sub(1) as f32;
            }
            // IT mode: "Oxx past the sample end will be ignored".
        }
        if let Some(ins) = &ins {
            v.vol_env = EnvState::start(&ins.volume_envelope);
            v.pan_env = EnvState::start(&ins.panning_envelope);
            v.pitch_env = EnvState::start(&ins.pitch_envelope);
        }
    }

    /// Retrigger envelopes (compatible-Gxx tone portamento with an
    /// instrument).
    fn retrigger_envelopes(&mut self, ch: usize) {
        let Some(vi) = self.channels[ch].voice else {
            return;
        };
        let Some(ins) = self.instrument_for(self.voices[vi].instrument).cloned() else {
            return;
        };
        let v = &mut self.voices[vi];
        v.vol_env = EnvState::start(&ins.volume_envelope);
        v.pan_env = EnvState::start(&ins.panning_envelope);
        v.pitch_env = EnvState::start(&ins.pitch_envelope);
        v.fade = IT_FADEOUT_COUNT;
        v.fading = false;
        v.released = false;
    }

    // ------------------------------------------------------------------
    // Row processing (tick 0)
    // ------------------------------------------------------------------

    fn process_row(&mut self) {
        let n_ch = self.channels.len();
        for ch in 0..n_ch {
            let cell = self.cell_at(self.row, ch);
            self.channels[ch].note_cut_tick = None;
            self.channels[ch].note_delay_tick = None;
            self.channels[ch].porta_this_row = false;
            self.channels[ch].arp_offset = 0;
            // Resolve the effect early: note delay defers the whole
            // cell.
            let (cmd, param) = if cell.has_command() {
                (cell.command, cell.param)
            } else {
                (0, 0)
            };
            if cmd == FX_S && param >> 4 == 0xD && param & 0x0F != 0 {
                let c = &mut self.channels[ch];
                c.note_delay_tick = Some(param & 0x0F);
                c.delayed_cell = cell;
                c.cmd = FX_S;
                c.param = param;
                c.volcol = ItVolumeColumn::None;
                continue;
            }
            self.process_cell(ch, cell);
        }
    }

    /// Apply one cell's note / instrument / volume column / effect
    /// (tick-0 portion) to channel `ch`.
    fn process_cell(&mut self, ch: usize, cell: ItCell) {
        let (cmd, param) = if cell.has_command() {
            (cell.command, cell.param)
        } else {
            (0, 0)
        };
        let volcol = cell.volume_column();
        let porta_cmd =
            cmd == FX_G || cmd == FX_L || matches!(volcol, ItVolumeColumn::TonePortamento(_));

        // ---- instrument column ----
        let mut instrument = self.channels[ch].last_instrument;
        let inst_given = cell.has_instrument();
        if inst_given {
            instrument = cell.instrument;
            self.channels[ch].last_instrument = instrument;
        }

        // ---- sample offset (Oxx / SAy) resolved before the trigger ----
        let mut offset = 0usize;
        if cmd == FX_O {
            let c = &mut self.channels[ch];
            if param != 0 {
                c.mem_o = param;
            }
            offset = (c.mem_o as usize) << 8 | (c.high_offset as usize) << 16;
        }

        // ---- note column ----
        if cell.has_note() {
            if porta_cmd
                && self.channels[ch]
                    .voice
                    .is_some_and(|vi| self.voices[vi].active)
            {
                // Tone portamento: the note is the target, no retrigger.
                let (note, sample_no) = self.resolve(cell.note, instrument);
                let c5 = self
                    .sample_for(sample_no)
                    .map_or(self.channels[ch].c5_speed, |s| s.c5_speed.max(1));
                let target = note_frequency(note, c5.max(1));
                let chan = &mut self.channels[ch];
                chan.porta_target = target;
                chan.last_note = cell.note;
                chan.porta_this_row = true;
                if inst_given {
                    if self.compat_gxx {
                        // "Gxx with an instrument present will cause the
                        // envelopes to be retriggered. If you change a
                        // sample on a row with Gxx, it'll adjust the
                        // frequency … NewFrequency = OldFrequency *
                        // NewC5 / OldC5".
                        let old_c5 = self.channels[ch].c5_speed.max(1);
                        if c5 != old_c5 {
                            let c = &mut self.channels[ch];
                            c.freq = c.freq * c5 as f64 / old_c5 as f64;
                            c.c5_speed = c5;
                        }
                        self.retrigger_envelopes(ch);
                    }
                    let vol = self.sample_for(sample_no).map(|s| s.default_volume);
                    if let Some(vol) = vol {
                        self.channels[ch].volume = vol;
                    }
                }
            } else {
                self.trigger_note(ch, cell.note, instrument, offset);
            }
        } else if cell.is_note_off() {
            self.note_off(ch);
        } else if cell.is_note_cut() {
            self.note_cut(ch);
        } else if cell.is_note_fade() {
            self.note_fade(ch);
        } else if inst_given {
            // Instrument alone: reload the sample default volume.
            let (_, sample_no) = self.resolve(self.channels[ch].last_note, instrument);
            if let Some(vol) = self.sample_for(sample_no).map(|s| s.default_volume) {
                self.channels[ch].volume = vol;
            }
            if self.instrument_mode {
                if let Some(vi) = self.channels[ch].voice {
                    if self.voices[vi].active && !self.voices[vi].background {
                        self.retrigger_envelopes(ch);
                    }
                }
            }
        }

        // ---- volume column (tick-0 part) ----
        self.channels[ch].volcol = volcol;
        match volcol {
            ItVolumeColumn::Volume(v) => self.channels[ch].volume = v.min(64),
            ItVolumeColumn::Panning(p) => {
                self.channels[ch].pan = p.min(64);
                self.channels[ch].surround = false;
            }
            ItVolumeColumn::FineVolumeUp(x) => {
                let c = &mut self.channels[ch];
                if x != 0 {
                    c.mem_volcol_slide = x;
                }
                c.volume = (c.volume + c.mem_volcol_slide).min(64);
            }
            ItVolumeColumn::FineVolumeDown(x) => {
                let c = &mut self.channels[ch];
                if x != 0 {
                    c.mem_volcol_slide = x;
                }
                c.volume = c.volume.saturating_sub(c.mem_volcol_slide);
            }
            ItVolumeColumn::VolumeSlideUp(x) | ItVolumeColumn::VolumeSlideDown(x) => {
                if x != 0 {
                    self.channels[ch].mem_volcol_slide = x;
                }
            }
            ItVolumeColumn::PitchSlideUp(x) | ItVolumeColumn::PitchSlideDown(x) => {
                // "Pitch slide up/down affect E/F/(G)'s memory — a Pitch
                // slide up/down of x is equivalent to a normal slide by
                // x*4".
                if x != 0 {
                    let c = &mut self.channels[ch];
                    c.mem_ef = x * 4;
                    if self.compat_gxx {
                        c.mem_g = x * 4;
                    }
                }
            }
            ItVolumeColumn::TonePortamento(x) => {
                // "Portamento to (Gx) affects the memory for Gxx".
                if x != 0 {
                    let c = &mut self.channels[ch];
                    c.mem_g = IT_VOLCOL_PORTA_SLIDE_TABLE[x as usize];
                    if self.compat_gxx {
                        c.mem_ef = c.mem_g;
                    }
                }
            }
            ItVolumeColumn::Vibrato(y) => {
                // "Vibrato uses the same 'memory' as Hxx/Uxx."
                if y != 0 {
                    self.channels[ch].mem_h_depth = y * 4;
                }
            }
            ItVolumeColumn::None => {}
        }

        // ---- effect column (tick-0 part) ----
        self.channels[ch].cmd = cmd;
        self.channels[ch].param = param;
        self.effect_tick0(ch, cmd, param);
    }

    fn note_off(&mut self, ch: usize) {
        if let Some(vi) = self.channels[ch].voice {
            let v = &mut self.voices[vi];
            if v.active && !v.background {
                Self::release_voice(v, self.instrument_mode, &self.module);
            }
        }
    }

    fn note_cut(&mut self, ch: usize) {
        if let Some(vi) = self.channels[ch].voice {
            let v = &mut self.voices[vi];
            if !v.background {
                v.active = false;
            }
        }
    }

    fn note_fade(&mut self, ch: usize) {
        if let Some(vi) = self.channels[ch].voice {
            let v = &mut self.voices[vi];
            if v.active && !v.background {
                v.fading = true;
            }
        }
    }

    /// Resolve `xx == 0 → memory` for one memory slot, storing a
    /// non-zero parameter.
    fn mem(slot: &mut u8, param: u8) -> u8 {
        if param != 0 {
            *slot = param;
        }
        *slot
    }

    fn effect_tick0(&mut self, ch: usize, cmd: u8, param: u8) {
        let x = param >> 4;
        let y = param & 0x0F;
        match cmd {
            FX_A => {
                // "if (xx != 0) { Maxtick = xx; Currenttick = xx; }"
                if param != 0 {
                    self.speed = param;
                }
            }
            FX_B => {
                // "ProcessOrder = xx - 1; ProcessRow = 0xFFFE" — the jump
                // lands at the start of order xx after this row.
                self.pending_order_jump = Some(param as usize);
                if self.pending_break_row.is_none() {
                    self.pending_break_row = Some(0);
                }
            }
            FX_C => {
                // "BreakRow = xx" — IT's Cxx "is now in *HEX*".
                self.pending_break_row = Some(param as u16);
            }
            FX_D | FX_K | FX_L => {
                let c = &mut self.channels[ch];
                let p = Self::mem(&mut c.mem_d, param);
                let (px, py) = (p >> 4, p & 0x0F);
                // "Order of testing: Dx0, D0x, DxF, DFx". Dx0 / D0x with
                // x = F also slide straight away (S3M compat).
                if py == 0 {
                    if px == 0xF {
                        c.volume = (c.volume + 15).min(64);
                    }
                } else if px == 0 {
                    if py == 0xF {
                        c.volume = c.volume.saturating_sub(15);
                    }
                } else if py == 0xF {
                    c.volume = (c.volume + px).min(64);
                } else if px == 0xF {
                    c.volume = c.volume.saturating_sub(py);
                }
                if cmd == FX_L {
                    self.channels[ch].porta_this_row = true;
                }
            }
            FX_E | FX_F => {
                let c = &mut self.channels[ch];
                let p = Self::mem(&mut c.mem_ef, param);
                if self.compat_gxx {
                    c.mem_g = p;
                }
                let down = cmd == FX_E;
                let (px, py) = (p >> 4, p & 0x0F);
                // EFx fine (×4 units once), EEx extra fine (×1 once).
                let units = match px {
                    0xF => 4.0 * py as f64,
                    0xE => py as f64,
                    _ => 0.0,
                };
                if units != 0.0 {
                    let u = if down { -units } else { units };
                    c.freq = clamp_freq(slide(c.freq, u, self.linear_slides));
                }
            }
            FX_G => {
                let c = &mut self.channels[ch];
                if param != 0 {
                    c.mem_g = param;
                    if self.compat_gxx {
                        c.mem_ef = param;
                    }
                } else if self.compat_gxx {
                    c.mem_g = c.mem_ef;
                }
                c.porta_this_row = true;
            }
            FX_H | FX_U => {
                let c = &mut self.channels[ch];
                if x != 0 {
                    c.mem_h_speed = x * 4;
                }
                if y != 0 {
                    // Hxy: depth = y*4; Uxy: depth = y; doubled under
                    // Old Effects.
                    let d = if cmd == FX_H { y * 4 } else { y };
                    c.mem_h_depth = if self.old_effects { d << 1 } else { d };
                }
            }
            FX_I => {
                let c = &mut self.channels[ch];
                let p = Self::mem(&mut c.mem_i, param);
                let _ = p;
            }
            FX_J => {
                let c = &mut self.channels[ch];
                Self::mem(&mut c.mem_j, param);
            }
            FX_M => {
                self.channels[ch].channel_volume = param.min(64);
            }
            FX_N => {
                let c = &mut self.channels[ch];
                let p = Self::mem(&mut c.mem_n, param);
                let (px, py) = (p >> 4, p & 0x0F);
                // "Order of testing: Nx0, N0x, NxF, NFx".
                if py == 0 || px == 0 {
                    // per-tick
                } else if py == 0xF {
                    c.channel_volume = (c.channel_volume + px).min(64);
                } else if px == 0xF {
                    c.channel_volume = c.channel_volume.saturating_sub(py);
                }
            }
            FX_O => {
                // Handled before the note trigger. A bare Oxx on a row
                // without a note re-seeks the sounding voice.
                let c = &self.channels[ch];
                let cell = self.cell_at(self.row, ch);
                if !cell.has_note() {
                    if let Some(vi) = c.voice {
                        let offset = (c.mem_o as usize) << 8 | (c.high_offset as usize) << 16;
                        let frames = self
                            .sample_for(self.voices[vi].sample)
                            .map_or(0, |s| s.pcm.len());
                        let v = &mut self.voices[vi];
                        if v.active && !v.background {
                            if offset < frames {
                                v.mixer.pos = offset as f32;
                            } else if self.old_effects {
                                v.mixer.pos = frames.saturating_sub(1) as f32;
                            }
                        }
                    }
                }
            }
            FX_P => {
                // Panning slide: Px0 slides LEFT by x per tick, P0y slides
                // RIGHT by y; PxF / PFy are the fine (tick-0) forms.
                let c = &mut self.channels[ch];
                let p = Self::mem(&mut c.mem_p, param);
                let (px, py) = (p >> 4, p & 0x0F);
                if c.pan != IT_PAN_SURROUND {
                    if py == 0xF && px != 0 {
                        c.pan = c.pan.saturating_sub(px);
                    } else if px == 0xF && py != 0 {
                        c.pan = (c.pan + py).min(64);
                    }
                }
            }
            FX_Q => {
                let c = &mut self.channels[ch];
                Self::mem(&mut c.mem_q, param);
            }
            FX_R => {
                let c = &mut self.channels[ch];
                if x != 0 {
                    c.mem_r = (c.mem_r & 0x0F) | (x << 4);
                }
                if y != 0 {
                    c.mem_r = (c.mem_r & 0xF0) | y;
                }
            }
            FX_S => self.special_tick0(ch, param),
            FX_T => {
                let c = &mut self.channels[ch];
                let p = Self::mem(&mut c.mem_t, param);
                if p >= 0x20 {
                    self.tempo = p;
                }
            }
            FX_V => {
                self.global_volume = param.min(128);
            }
            FX_W => {
                let c = &mut self.channels[ch];
                let p = Self::mem(&mut c.mem_w, param);
                let (px, py) = (p >> 4, p & 0x0F);
                // "Order of testing: Wx0, W0x, WxF, WFx".
                if py == 0 || px == 0 {
                } else if py == 0xF {
                    self.global_volume = (self.global_volume + px).min(128);
                } else if px == 0xF {
                    self.global_volume = self.global_volume.saturating_sub(py);
                }
            }
            FX_X => {
                // Xxx set panning: 0x00 left … 0xFF right, onto 0..=64.
                let c = &mut self.channels[ch];
                c.pan = ((param as u16 + 2) / 4).min(64) as u8;
                c.surround = false;
            }
            FX_Y => {
                let c = &mut self.channels[ch];
                if x != 0 {
                    c.mem_y = (c.mem_y & 0x0F) | (x << 4);
                }
                if y != 0 {
                    c.mem_y = (c.mem_y & 0xF0) | y;
                }
            }
            FX_Z => {
                // Zxx MIDI macro: the staged text pins no macro
                // semantics; documented no-op.
            }
            _ => {}
        }
    }

    fn special_tick0(&mut self, ch: usize, param: u8) {
        // "S00" reuses the last Sxx parameter (S3M-style memory).
        let p = {
            let c = &mut self.channels[ch];
            Self::mem(&mut c.mem_s, param)
        };
        let x = p >> 4;
        let y = p & 0x0F;
        match x {
            0x1 => self.channels[ch].glissando = y != 0,
            0x2 => {
                // "Set finetune (=C4Spd)": retune the sounding voice's
                // C5 speed to the ST3 table entry.
                let c5 = IT_S2X_FINETUNE_TABLE[y as usize];
                let c = &mut self.channels[ch];
                let old = c.c5_speed.max(1);
                c.freq = c.freq * c5 as f64 / old as f64;
                c.porta_target = c.porta_target * c5 as f64 / old as f64;
                c.c5_speed = c5;
            }
            0x3 => self.channels[ch].vib_wave = y & 3,
            0x4 => self.channels[ch].trem_wave = y & 3,
            0x5 => self.channels[ch].panb_wave = y & 3,
            0x6 => self.tick_delay = self.tick_delay.saturating_add(y),
            0x7 => self.nna_control(ch, y),
            0x8 => {
                // "Set channel pan position … 0 being left and F being
                // right": nibble → 0..=64.
                let c = &mut self.channels[ch];
                c.pan = ((y as u16 * 64 + 7) / 15).min(64) as u8;
                c.surround = false;
            }
            0x9 => {
                // Sound control: S91 = surround on, S90 = off.
                let c = &mut self.channels[ch];
                match y {
                    0 => c.surround = false,
                    1 => c.surround = true,
                    _ => {}
                }
            }
            0xA => self.channels[ch].high_offset = y,
            0xB => {
                // Pattern loop: SB0 sets the loop row; SBx (x>0) jumps
                // back x times.
                let c = &mut self.channels[ch];
                if y == 0 {
                    c.loop_row = self.row;
                } else if c.loop_count == 0 {
                    c.loop_count = y;
                    self.pending_loop_row = Some(c.loop_row);
                } else {
                    c.loop_count -= 1;
                    if c.loop_count > 0 {
                        self.pending_loop_row = Some(c.loop_row);
                    }
                }
            }
            0xC => {
                // SC0 cuts immediately (nothing is heard).
                if y == 0 {
                    self.note_cut(ch);
                } else {
                    self.channels[ch].note_cut_tick = Some(y);
                }
            }
            0xD => {
                // SD0 = play now (already handled as an ordinary row).
            }
            0xE if !self.in_pattern_delay_replay && self.pattern_delay == 0 => {
                self.pattern_delay = y;
            }
            _ => {}
        }
    }

    /// `S7x` New Note Action control.
    fn nna_control(&mut self, ch: usize, y: u8) {
        let instrument_mode = self.instrument_mode;
        match y {
            // S70 / S71 / S72: past-note cut / off / fade — act on the
            // background voices of this host channel.
            0..=2 => {
                let module = &self.module;
                for v in self.voices.iter_mut() {
                    if !v.active || !v.background || v.host as usize != ch {
                        continue;
                    }
                    match y {
                        0 => v.active = false,
                        1 => Self::release_voice(v, instrument_mode, module),
                        _ => v.fading = true,
                    }
                }
            }
            // S73..S76: set the NNA of the current note.
            3 => self.set_voice_nna(ch, ItNna::Cut),
            4 => self.set_voice_nna(ch, ItNna::Continue),
            5 => self.set_voice_nna(ch, ItNna::NoteOff),
            6 => self.set_voice_nna(ch, ItNna::NoteFade),
            // S77/S78 volume envelope off/on, S79/S7A panning, S7B/S7C
            // pitch.
            7 | 8 => self.set_env_enabled(ch, 0, y == 8),
            9 | 0xA => self.set_env_enabled(ch, 1, y == 0xA),
            0xB | 0xC => self.set_env_enabled(ch, 2, y == 0xC),
            _ => {}
        }
    }

    fn set_voice_nna(&mut self, ch: usize, nna: ItNna) {
        self.channels[ch].nna_override = Some(nna);
    }

    fn set_env_enabled(&mut self, ch: usize, which: u8, on: bool) {
        let Some(vi) = self.channels[ch].voice else {
            return;
        };
        let v = &mut self.voices[vi];
        let e = match which {
            0 => &mut v.vol_env,
            1 => &mut v.pan_env,
            _ => &mut v.pitch_env,
        };
        e.enabled = on;
    }

    // ------------------------------------------------------------------
    // Per-tick effects (ticks 1..speed-1, plus the every-tick vibrato)
    // ------------------------------------------------------------------

    fn effects_tick(&mut self, ch: usize, first_tick: bool) {
        let cmd = self.channels[ch].cmd;
        let param = self.channels[ch].param;
        let volcol = self.channels[ch].volcol;

        // Note delay fires its cell on its tick.
        if let Some(t) = self.channels[ch].note_delay_tick {
            if self.tick == t {
                self.channels[ch].note_delay_tick = None;
                let cell = self.channels[ch].delayed_cell;
                let mut cell = cell;
                // Strip the SDx so the delayed cell's own effect (none)
                // does not re-schedule.
                cell.mask &= !crate::it::IT_CELL_COMMAND;
                self.process_cell(ch, cell);
                self.channels[ch].cmd = FX_S;
                self.channels[ch].param = param;
            }
        }
        if let Some(t) = self.channels[ch].note_cut_tick {
            if self.tick == t {
                self.channels[ch].note_cut_tick = None;
                self.note_cut(ch);
            }
        }

        let linear = self.linear_slides;
        // ---- vibrato: every tick in IT mode, non-row ticks in old mode ----
        let vib_active =
            matches!(cmd, FX_H | FX_U | FX_K) || matches!(volcol, ItVolumeColumn::Vibrato(_));
        if vib_active && (!first_tick || !self.old_effects) {
            let rnd = self.rand_wave();
            let old = self.old_effects;
            let c = &mut self.channels[ch];
            let depth = c.mem_h_depth as i32;
            // The position advances BEFORE the table read (the row tick
            // already sounds at `speed`, verified against the black-box
            // oracle tick by tick).
            c.vib_pos = c.vib_pos.wrapping_add(c.mem_h_speed);
            let w = waveform(c.vib_wave, c.vib_pos, rnd);
            // Vibrato depth is expressed in the fine-slide units of the
            // slide mode: `table(±64) * depth / 64` → ±depth units
            // (linear 1/64-semitone steps, or Amiga period units). Under
            // "Old Effects" the vibrato is "played in the normal manner"
            // — the other trackers' period-domain manner, where a
            // positive table value raises the period and lowers the
            // pitch — so the sign flips.
            let d = (w * depth) as f64 / 64.0;
            c.vib_delta = if old { -d } else { d };
        } else if !(vib_active && first_tick) {
            // Old Effects keep the last delta across the row tick ("it
            // is updated every non-row frame"); otherwise no vibrato.
            self.channels[ch].vib_delta = 0.0;
        }

        // ---- tremolo (Rxy) ----
        if cmd == FX_R {
            let rnd = self.rand_wave();
            let c = &mut self.channels[ch];
            if !first_tick || !self.old_effects {
                let speed = (c.mem_r >> 4) * 4;
                let depth = (c.mem_r & 0x0F) as i32;
                let w = waveform(c.trem_wave, c.trem_pos, rnd);
                c.trem_delta = w * depth / 32;
                c.trem_pos = c.trem_pos.wrapping_add(speed);
            }
        } else {
            self.channels[ch].trem_delta = 0;
        }

        // ---- panbrello (Yxy): "a table 4 times larger" ----
        if cmd == FX_Y {
            let rnd = self.rand_wave();
            let c = &mut self.channels[ch];
            let speed = (c.mem_y >> 4) as u16;
            let depth = (c.mem_y & 0x0F) as i32;
            if c.panb_wave == 3 {
                // "If the waveform is set to random, then the 'speed'
                // part of the command is interpreted as a delay."
                if c.panb_delay == 0 {
                    c.panb_random = rnd;
                    c.panb_delay = speed as u8;
                } else {
                    c.panb_delay -= 1;
                }
                c.panb_delta = c.panb_random as i32 * depth / 32;
            } else {
                // "This uses a table 4 times larger (hence 4 times
                // slower) than vibrato": the position steps by `x` per
                // tick where vibrato steps by `4x`, so one cycle takes
                // 256/x ticks. Depth scales like tremolo.
                // Unlike vibrato, the table is read at the current
                // position and advanced afterwards (tick 0 of a new
                // note sits at the channel pan).
                let w = waveform(c.panb_wave, c.panb_pos as u8, rnd);
                c.panb_delta = w * depth / 32;
                c.panb_pos = c.panb_pos.wrapping_add(speed);
            }
        } else {
            self.channels[ch].panb_delta = 0;
        }

        // ---- tremor (Ixy): every tick, on for x then off for y ----
        if cmd == FX_I {
            let c = &mut self.channels[ch];
            let p = c.mem_i;
            let mut on = p >> 4;
            let mut off = p & 0x0F;
            if self.old_effects {
                on += 1;
                off += 1;
            }
            let period = (on as u16 + off as u16).max(1);
            c.tremor_off = (c.tremor_pos as u16) >= on as u16;
            c.tremor_pos = ((c.tremor_pos as u16 + 1) % period) as u8;
        } else {
            self.channels[ch].tremor_off = false;
        }

        // ---- retrig (Qxy): the counter runs every tick, across rows ----
        if cmd == FX_Q {
            if self.channels[ch].note_triggered {
                self.channels[ch].note_triggered = false;
            } else {
                self.retrig_tick(ch);
            }
        } else {
            self.channels[ch].note_triggered = false;
        }

        if first_tick {
            return;
        }

        // ---- volume column per-tick ----
        match volcol {
            ItVolumeColumn::VolumeSlideUp(_) => {
                let c = &mut self.channels[ch];
                c.volume = (c.volume + c.mem_volcol_slide).min(64);
            }
            ItVolumeColumn::VolumeSlideDown(_) => {
                let c = &mut self.channels[ch];
                c.volume = c.volume.saturating_sub(c.mem_volcol_slide);
            }
            ItVolumeColumn::PitchSlideUp(_) => {
                let c = &mut self.channels[ch];
                c.freq = clamp_freq(slide(c.freq, 4.0 * c.mem_ef as f64, linear));
            }
            ItVolumeColumn::PitchSlideDown(_) => {
                let c = &mut self.channels[ch];
                c.freq = clamp_freq(slide(c.freq, -4.0 * c.mem_ef as f64, linear));
            }
            ItVolumeColumn::TonePortamento(_) => self.tone_porta_step(ch),
            _ => {}
        }

        match cmd {
            FX_D | FX_K | FX_L => {
                let c = &mut self.channels[ch];
                let p = c.mem_d;
                let (px, py) = (p >> 4, p & 0x0F);
                if py == 0 {
                    c.volume = (c.volume + px).min(64);
                } else if px == 0 {
                    c.volume = c.volume.saturating_sub(py);
                }
                if cmd == FX_L {
                    self.tone_porta_step(ch);
                }
            }
            FX_E | FX_F => {
                let c = &mut self.channels[ch];
                let p = c.mem_ef;
                if p < 0xE0 {
                    let units = 4.0 * p as f64;
                    let u = if cmd == FX_E { -units } else { units };
                    c.freq = clamp_freq(slide(c.freq, u, linear));
                }
            }
            FX_G => self.tone_porta_step(ch),
            FX_J => {
                let c = &mut self.channels[ch];
                let p = c.mem_j;
                c.arp_offset = match self.tick % 3 {
                    1 => p >> 4,
                    2 => p & 0x0F,
                    _ => 0,
                };
            }
            FX_N => {
                let c = &mut self.channels[ch];
                let p = c.mem_n;
                let (px, py) = (p >> 4, p & 0x0F);
                if py == 0 {
                    c.channel_volume = (c.channel_volume + px).min(64);
                } else if px == 0 {
                    c.channel_volume = c.channel_volume.saturating_sub(py);
                }
            }
            FX_P => {
                let c = &mut self.channels[ch];
                let p = c.mem_p;
                let (px, py) = (p >> 4, p & 0x0F);
                if c.pan != IT_PAN_SURROUND {
                    if py == 0 {
                        c.pan = c.pan.saturating_sub(px);
                    } else if px == 0 {
                        c.pan = (c.pan + py).min(64);
                    }
                }
            }
            FX_T => {
                let p = self.channels[ch].mem_t;
                // T0x tempo slide down, T1x tempo slide up, per tick.
                match p >> 4 {
                    0 => self.tempo = self.tempo.saturating_sub(p & 0x0F).max(32),
                    1 => self.tempo = self.tempo.saturating_add(p & 0x0F),
                    _ => {}
                }
            }
            FX_W => {
                let p = self.channels[ch].mem_w;
                let (px, py) = (p >> 4, p & 0x0F);
                if py == 0 {
                    self.global_volume = (self.global_volume + px).min(128);
                } else if px == 0 {
                    self.global_volume = self.global_volume.saturating_sub(py);
                }
            }
            _ => {}
        }
        let _ = param;
    }

    fn tone_porta_step(&mut self, ch: usize) {
        let linear = self.linear_slides;
        let c = &mut self.channels[ch];
        let units = 4.0 * c.mem_g as f64;
        if units == 0.0 || c.porta_target <= 0.0 {
            return;
        }
        let target = c.porta_target;
        if c.freq < target {
            c.freq = slide(c.freq, units, linear).min(target);
        } else if c.freq > target {
            c.freq = slide(c.freq, -units, linear).max(target);
        }
        if c.glissando {
            c.freq = snap_to_semitone(c.freq, c.c5_speed.max(1));
        }
        c.freq = clamp_freq(c.freq);
    }

    /// `Qxy`: retrigger every `y` ticks with the S3M volume table.
    fn retrig_tick(&mut self, ch: usize) {
        let p = self.channels[ch].mem_q;
        let (x, y) = (p >> 4, p & 0x0F);
        if y == 0 {
            return;
        }
        let c = &mut self.channels[ch];
        c.retrig_count += 1;
        if c.retrig_count < y {
            return;
        }
        c.retrig_count = 0;
        let v = c.volume as i32;
        let nv = match x {
            1 => v - 1,
            2 => v - 2,
            3 => v - 4,
            4 => v - 8,
            5 => v - 16,
            6 => v * 2 / 3,
            7 => v / 2,
            9 => v + 1,
            0xA => v + 2,
            0xB => v + 4,
            0xC => v + 8,
            0xD => v + 16,
            0xE => v * 3 / 2,
            0xF => v * 2,
            _ => v,
        };
        c.volume = nv.clamp(0, 64) as u8;
        if let Some(vi) = c.voice {
            let v = &mut self.voices[vi];
            if v.active && !v.background {
                v.mixer.pos = 0.0;
                v.mixer.direction = 1;
            }
        }
    }

    // ------------------------------------------------------------------
    // Tick update: envelopes, fadeout, final volume / pan / pitch
    // ------------------------------------------------------------------

    fn update_voices(&mut self) {
        let instrument_mode = self.instrument_mode;
        let global = self.global_volume as i64;
        let linear = self.linear_slides;
        let rnd = self.rand_wave();
        for vi in 0..self.voices.len() {
            if !self.voices[vi].active {
                continue;
            }
            let host = self.voices[vi].host as usize;
            let (fg_vol, fg_pan, fg_freq, cv, muted, vib, trem, panb, arp, tremor_off, c5) = {
                let c = &self.channels[host];
                (
                    c.volume,
                    c.pan,
                    c.freq,
                    c.channel_volume as i64,
                    c.muted,
                    c.vib_delta,
                    c.trem_delta,
                    c.panb_delta,
                    c.arp_offset,
                    c.tremor_off,
                    c.c5_speed,
                )
            };
            let foreground = !self.voices[vi].background;
            let sample_gv = self
                .sample_for(self.voices[vi].sample)
                .map_or(64, |s| s.global_volume as i64);
            let ins = if instrument_mode {
                self.instrument_for(self.voices[vi].instrument).cloned()
            } else {
                None
            };
            let v = &mut self.voices[vi];
            if foreground {
                v.volume = fg_vol;
                v.pan = fg_pan;
                v.freq = fg_freq;
            }

            // Note volume with tremolo / tremor applied (foreground only).
            let mut vol = v.volume as i64;
            if foreground {
                vol = (vol + trem as i64).clamp(0, 64);
                if tremor_off {
                    vol = 0;
                }
            }

            let mut fv: i64;
            let mut pan = v.pan as i32;
            let mut freq = v.freq;
            if let Some(ins) = &ins {
                // 1) volume envelope, 2) end → fade, 3) NFC -= FadeOut.
                let vev = if v.vol_env.enabled {
                    v.vol_env.step(&ins.volume_envelope, !v.released) / 256
                } else {
                    64
                };
                if v.vol_env.enabled && v.vol_env.ended {
                    v.fading = true;
                }
                if v.fading {
                    v.fade = v.fade.saturating_sub(ins.fadeout);
                }
                // 4) FV = Vol*SV*IV*CV*GV*VEV*NFC / 2^41
                fv = (vol
                    * sample_gv
                    * ins.global_volume as i64
                    * cv
                    * global
                    * vev.clamp(0, 64) as i64
                    * v.fade as i64)
                    >> 41;
                // Panning envelope: -32..+32, scaled by the room left
                // toward the nearer edge so a full-swing envelope never
                // overshoots: `pan + env * (32 - |pan - 32|) / 32`.
                if v.pan_env.enabled && pan != IT_PAN_SURROUND as i32 {
                    let pe = v.pan_env.step(&ins.panning_envelope, !v.released) / 256;
                    let room = 32 - (pan - 32).abs();
                    pan = (pan + pe * room / 32).clamp(0, 64);
                }
                // Pitch envelope: each unit is a half semitone — 32
                // fine-slide units under the 768/octave linear system.
                if v.pitch_env.enabled {
                    let pe = v.pitch_env.step(&ins.pitch_envelope, !v.released);
                    freq = linear_slide(freq, pe as f64 * 32.0 / 256.0);
                }
                // Random volume variation.
                fv = fv * v.random_vol_x256 as i64 / 256;
                // A fully faded voice is done.
                if v.fade == 0 && v.fading {
                    v.active = false;
                }
            } else {
                // Sample mode: FV = Vol * SV * CV * GV / 2^18.
                fv = (vol * sample_gv * cv * global) >> 18;
            }

            // Sample vibrato ("the depth is basically the running-sum of
            // the rate divided by 256", applied as a fine linear slide).
            let smp = self.module.samples.get(v.sample as usize - 1);
            if let Some(smp) = smp {
                if smp.vibrato_depth > 0 && smp.vibrato_rate > 0 {
                    let sum = v.sample_vib_sweep as u32 + smp.vibrato_rate as u32;
                    v.sample_vib_sweep = sum.min(0xFFFF) as u16;
                    let depth = ((sum >> 8) as u8).min(smp.vibrato_depth) as i32;
                    let w = waveform(smp.vibrato_wave as u8, v.sample_vib_pos, rnd);
                    let delta = (w * depth) as f64 / 64.0;
                    freq = linear_slide(freq, delta);
                    if smp.vibrato_wave != ItVibratoWave::Random {
                        v.sample_vib_pos = v.sample_vib_pos.wrapping_add(smp.vibrato_speed);
                    }
                }
            }

            // Foreground per-tick pitch modifiers.
            if foreground {
                if arp != 0 {
                    let n = (v.note as u16 + arp as u16).min(IT_MAX_NOTE as u16) as u8;
                    let base = note_frequency(v.note, c5.max(1));
                    if base > 0.0 {
                        freq *= note_frequency(n, c5.max(1)) / base;
                    }
                }
                if vib != 0.0 {
                    freq = slide(freq, vib, linear);
                }
                if panb != 0 && pan != IT_PAN_SURROUND as i32 {
                    pan = (pan + panb).clamp(0, 64);
                }
            }

            v.amp = if muted {
                0.0
            } else {
                (fv.clamp(0, 128) as f32) / 128.0
            };
            v.final_pan = pan.clamp(0, 100) as u8;
            v.play_freq = clamp_freq(freq);
            v.mixer.freq = v.play_freq as f32;
            v.mixer.volume = 1.0;
        }
    }

    /// Advance one tick: row processing on tick 0, per-tick effects
    /// otherwise, then the voice update.
    fn advance_tick(&mut self) {
        if self.tick == 0 {
            if !self.settle_order() {
                self.ended = true;
                return;
            }
            if !self.in_pattern_delay_replay {
                self.process_row();
            }
            for ch in 0..self.channels.len() {
                self.effects_tick(ch, true);
            }
        } else {
            for ch in 0..self.channels.len() {
                self.effects_tick(ch, false);
            }
        }
        self.update_voices();
    }

    /// Render interleaved stereo `i16` into `dst`; returns frames
    /// produced (0 once the song has ended).
    pub fn render(&mut self, dst: &mut [i16]) -> usize {
        assert!(dst.len() % 2 == 0);
        let total_frames = dst.len() / 2;
        let mut produced = 0usize;
        let out_rate = self.sample_rate as f32;
        let mix_vol = self.module.header.mix_volume.min(128) as f32 / 128.0;
        let sep = self.module.header.pan_separation.min(128) as i32;

        while produced < total_frames {
            if self.ended {
                break;
            }
            if self.tick_sample_cursor == 0 {
                self.advance_tick();
                if self.ended {
                    break;
                }
            }
            let spt = self.samples_per_tick().max(1);
            let remaining = spt.saturating_sub(self.tick_sample_cursor);
            let want = (total_frames - produced).min(remaining as usize);

            for _ in 0..want {
                let mut l = 0.0f32;
                let mut r = 0.0f32;
                for v in self.voices.iter_mut() {
                    if !v.active {
                        continue;
                    }
                    let Some(sample) = self.module.samples.get(v.sample as usize - 1) else {
                        v.active = false;
                        continue;
                    };
                    let view = ItLoopView {
                        sample,
                        sustain: !v.released,
                    };
                    let s = v.mixer.render_one(&view, out_rate) * v.amp;
                    if !v.mixer.active {
                        v.active = false;
                    }
                    let pan = if v.final_pan == IT_PAN_SURROUND {
                        32
                    } else {
                        32 + (v.final_pan as i32 - 32) * sep / 128
                    };
                    let pf = pan.clamp(0, 64) as f32 / 64.0;
                    l += s * (1.0 - pf);
                    r += s * pf;
                }
                let l = (l * mix_vol).clamp(-1.0, 1.0);
                let r = (r * mix_vol).clamp(-1.0, 1.0);
                let off = produced * 2;
                dst[off] = (l * 32767.0) as i16;
                dst[off + 1] = (r * 32767.0) as i16;
                produced += 1;
            }

            self.tick_sample_cursor += want as u32;
            if self.tick_sample_cursor >= spt {
                self.tick_sample_cursor = 0;
                self.tick += 1;
                let row_ticks = self.speed.saturating_add(self.tick_delay);
                if self.tick >= row_ticks.max(1) {
                    self.tick = 0;
                    self.tick_delay = 0;
                    if self.pattern_delay > 0 {
                        self.pattern_delay -= 1;
                        self.in_pattern_delay_replay = true;
                    } else {
                        self.in_pattern_delay_replay = false;
                        self.next_row();
                    }
                }
            }
        }
        produced
    }

    /// Number of currently sounding voices (foreground + background).
    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|v| v.active).count()
    }
}

/// Snap `freq` to the nearest note of the pitch table for `c5_speed`
/// (glissando control `S1x`).
fn snap_to_semitone(freq: f64, c5_speed: u32) -> f64 {
    if freq <= 0.0 {
        return freq;
    }
    // Semitones above C-5.
    let semis = 12.0 * (freq / c5_speed as f64).log2();
    let n = (semis.round() as i32 + IT_C5_NOTE as i32).clamp(0, IT_MAX_NOTE as i32) as u8;
    note_frequency(n, c5_speed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::it::tests::{
        build_module, build_new_instrument, build_pattern, build_sample_header, cmd_cell, note_cell,
    };
    use crate::it::{
        parse_module, ItVolumeColumn, IT_CELL_COMMAND, IT_CELL_NOTE, IT_CELL_VOLPAN, IT_CVT_SIGNED,
        IT_FLAG_INSTRUMENTS, IT_FLAG_LINEAR_SLIDES, IT_SMP_HAS_SAMPLE, IT_SMP_LOOP,
        IT_SMP_SUSTAIN_LOOP,
    };

    const RATE: u32 = 44_100;

    /// A 64-frame square wave body (signed 8-bit).
    pub(crate) fn square_body() -> Vec<u8> {
        (0..64u8)
            .map(|i| if i < 32 { 0x60 } else { 0xA0 })
            .collect()
    }

    /// Looped square-wave sample header (loop over the whole body).
    pub(crate) fn square_sample(extra_flags: u8) -> (Vec<u8>, Vec<u8>) {
        (
            build_sample_header(
                "sq",
                IT_SMP_HAS_SAMPLE | IT_SMP_LOOP | extra_flags,
                IT_CVT_SIGNED,
                64,
                (0, 64, 0, 64),
                8363,
                0,
            ),
            square_body(),
        )
    }

    /// Sample-mode module with one looped square sample and the given
    /// patterns.
    pub(crate) fn sample_mode_module(flags: u16, patterns: &[Vec<u8>], orders: &[u8]) -> Vec<u8> {
        build_module(orders, flags, &[], &[square_sample(0)], patterns)
    }

    /// The simplest playable module: C-5 on channel 0 row 0.
    pub(crate) fn build_ping_it() -> Vec<u8> {
        let pat = build_pattern(8, &[(0, 0, note_cell(60, 1))]);
        sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0])
    }

    fn player(bytes: &[u8]) -> ItPlayerState {
        ItPlayerState::new(parse_module(bytes).unwrap(), RATE)
    }

    fn render_all(p: &mut ItPlayerState, max_frames: usize) -> Vec<i16> {
        let mut out = Vec::new();
        let mut buf = vec![0i16; 2048];
        while out.len() / 2 < max_frames {
            let n = p.render(&mut buf);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n * 2]);
        }
        out
    }

    #[test]
    fn pitch_table_anchors() {
        assert_eq!(IT_PITCH_TABLE[60], 65536, "C-5 is unity");
        assert_eq!(IT_PITCH_TABLE[0], 2048, "C-0 is five octaves down");
        assert_eq!(IT_PITCH_TABLE[119], (30 << 16) | 13368);
        // Every octave doubles (integer table; allow ±1 rounding).
        for n in 0..108 {
            let a = IT_PITCH_TABLE[n] as i64 * 2;
            let b = IT_PITCH_TABLE[n + 12] as i64;
            assert!((a - b).abs() <= 2, "note {n}: {a} vs {b}");
        }
        assert!((note_frequency(60, 8363) - 8363.0).abs() < 1e-9);
        assert!((note_frequency(72, 8363) - 16726.0).abs() < 1e-6);
    }

    #[test]
    fn slide_formulas() {
        // Linear: 768 units = one octave.
        assert!((linear_slide(1000.0, 768.0) - 2000.0).abs() < 1e-9);
        assert!((linear_slide(1000.0, -768.0) - 500.0).abs() < 1e-9);
        // Amiga: adding to the period lowers the pitch.
        let f = 8363.0;
        assert!(amiga_slide(f, 4.0) < f);
        assert!(amiga_slide(f, -4.0) > f);
        assert!((amiga_slide(f, 0.0) - f).abs() < 1e-6);
        // `slide` maps positive units to a higher pitch in both modes.
        assert!(slide(f, 16.0, true) > f);
        assert!(slide(f, 16.0, false) > f);
        assert_eq!(amiga_slide(0.0, 4.0), 0.0);
        assert!((snap_to_semitone(8400.0, 8363) - 8363.0).abs() < 1e-6);
        assert!((snap_to_semitone(8363.0 * 1.06, 8363) - note_frequency(61, 8363)).abs() < 1e-6);
    }

    #[test]
    fn tables_have_expected_shape() {
        assert_eq!(IT_FINE_SINE[0], 0);
        assert_eq!(IT_FINE_SINE[64], 64);
        assert_eq!(IT_FINE_SINE[128], 0);
        assert_eq!(IT_FINE_SINE[192], -64);
        assert_eq!(IT_FINE_RAMP_DOWN[0], 64);
        assert_eq!(IT_FINE_RAMP_DOWN[255], -64);
        assert_eq!(it_fine_square(0), 64);
        assert_eq!(it_fine_square(200), 0);
        assert_eq!(IT_S2X_FINETUNE_TABLE[8], 8363);
    }

    #[test]
    fn ping_renders_audio_and_ends() {
        let mut p = player(&build_ping_it());
        assert!(!p.instrument_mode);
        assert_eq!(p.channels.len(), 1);
        let pcm = render_all(&mut p, 400_000);
        assert!(pcm.iter().any(|&s| s != 0), "audible output expected");
        assert!(p.ended, "one pass through the order list ends the song");
        // 8 rows × 6 ticks × (2.5/125 s) = 0.96 s.
        let frames = pcm.len() / 2;
        let expected = (RATE as f64 * 0.96) as usize;
        assert!(
            (frames as i64 - expected as i64).abs() < 64,
            "frames {frames}"
        );
    }

    #[test]
    fn samples_per_tick_follows_tempo() {
        let mut p = player(&build_ping_it());
        assert_eq!(p.samples_per_tick(), 882);
        p.tempo = 250;
        assert_eq!(p.samples_per_tick(), 441);
    }

    #[test]
    fn axx_sets_speed_and_txx_sets_tempo() {
        let pat = build_pattern(
            4,
            &[
                (0, 0, note_cell(60, 1)),
                (0, 1, cmd_cell('A', 3)),
                (1, 1, cmd_cell('T', 0xFA)),
                (2, 1, cmd_cell('A', 0)),
            ],
        );
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0]);
        let mut p = player(&m);
        // Tick 0 of row 0.
        p.render(&mut [0i16; 2]);
        assert_eq!(p.speed, 3);
        assert_eq!(p.tempo, 125);
        // Advance into row 1 (3 ticks × 882 frames).
        render_all_frames(&mut p, 3 * 882);
        assert_eq!(p.row, 1);
        assert_eq!(p.tempo, 250);
        render_all_frames(&mut p, 3 * 441);
        assert_eq!(p.row, 2);
        assert_eq!(p.speed, 3, "A00 leaves the speed alone");
    }

    pub(crate) fn render_all_frames(p: &mut ItPlayerState, frames: usize) {
        let mut buf = vec![0i16; frames * 2];
        let mut done = 0;
        while done < frames {
            let n = p.render(&mut buf[done * 2..]);
            if n == 0 {
                break;
            }
            done += n;
        }
    }

    #[test]
    fn bxx_and_cxx_flow() {
        // Order 0: pattern 0 (4 rows), row 1 has B02 → jump to order 2.
        // Order 2: pattern 1 (8 rows), row 0 has C03 → next order row 3.
        // Order 3: pattern 2 (8 rows).
        let p0 = build_pattern(4, &[(0, 0, note_cell(60, 1)), (1, 0, cmd_cell('B', 2))]);
        let p1 = build_pattern(8, &[(0, 0, cmd_cell('C', 3))]);
        let p2 = build_pattern(8, &[]);
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[p0, p1, p2], &[0, 0, 1, 2]);
        let mut p = player(&m);
        render_all_frames(&mut p, 2 * 6 * 882);
        assert_eq!((p.order_index, p.row), (2, 0));
        render_all_frames(&mut p, 6 * 882);
        assert_eq!((p.order_index, p.row), (3, 3));
        // Break past the end of the pattern lands on row 0 of order 3
        // after row 3..7 play; then the song ends.
        render_all_frames(&mut p, 5 * 6 * 882);
        assert!(p.ended);
    }

    #[test]
    fn order_skip_and_end_markers() {
        let p0 = build_pattern(2, &[(0, 0, note_cell(60, 1))]);
        let m = sample_mode_module(
            IT_FLAG_LINEAR_SLIDES,
            &[p0],
            &[IT_ORDER_SKIP, 0, IT_ORDER_END, 0],
        );
        let mut p = player(&m);
        p.render(&mut [0i16; 2]);
        assert_eq!(p.order_index, 1, "+++ is skipped at start");
        render_all_frames(&mut p, 2 * 6 * 882);
        assert!(p.ended, "--- ends the song");
        assert_eq!(
            p.order_index, 1,
            "wrap to order 0 after the end marker, then +++ is skipped again"
        );
    }

    #[test]
    fn dxx_volume_slides_all_four_forms() {
        let mk = |param: u8| {
            let mut c = note_cell(60, 1);
            c.mask |= IT_CELL_COMMAND;
            c.command = 4;
            c.param = param;
            c
        };
        // D0F: tick 0 minus 15, then minus 15 per tick? No: D0F matches
        // "D0x with x = F" → slide down 15 straight away AND every tick.
        let pat = build_pattern(
            1,
            &[
                (0, 0, mk(0x0F)),
                (0, 1, mk(0x40)),
                (0, 2, mk(0x2F)),
                (0, 3, mk(0xF2)),
                (0, 4, mk(0x03)),
            ],
        );
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0]);
        let mut p = player(&m);
        p.render(&mut [0i16; 2]);
        assert_eq!(p.channels[0].volume, 49);
        assert_eq!(p.channels[1].volume, 64, "Dx0 does nothing on tick 0");
        assert_eq!(p.channels[2].volume, 64, "DxF: +2 clamps at 64");
        assert_eq!(p.channels[3].volume, 62, "DFx: -2 on tick 0");
        assert_eq!(p.channels[4].volume, 64);
        render_all_frames(&mut p, 882);
        assert_eq!(p.channels[0].volume, 34, "D0F keeps sliding 15/tick");
        assert_eq!(p.channels[1].volume, 64);
        assert_eq!(p.channels[2].volume, 64, "fine slides are tick-0 only");
        assert_eq!(p.channels[3].volume, 62);
        assert_eq!(p.channels[4].volume, 61, "D03: -3 per tick");
        render_all_frames(&mut p, 4 * 882);
        assert_eq!(p.channels[4].volume, 49);
    }

    #[test]
    fn exx_fxx_linear_slides_with_memory_and_fine_forms() {
        let mk = |letter: char, param: u8| {
            let mut c = note_cell(60, 1);
            c.mask |= IT_CELL_COMMAND;
            c.command = letter as u8 - b'@';
            c.param = param;
            c
        };
        let pat = build_pattern(
            2,
            &[
                (0, 0, mk('F', 0x10)),
                (0, 1, mk('E', 0x10)),
                (0, 2, mk('F', 0xF4)),
                (0, 3, mk('E', 0xE4)),
                (1, 0, cmd_cell('F', 0)),
            ],
        );
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0]);
        let mut p = player(&m);
        p.render(&mut [0i16; 2]);
        let base = 8363.0;
        assert!((p.channels[0].freq - base).abs() < 1e-6);
        assert!(
            (p.channels[2].freq - linear_slide(base, 16.0)).abs() < 1e-6,
            "FF4 = +16 once"
        );
        assert!(
            (p.channels[3].freq - linear_slide(base, -4.0)).abs() < 1e-6,
            "EE4 = -4 once"
        );
        render_all_frames(&mut p, 882);
        assert!(
            (p.channels[0].freq - linear_slide(base, 64.0)).abs() < 1e-6,
            "F10 = +64/tick"
        );
        assert!((p.channels[1].freq - linear_slide(base, -64.0)).abs() < 1e-6);
        assert!(
            (p.channels[2].freq - linear_slide(base, 16.0)).abs() < 1e-6,
            "fine: once only"
        );
        // Row 1: F00 reuses 0x10.
        render_all_frames(&mut p, 5 * 882);
        let after_row0 = linear_slide(base, 5.0 * 64.0);
        assert!((p.channels[0].freq - after_row0).abs() < 1e-3);
        render_all_frames(&mut p, 882);
        assert!((p.channels[0].freq - linear_slide(after_row0, 64.0)).abs() < 1e-3);
    }

    #[test]
    fn amiga_slides_move_the_period() {
        let mut c = note_cell(60, 1);
        c.mask |= IT_CELL_COMMAND;
        c.command = FX_F;
        c.param = 0x01;
        let pat = build_pattern(1, &[(0, 0, c)]);
        let m = sample_mode_module(0, &[pat], &[0]);
        let mut p = player(&m);
        assert!(!p.linear_slides);
        p.render(&mut [0i16; 2]);
        let f0 = p.channels[0].freq;
        render_all_frames(&mut p, 882);
        let f1 = p.channels[0].freq;
        let p0 = IT_AMIGA_PERIOD_CLOCK / f0;
        let p1 = IT_AMIGA_PERIOD_CLOCK / f1;
        assert!(
            (p0 - 1712.0).abs() < 0.1,
            "C-5 at 8363 Hz is ST3 period ~1712 (14317056 / 8363), got {p0}"
        );
        assert!(
            ((p0 - p1) - 4.0).abs() < 1e-6,
            "F01 = 4 period units per tick"
        );
    }

    #[test]
    fn gxx_tone_portamento_reaches_target_without_retrigger() {
        let mut g = note_cell(72, 0);
        g.mask = IT_CELL_NOTE | IT_CELL_COMMAND;
        g.command = FX_G;
        g.param = 0x20; // 128 units/tick → 6 ticks to climb 768
        let pat = build_pattern(
            3,
            &[
                (0, 0, note_cell(60, 1)),
                (1, 0, g),
                (2, 0, cmd_cell('G', 0)),
            ],
        );
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0]);
        let mut p = player(&m);
        render_all_frames(&mut p, 6 * 882 + 1);
        assert_eq!(p.row, 1);
        assert!(
            (p.channels[0].freq - 8363.0).abs() < 1e-6,
            "no jump on the porta row"
        );
        assert!((p.channels[0].porta_target - 16726.0).abs() < 1e-6);
        let vi = p.channels[0].voice.unwrap();
        assert!(
            p.voices[vi].mixer.pos > 1.0,
            "the voice was not retriggered"
        );
        render_all_frames(&mut p, 5 * 882);
        assert!((p.channels[0].freq - linear_slide(8363.0, 5.0 * 128.0)).abs() < 1e-3);
        render_all_frames(&mut p, 6 * 882);
        assert!(
            (p.channels[0].freq - 16726.0).abs() < 1e-6,
            "clamped at the target"
        );
    }

    #[test]
    fn volume_column_forms() {
        let vc = |note: bool, v: u8| ItCell {
            mask: if note {
                IT_CELL_NOTE | IT_CELL_VOLPAN | crate::it::IT_CELL_INSTRUMENT
            } else {
                IT_CELL_VOLPAN
            },
            note: 60,
            instrument: 1,
            volpan: v,
            ..ItCell::default()
        };
        let pat = build_pattern(
            2,
            &[
                (0, 0, vc(true, 32)),
                (0, 1, vc(true, 65 + 5)),
                (0, 2, vc(true, 75 + 5)),
                (0, 3, vc(true, 85 + 2)),
                (0, 4, vc(true, 95 + 2)),
                (0, 5, vc(true, 128 + 10)),
                (1, 1, vc(false, 65)),
            ],
        );
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0]);
        let mut p = player(&m);
        p.render(&mut [0i16; 2]);
        assert_eq!(p.channels[0].volume, 32);
        assert_eq!(p.channels[1].volume, 64, "fine up from 64 clamps");
        assert_eq!(p.channels[2].volume, 59);
        assert_eq!(p.channels[3].volume, 64);
        assert_eq!(p.channels[4].volume, 64);
        assert_eq!(p.channels[5].pan, 10);
        assert_eq!(p.channels[5].volcol, ItVolumeColumn::Panning(10));
        render_all_frames(&mut p, 5 * 882);
        assert_eq!(p.channels[3].volume, 64);
        assert_eq!(p.channels[4].volume, 64 - 10);
        // Row 1 channel 1: fine-up 0 → memory (5) → 64 stays 64; use
        // channel 2's memory instead: not shared across channels.
        render_all_frames(&mut p, 882);
        assert_eq!(p.channels[1].volume, 64);
    }

    #[test]
    fn note_cut_and_note_off_in_sample_mode() {
        let pat = build_pattern(
            4,
            &[
                (0, 0, note_cell(60, 1)),
                (
                    1,
                    0,
                    ItCell {
                        mask: IT_CELL_NOTE,
                        note: crate::it::IT_NOTE_CUT,
                        ..ItCell::default()
                    },
                ),
                (0, 1, note_cell(60, 1)),
                (
                    1,
                    1,
                    ItCell {
                        mask: IT_CELL_NOTE,
                        note: crate::it::IT_NOTE_OFF,
                        ..ItCell::default()
                    },
                ),
            ],
        );
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0]);
        let mut p = player(&m);
        p.render(&mut [0i16; 2]);
        assert_eq!(p.active_voices(), 2);
        render_all_frames(&mut p, 6 * 882);
        assert_eq!(p.row, 1);
        // The cut voice is gone; the note-off voice keeps playing (no
        // sustain loop to release, nothing to fade in sample mode).
        assert_eq!(p.active_voices(), 1);
        let vi = p.channels[1].voice.unwrap();
        assert!(p.voices[vi].active && p.voices[vi].released);
    }

    #[test]
    fn note_off_releases_a_sustain_loop() {
        let (mut hdr, body) = square_sample(IT_SMP_SUSTAIN_LOOP);
        // Sustain loop 0..16, normal loop over the whole body.
        hdr[0x40..0x44].copy_from_slice(&0u32.to_le_bytes());
        hdr[0x44..0x48].copy_from_slice(&16u32.to_le_bytes());
        let pat = build_pattern(
            4,
            &[
                (0, 0, note_cell(60, 1)),
                (
                    1,
                    0,
                    ItCell {
                        mask: IT_CELL_NOTE,
                        note: crate::it::IT_NOTE_OFF,
                        ..ItCell::default()
                    },
                ),
            ],
        );
        let m = build_module(&[0], IT_FLAG_LINEAR_SLIDES, &[], &[(hdr, body)], &[pat]);
        let mut p = player(&m);
        render_all_frames(&mut p, 6 * 882 - 10);
        let vi = p.channels[0].voice.unwrap();
        assert!(
            p.voices[vi].mixer.pos < 16.0,
            "held: the sustain loop confines the cursor"
        );
        assert!(!p.voices[vi].released);
        render_all_frames(&mut p, 6 * 882);
        assert!(p.voices[vi].released);
        assert!(
            p.voices[vi].active,
            "released into the normal loop, still sounding"
        );
        assert!(
            p.voices[vi].mixer.pos >= 16.0,
            "past the sustain region after release"
        );
    }

    #[test]
    fn sbx_pattern_loop_and_sex_pattern_delay() {
        let pat = build_pattern(
            4,
            &[
                (0, 0, note_cell(60, 1)),
                (1, 0, cmd_cell('S', 0xB0)),
                (2, 0, cmd_cell('S', 0xB2)),
                (3, 0, cmd_cell('S', 0xE1)),
            ],
        );
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0]);
        let mut p = player(&m);
        // rows: 0,1,2,(1,2),(1,2),3,3 → 9 rows, then end.
        let mut rows_seen = Vec::new();
        for _ in 0..12 {
            p.render(&mut [0i16; 2]);
            rows_seen.push(p.row);
            render_all_frames(&mut p, 6 * 882 - 1);
            if p.ended {
                break;
            }
        }
        assert_eq!(rows_seen, vec![0, 1, 2, 1, 2, 1, 2, 3, 3]);
    }

    #[test]
    fn scx_note_cut_and_sdx_note_delay() {
        let mut cut = note_cell(60, 1);
        cut.mask |= IT_CELL_COMMAND;
        cut.command = FX_S;
        cut.param = 0xC2;
        let mut delay = note_cell(60, 1);
        delay.mask |= IT_CELL_COMMAND;
        delay.command = FX_S;
        delay.param = 0xD3;
        let pat = build_pattern(2, &[(0, 0, cut), (0, 1, delay)]);
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0]);
        let mut p = player(&m);
        p.render(&mut [0i16; 2]);
        assert!(
            p.voices.iter().any(|v| v.active && v.host == 0),
            "SC2 note plays on tick 0"
        );
        assert!(
            !p.voices.iter().any(|v| v.active && v.host == 1),
            "SD3 note is deferred"
        );
        render_all_frames(&mut p, 2 * 882);
        assert!(
            !p.voices.iter().any(|v| v.active && v.host == 0),
            "cut on tick 2"
        );
        assert!(!p.voices.iter().any(|v| v.active && v.host == 1));
        render_all_frames(&mut p, 882);
        assert!(
            p.voices.iter().any(|v| v.active && v.host == 1),
            "delayed note fires on tick 3"
        );
    }

    #[test]
    fn oxx_sample_offset_with_high_offset_and_out_of_range() {
        let (mut hdr, _) = square_sample(0);
        let body: Vec<u8> = (0..300u32).map(|i| (i & 0x7F) as u8).collect();
        hdr[0x30..0x34].copy_from_slice(&300u32.to_le_bytes());
        hdr[0x38..0x3C].copy_from_slice(&300u32.to_le_bytes());
        let mut o1 = note_cell(60, 1);
        o1.mask |= IT_CELL_COMMAND;
        o1.command = FX_O;
        o1.param = 0x01; // 256 frames
        let mut o2 = note_cell(60, 1);
        o2.mask |= IT_CELL_COMMAND;
        o2.command = FX_O;
        o2.param = 0x02; // 512 > 300 → ignored in IT mode
        let pat = build_pattern(1, &[(0, 0, o1), (0, 1, o2)]);
        let m = build_module(
            &[0],
            IT_FLAG_LINEAR_SLIDES,
            &[],
            &[(hdr.clone(), body.clone())],
            std::slice::from_ref(&pat),
        );
        let mut p = player(&m);
        p.render(&mut [0i16; 2]);
        let v0 = p.channels[0].voice.unwrap();
        let v1 = p.channels[1].voice.unwrap();
        assert!((p.voices[v0].mixer.pos - 256.0).abs() < 1.0);
        assert!(p.voices[v1].mixer.pos < 1.0, "out-of-range Oxx ignored");
        // Old effects: past the end plays from the end.
        let m = build_module(
            &[0],
            IT_FLAG_LINEAR_SLIDES | crate::it::IT_FLAG_OLD_EFFECTS,
            &[],
            &[(hdr, body)],
            &[pat],
        );
        let mut p = player(&m);
        p.render(&mut [0i16; 2]);
        let v1 = p.channels[1].voice.unwrap();
        assert!((p.voices[v1].mixer.pos - 299.0).abs() < 1.0);
    }

    #[test]
    fn global_and_channel_volume_effects() {
        let pat = build_pattern(
            2,
            &[
                (0, 0, note_cell(60, 1)),
                (0, 1, cmd_cell('V', 0x40)),
                (0, 2, cmd_cell('M', 0x20)),
                (1, 1, cmd_cell('W', 0x10)),
                (1, 2, cmd_cell('N', 0x01)),
            ],
        );
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0]);
        let mut p = player(&m);
        p.render(&mut [0i16; 2]);
        assert_eq!(p.global_volume, 64);
        assert_eq!(p.channels[2].channel_volume, 32);
        render_all_frames(&mut p, 6 * 882);
        assert_eq!(p.row, 1);
        render_all_frames(&mut p, 2 * 882);
        assert_eq!(p.global_volume, 66);
        assert_eq!(p.channels[2].channel_volume, 30);
        // Sample-mode FV = Vol*SV*CV*GV / 2^18 for channel 0 (vol 64, SV
        // 64, CV 64, GV 66) = 66 → amp 66/128.
        let v0 = p.channels[0].voice.unwrap();
        assert!((p.voices[v0].amp - 66.0 / 128.0).abs() < 1e-6);
    }

    #[test]
    fn muted_channel_still_processes_effects_but_is_silent() {
        let mut m = {
            let pat = build_pattern(1, &[(0, 0, note_cell(60, 1)), (0, 0, cmd_cell('V', 0x10))]);
            sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0])
        };
        m[0x40] |= 0x80; // channel 0 disabled
        let mut p = player(&m);
        assert!(p.channels[0].muted);
        let pcm = render_all(&mut p, 2000);
        assert!(pcm.iter().all(|&s| s == 0), "muted channel renders silence");
        assert_eq!(
            p.global_volume, 16,
            "effects in muted channels are still processed"
        );
    }

    #[test]
    fn xxx_pan_and_pxy_pan_slide() {
        let pat = build_pattern(
            2,
            &[
                (0, 0, note_cell(60, 1)),
                (0, 0, cmd_cell('X', 0x80)),
                (1, 0, cmd_cell('P', 0x20)),
            ],
        );
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0]);
        let mut p = player(&m);
        p.render(&mut [0i16; 2]);
        assert_eq!(p.channels[0].pan, 32);
        render_all_frames(&mut p, 6 * 882 + 2 * 882);
        assert_eq!(p.channels[0].pan, 28, "P20 slides LEFT 2 per tick");
    }

    #[test]
    fn instrument_mode_envelope_and_fadeout_end_the_voice() {
        use crate::it::{IT_ENV_ON, IT_INSTRUMENT_SIZE};
        // Volume envelope 64 → 0 over 12 ticks, no loop → then fade at
        // 128/tick (8 ticks to zero).
        let nodes: &[(i8, u16)] = &[(64, 0), (0, 12)];
        let ins = build_new_instrument(
            "env",
            (0, 0, 0),
            128,
            &[],
            [
                (IT_ENV_ON, 0, 0, 0, 0, nodes),
                (0, 0, 0, 0, 0, &[]),
                (0, 0, 0, 0, 0, &[]),
            ],
        );
        assert_eq!(ins.len(), IT_INSTRUMENT_SIZE);
        let pat = build_pattern(8, &[(0, 0, note_cell(60, 1))]);
        let m = build_module(
            &[0],
            IT_FLAG_LINEAR_SLIDES | IT_FLAG_INSTRUMENTS,
            &[ins],
            &[square_sample(0)],
            &[pat],
        );
        let mut p = player(&m);
        assert!(p.instrument_mode);
        p.render(&mut [0i16; 2]);
        let vi = p.channels[0].voice.unwrap();
        assert!(
            (p.voices[vi].amp - 1.0).abs() < 1e-6,
            "tick 0: VEV 64 → FV 128"
        );
        render_all_frames(&mut p, 6 * 882);
        // Tick 6: envelope at 32 → FV 64.
        assert!(
            (p.voices[vi].amp - 0.5).abs() < 1e-6,
            "amp {}",
            p.voices[vi].amp
        );
        render_all_frames(&mut p, 6 * 882);
        assert!(p.voices[vi].fading, "envelope end turns on note fade");
        render_all_frames(&mut p, 9 * 882);
        assert!(!p.voices[vi].active, "faded to zero → voice done");
    }

    #[test]
    fn nna_continue_keeps_the_old_voice_in_the_background() {
        use crate::it::IT_ENV_ON;
        let nodes: &[(i8, u16)] = &[(64, 0), (64, 100)];
        let ins = build_new_instrument(
            "cont",
            (1, 0, 0), // NNA continue
            0,
            &[],
            [
                (IT_ENV_ON | crate::it::IT_ENV_LOOP, 0, 1, 0, 0, nodes),
                (0, 0, 0, 0, 0, &[]),
                (0, 0, 0, 0, 0, &[]),
            ],
        );
        let pat = build_pattern(
            4,
            &[
                (0, 0, note_cell(60, 1)),
                (1, 0, note_cell(64, 1)),
                (2, 0, note_cell(67, 1)),
            ],
        );
        let m = build_module(
            &[0],
            IT_FLAG_LINEAR_SLIDES | IT_FLAG_INSTRUMENTS,
            &[ins],
            &[square_sample(0)],
            &[pat],
        );
        let mut p = player(&m);
        render_all_frames(&mut p, 2 * 6 * 882 + 1);
        assert_eq!(p.active_voices(), 3, "two background + one foreground");
        assert_eq!(
            p.voices.iter().filter(|v| v.active && v.background).count(),
            2
        );
        let fg = p.channels[0].voice.unwrap();
        assert_eq!(p.voices[fg].note, 67);
    }

    #[test]
    fn nna_cut_and_dct_note_cut_duplicates() {
        // NNA cut: only one voice ever. DCT note + DCA cut: a repeated
        // note with NNA continue is cut anyway.
        let ins_cut = build_new_instrument("cut", (0, 0, 0), 0, &[], [(0, 0, 0, 0, 0, &[]); 3]);
        let pat = build_pattern(3, &[(0, 0, note_cell(60, 1)), (1, 0, note_cell(64, 1))]);
        let m = build_module(
            &[0],
            IT_FLAG_LINEAR_SLIDES | IT_FLAG_INSTRUMENTS,
            &[ins_cut],
            &[square_sample(0)],
            std::slice::from_ref(&pat),
        );
        let mut p = player(&m);
        render_all_frames(&mut p, 6 * 882 + 1);
        assert_eq!(p.active_voices(), 1);

        let ins_dct = build_new_instrument("dct", (1, 1, 0), 0, &[], [(0, 0, 0, 0, 0, &[]); 3]);
        let pat2 = build_pattern(
            3,
            &[
                (0, 0, note_cell(60, 1)),
                (1, 0, note_cell(60, 1)),
                (2, 0, note_cell(62, 1)),
            ],
        );
        let m = build_module(
            &[0],
            IT_FLAG_LINEAR_SLIDES | IT_FLAG_INSTRUMENTS,
            &[ins_dct],
            &[square_sample(0)],
            &[pat2],
        );
        let mut p = player(&m);
        render_all_frames(&mut p, 6 * 882 + 1);
        assert_eq!(p.active_voices(), 1, "duplicate note cut by DCT/DCA");
        render_all_frames(&mut p, 6 * 882);
        assert_eq!(
            p.active_voices(),
            2,
            "a different note continues in the background"
        );
    }

    #[test]
    fn note_off_in_instrument_mode_fades_without_an_envelope() {
        let ins = build_new_instrument("nf", (0, 0, 0), 64, &[], [(0, 0, 0, 0, 0, &[]); 3]);
        let pat = build_pattern(
            8,
            &[
                (0, 0, note_cell(60, 1)),
                (
                    1,
                    0,
                    ItCell {
                        mask: IT_CELL_NOTE,
                        note: crate::it::IT_NOTE_OFF,
                        ..ItCell::default()
                    },
                ),
            ],
        );
        let m = build_module(
            &[0],
            IT_FLAG_LINEAR_SLIDES | IT_FLAG_INSTRUMENTS,
            &[ins],
            &[square_sample(0)],
            &[pat],
        );
        let mut p = player(&m);
        render_all_frames(&mut p, 6 * 882 + 1);
        let vi = p.channels[0].voice.unwrap();
        assert!(p.voices[vi].released && p.voices[vi].fading);
        assert!(p.voices[vi].fade < IT_FADEOUT_COUNT);
        render_all_frames(&mut p, 17 * 882);
        assert!(!p.voices[vi].active, "1024 / 64 = 16 ticks to silence");
    }

    #[test]
    fn instrument_pan_and_pitch_pan_separation() {
        let mut ins = build_new_instrument("pan", (0, 0, 0), 0, &[], [(0, 0, 0, 0, 0, &[]); 3]);
        ins[0x19] = 16; // DfP 16, use
        ins[0x16] = 8; // PPS
        ins[0x17] = 60; // PPC = C-5
        let pat = build_pattern(1, &[(0, 0, note_cell(60, 1)), (0, 1, note_cell(72, 1))]);
        let m = build_module(
            &[0],
            IT_FLAG_LINEAR_SLIDES | IT_FLAG_INSTRUMENTS,
            &[ins],
            &[square_sample(0)],
            &[pat],
        );
        let mut p = player(&m);
        p.render(&mut [0i16; 2]);
        assert_eq!(p.channels[0].pan, 16);
        assert_eq!(p.channels[1].pan, 16 + 12 * 8 / 8, "+12 notes × PPS 8 / 8");
    }

    #[test]
    fn vibrato_moves_pitch_every_tick_in_it_mode() {
        let mut c = note_cell(60, 1);
        c.mask |= IT_CELL_COMMAND;
        c.command = FX_H;
        c.param = 0x4F;
        let pat = build_pattern(
            4,
            &[
                (0, 0, c),
                (1, 0, cmd_cell('H', 0)),
                (2, 0, cmd_cell('H', 0)),
            ],
        );
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0]);
        let mut p = player(&m);
        let mut freqs = Vec::new();
        for _ in 0..18 {
            p.render(&mut [0i16; 2]);
            let vi = p.channels[0].voice.unwrap();
            freqs.push(p.voices[vi].play_freq);
            render_all_frames(&mut p, 881);
        }
        assert!(
            freqs.windows(2).any(|w| (w[0] - w[1]).abs() > 1.0),
            "pitch moves"
        );
        assert!(freqs.iter().any(|&f| f > 8363.5) && freqs.iter().any(|&f| f < 8362.5));
        // Old effects: tick 0 is untouched.
        let m = sample_mode_module(
            IT_FLAG_LINEAR_SLIDES | crate::it::IT_FLAG_OLD_EFFECTS,
            &[build_pattern(2, &[(0, 0, c)])],
            &[0],
        );
        let mut p = player(&m);
        p.render(&mut [0i16; 2]);
        let vi = p.channels[0].voice.unwrap();
        assert!((p.voices[vi].play_freq - 8363.0).abs() < 1e-6);
        assert_eq!(
            p.channels[0].mem_h_depth,
            15 * 4 * 2,
            "old effects doubles the depth"
        );
    }
    #[test]
    fn compat_gxx_retunes_on_sample_change_and_links_e_f_g_memory() {
        // §"Impulse Header Layout" Flags bit 5: "Link Effect G's memory
        // with Effect E/F. Also Gxx with an instrument present will
        // cause the envelopes to be retriggered. If you change a sample
        // on a row with Gxx, it'll adjust the frequency of the current
        // note according to: NewFrequency = OldFrequency * NewC5 / OldC5".
        use crate::it::{IT_ENV_ON, IT_FLAG_COMPAT_GXX};
        let nodes: &[(i8, u16)] = &[(64, 0), (0, 40)];
        let ins_a = build_new_instrument(
            "a",
            (0, 0, 0),
            0,
            &(0..120).map(|n| (n, 1)).collect::<Vec<_>>(),
            [
                (IT_ENV_ON, 0, 0, 0, 0, nodes),
                (0, 0, 0, 0, 0, &[]),
                (0, 0, 0, 0, 0, &[]),
            ],
        );
        let ins_b = build_new_instrument(
            "b",
            (0, 0, 0),
            0,
            &(0..120).map(|n| (n, 2)).collect::<Vec<_>>(),
            [
                (IT_ENV_ON, 0, 0, 0, 0, nodes),
                (0, 0, 0, 0, 0, &[]),
                (0, 0, 0, 0, 0, &[]),
            ],
        );
        let (mut hi_hdr, hi_body) = square_sample(0);
        hi_hdr[0x3C..0x40].copy_from_slice(&16726u32.to_le_bytes());
        let mut g = note_cell(60, 2);
        g.mask |= IT_CELL_COMMAND;
        g.command = FX_G;
        g.param = 0x00;
        let pat = build_pattern(
            4,
            &[
                (0, 0, note_cell(60, 1)),
                (0, 0, cmd_cell('F', 0x03)),
                (1, 0, g),
                (2, 0, cmd_cell('E', 0x00)),
            ],
        );
        let m = build_module(
            &[0],
            IT_FLAG_LINEAR_SLIDES | IT_FLAG_INSTRUMENTS | IT_FLAG_COMPAT_GXX,
            &[ins_a, ins_b],
            &[square_sample(0), (hi_hdr, hi_body)],
            &[pat],
        );
        let mut p = player(&m);
        // Row 0: F03 (12 units/tick × 5 ticks).
        render_all_frames(&mut p, 6 * 882);
        let after_row0 = linear_slide(8363.0, 60.0);
        assert!((p.channels[0].freq - after_row0).abs() < 1e-3);
        // Row 1 tick 0: G00 with instrument 2 (C5 16726): retune ×2, the
        // envelope restarts, and G00 inherits F's memory (03).
        p.render(&mut [0i16; 2]);
        let vi = p.channels[0].voice.unwrap();
        assert_eq!(p.voices[vi].vol_env.tick, 1, "envelope retriggered");
        assert!(
            (p.channels[0].freq - after_row0 * 2.0).abs() < 1e-3,
            "NewC5/OldC5 retune"
        );
        assert_eq!(p.channels[0].c5_speed, 16726);
        assert_eq!(p.channels[0].mem_g, 0x03, "G shares E/F memory");
        assert!((p.channels[0].porta_target - 16726.0).abs() < 1e-6);
        // Row 2: E00 reuses the linked memory (03) → slides down 12/tick.
        render_all_frames(&mut p, 6 * 882);
        let at_row2 = p.channels[0].freq;
        render_all_frames(&mut p, 882);
        assert!((p.channels[0].freq - linear_slide(at_row2, -12.0)).abs() < 1e-3);
    }

    #[test]
    fn old_effects_vibrato_is_period_domain_and_holds_over_row_ticks() {
        let mut c = note_cell(60, 1);
        c.mask |= IT_CELL_COMMAND;
        c.command = FX_H;
        c.param = 0x28;
        let pat = build_pattern(
            4,
            &[
                (0, 0, c),
                (1, 0, cmd_cell('H', 0)),
                (2, 0, cmd_cell('H', 0)),
            ],
        );
        let m = sample_mode_module(
            IT_FLAG_LINEAR_SLIDES | crate::it::IT_FLAG_OLD_EFFECTS,
            &[pat],
            &[0],
        );
        let mut p = player(&m);
        let mut freqs = Vec::new();
        for _ in 0..13 {
            p.render(&mut [0i16; 2]);
            let vi = p.channels[0].voice.unwrap();
            freqs.push(p.voices[vi].play_freq);
            render_all_frames(&mut p, 881);
        }
        assert!(
            (freqs[0] - 8363.0).abs() < 1e-6,
            "no update on the note's row tick"
        );
        assert!(
            freqs[1] < 8363.0,
            "old effects: a positive table value lowers the pitch"
        );
        assert!(
            (freqs[6] - freqs[5]).abs() < 1e-6,
            "the row tick keeps the previous delta ({} vs {})",
            freqs[5],
            freqs[6]
        );
        assert!(
            freqs[7] < freqs[6],
            "and the slide resumes on the next non-row tick"
        );
    }

    #[test]
    fn panbrello_cycle_is_256_over_speed_ticks() {
        // Y4F: speed 4 → one cycle every 64 ticks; peak (+30 at pos 64)
        // on tick 16; depth 15 × 64 / 32 = 30.
        let mut c = note_cell(60, 1);
        c.mask |= IT_CELL_COMMAND | IT_CELL_VOLPAN;
        c.volpan = 128 + 32;
        c.command = FX_Y;
        c.param = 0x4F;
        let cells: Vec<(u16, u8, ItCell)> = std::iter::once((0u16, 0u8, c))
            .chain((1..12u16).map(|r| (r, 0u8, cmd_cell('Y', 0))))
            .collect();
        let pat = build_pattern(12, &cells);
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0]);
        let mut p = player(&m);
        let mut pans = Vec::new();
        for _ in 0..33 {
            p.render(&mut [0i16; 2]);
            let vi = p.channels[0].voice.unwrap();
            pans.push(p.voices[vi].final_pan);
            render_all_frames(&mut p, 881);
        }
        assert_eq!(pans[0], 32, "tick 0 reads position 0");
        assert_eq!(pans[16], 62, "peak +30 at tick 16");
        assert_eq!(pans[32], 32, "back to centre at tick 32");
        assert!(pans[8] > 32 && pans[8] < 62);
    }

    #[test]
    fn s00_reuses_the_last_sxx_and_s6x_extends_the_row() {
        let mut cut = note_cell(60, 1);
        cut.mask |= IT_CELL_COMMAND;
        cut.command = FX_S;
        cut.param = 0xC1;
        let mut again = note_cell(60, 1);
        again.mask |= IT_CELL_COMMAND;
        again.command = FX_S;
        again.param = 0x00;
        let pat = build_pattern(
            4,
            &[
                (0, 0, cut),
                (1, 0, again),
                (2, 0, cmd_cell('S', 0x62)),
                (3, 0, note_cell(60, 1)),
            ],
        );
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0]);
        let mut p = player(&m);
        render_all_frames(&mut p, 6 * 882 + 2 * 882);
        assert_eq!(p.row, 1);
        assert_eq!(p.active_voices(), 0, "S00 repeated the SC1 cut");
        // Row 2 has S62: the row lasts 6 + 2 ticks.
        render_all_frames(&mut p, 4 * 882);
        assert_eq!(p.row, 2);
        render_all_frames(&mut p, 7 * 882);
        assert_eq!(p.row, 2, "still row 2 after 7 ticks");
        render_all_frames(&mut p, 882);
        assert_eq!(p.row, 3);
        assert_eq!(p.tick_delay, 0, "the extension is consumed");
    }

    #[test]
    fn tempo_slides_t0x_t1x_move_the_tempo_per_tick() {
        let pat = build_pattern(
            3,
            &[
                (0, 0, note_cell(60, 1)),
                (0, 0, cmd_cell('T', 0x02)),
                (1, 0, cmd_cell('T', 0x13)),
                (2, 0, cmd_cell('T', 0x00)),
            ],
        );
        let m = sample_mode_module(IT_FLAG_LINEAR_SLIDES, &[pat], &[0]);
        let mut p = player(&m);
        // Each tick's length follows the tempo set at its start: enter
        // the tick with one frame, then finish it.
        let tick = |p: &mut ItPlayerState| {
            p.render(&mut [0i16; 2]);
            let spt = p.samples_per_tick() as usize;
            render_all_frames(p, spt - 1);
        };
        tick(&mut p);
        assert_eq!(p.tempo, 125, "T0x does nothing on the row tick");
        for _ in 0..5 {
            tick(&mut p);
        }
        assert_eq!(p.tempo, 115);
        tick(&mut p);
        assert_eq!(p.row, 1);
        for _ in 0..5 {
            tick(&mut p);
        }
        assert_eq!(p.tempo, 130, "T13: +3 per tick");
        for _ in 0..6 {
            tick(&mut p);
        }
        assert_eq!(p.tempo, 145, "T00 reuses the last parameter");
    }

    #[test]
    fn note_fade_cell_and_s7x_past_note_controls() {
        use crate::it::IT_ENV_ON;
        let nodes: &[(i8, u16)] = &[(64, 0), (64, 100)];
        let ins = build_new_instrument(
            "cont",
            (1, 0, 0),
            64,
            &[],
            [
                (IT_ENV_ON | crate::it::IT_ENV_LOOP, 0, 1, 0, 0, nodes),
                (0, 0, 0, 0, 0, &[]),
                (0, 0, 0, 0, 0, &[]),
            ],
        );
        let fade_cell = ItCell {
            mask: IT_CELL_NOTE,
            note: 200,
            ..ItCell::default()
        };
        let pat = build_pattern(
            8,
            &[
                (0, 0, note_cell(60, 1)),
                (1, 0, fade_cell),
                (0, 1, note_cell(60, 1)),
                (1, 1, note_cell(64, 1)),
                (2, 1, cmd_cell('S', 0x70)),
                (0, 2, note_cell(60, 1)),
                (1, 2, note_cell(64, 1)),
                (2, 2, cmd_cell('S', 0x72)),
                (0, 3, note_cell(60, 1)),
                (1, 3, cmd_cell('S', 0x73)),
                (2, 3, note_cell(64, 1)),
            ],
        );
        let m = build_module(
            &[0],
            IT_FLAG_LINEAR_SLIDES | IT_FLAG_INSTRUMENTS,
            &[ins],
            &[square_sample(0)],
            &[pat],
        );
        let mut p = player(&m);
        render_all_frames(&mut p, 2 * 6 * 882 + 1);
        // ch0: "Others = note fade" — the voice fades without release.
        let v0 = p.channels[0].voice.unwrap();
        assert!(p.voices[v0].fading && !p.voices[v0].released);
        // ch1: S70 cut the background voice; only the foreground is left.
        let bg1 = p
            .voices
            .iter()
            .filter(|v| v.active && v.host == 1 && v.background)
            .count();
        assert_eq!(bg1, 0, "S70 = past note cut");
        // ch2: S72 fades the background voice.
        let bg2: Vec<&ItVoice> = p
            .voices
            .iter()
            .filter(|v| v.active && v.host == 2 && v.background)
            .collect();
        assert_eq!(bg2.len(), 1);
        assert!(bg2[0].fading, "S72 = past note fade");
        // ch3: S73 set the current note's NNA to cut, so the row-2 note
        // did not leave a background voice.
        let bg3 = p
            .voices
            .iter()
            .filter(|v| v.active && v.host == 3 && v.background)
            .count();
        assert_eq!(bg3, 0, "S73 = NNA cut for the next note");
    }

    #[test]
    fn instrument_column_alone_reloads_volume_and_pan_envelope_scales() {
        use crate::it::IT_ENV_ON;
        let pan_nodes: &[(i8, u16)] = &[(32, 0), (32, 100)];
        let ins = build_new_instrument(
            "pe",
            (0, 0, 0),
            0,
            &[],
            [
                (0, 0, 0, 0, 0, &[]),
                (IT_ENV_ON, 0, 0, 0, 0, pan_nodes),
                (0, 0, 0, 0, 0, &[]),
            ],
        );
        let mut vol = note_cell(60, 1);
        vol.mask |= IT_CELL_VOLPAN;
        vol.volpan = 10;
        let inst_only = ItCell {
            mask: crate::it::IT_CELL_INSTRUMENT,
            instrument: 1,
            ..ItCell::default()
        };
        let mut left = note_cell(60, 1);
        left.mask |= IT_CELL_COMMAND;
        left.command = FX_X;
        left.param = 0x00;
        let pat = build_pattern(
            4,
            &[
                (0, 0, vol),
                (1, 0, inst_only),
                (0, 1, note_cell(60, 1)),
                (0, 2, left),
            ],
        );
        let m = build_module(
            &[0],
            IT_FLAG_LINEAR_SLIDES | IT_FLAG_INSTRUMENTS,
            &[ins],
            &[square_sample(0)],
            &[pat],
        );
        let mut p = player(&m);
        p.render(&mut [0i16; 2]);
        assert_eq!(p.channels[0].volume, 10);
        let v1 = p.channels[1].voice.unwrap();
        assert_eq!(
            p.voices[v1].final_pan, 64,
            "centre pan + full envelope = hard right"
        );
        let v2 = p.channels[2].voice.unwrap();
        assert_eq!(
            p.voices[v2].final_pan, 0,
            "hard-left pan leaves no room: stays left"
        );
        render_all_frames(&mut p, 6 * 882);
        assert_eq!(
            p.channels[0].volume, 64,
            "instrument alone reloads the default volume"
        );
    }
}
