//! ProTracker playback engine.
//!
//! Drives a `PlayerState` forward one tick at a time. Two output modes
//! share the same mixing core:
//!
//! - [`PlayerState::render`] writes interleaved stereo S16 PCM, applying
//!   the Amiga pan convention (channels 0 & 3 lean LEFT, 1 & 2 lean
//!   RIGHT; repeats every 4 for >4-channel files) and a 1/(N/2)
//!   headroom scale. Pan separation defaults to 70 % rather than
//!   strict 100 % hard pan — see [`PlayerState::set_pan_separation`]
//!   and the citation in the field doc-comment for the rationale
//!   (`Protracker-effects-MODFIL12.txt` §11).
//! - [`PlayerState::render_per_channel`] writes one S16 plane per MOD
//!   tracker channel, post-volume but pre-pan and pre-mix. Downstream
//!   consumers that need to mix / pan / analyse channels independently
//!   (DAWs, visualisers, per-instrument remastering) drive the player
//!   via this path.
//!
//! Terminology:
//! - **Row**: a line in a pattern. A pattern has 64 rows.
//! - **Tick**: one row is `speed` ticks long (default 6).
//! - **BPM**: governs wall-clock tick duration. Samples-per-tick =
//!   `sample_rate * 2.5 / BPM` — 882 at 44.1 kHz / 125 BPM.
//! - **Period**: the Amiga Paula divider. Output frequency =
//!   PAULA_CLOCK / period.
//!
//! Effect coverage follows `docs/audio/trackers/mod/Protracker-v1.1B-mod.txt`
//! and the concrete tick-level semantics described in
//! `docs/audio/trackers/mod/FireLight-MOD-Player-Tutorial.txt` §5. All
//! 16 base effect slots (0..F) plus the 16 Exy sub-commands are wired —
//! see [`apply_tick0_effect`] / [`apply_tickn_effect`] for the dispatch
//! tables and the module doc-comment's coverage matrix.

use crate::header::{ModHeader, PATTERN_ROWS};
use crate::samples::SampleBody;

/// Paula clock (PAL) — classic MOD period→frequency constant. Divide by
/// the period to get the Amiga's output sample rate for that channel.
pub const PAULA_CLOCK: f32 = 7_093_789.2 / 2.0;

pub const DEFAULT_SPEED: u8 = 6;
pub const DEFAULT_BPM: u8 = 125;
pub const CHANNELS_PER_MOD: usize = 4;

/// Protracker porta-up floor: B-3 at finetune 0 is 113. Effects `1xx`
/// (porta up) and `E1x` (fine porta up) must not slide below this value
/// per `Protracker-v1.1B-mod.txt` ("You can NOT slide higher than B-3!
/// (Period 113)") and `Protracker-effects-MODFIL12.txt` §1
/// ("usually cannot slide past note B-3 unless you have implemented
/// octave 4 (NON-STANDARD!)").
pub const PERIOD_MIN: u16 = 113;
/// Protracker porta-down ceiling: C-1 at finetune 0 is 856. Effects
/// `2xx` (porta down) and `E2x` (fine porta down) clamp at this value
/// per `Protracker-v1.1B-mod.txt` ("You can NOT slide lower than C-1!
/// (Period 856)").
pub const PERIOD_MAX: u16 = 856;
/// Extended period floor — finetune +7 B-3 = 108. Per
/// `Protracker-effects-MODFIL12.txt` §3.2 the "Normal Minimum Period =
/// 108", and the period table includes 108 as the lowest legitimate
/// value (finetune +7 row 35). The mixer must accept this without
/// further clamping so finetune extremes play cleanly.
pub const PERIOD_MIN_EXT: u16 = 108;
/// Extended period ceiling — finetune -8 C-1 = 907. Per
/// `Protracker-effects-MODFIL12.txt` §3.2 the "Normal Maximum Period =
/// 907", and the period table's finetune -8 row begins with 907.
pub const PERIOD_MAX_EXT: u16 = 907;

/// 32-entry Protracker vibrato / tremolo sine table (half-wave positive
/// quadrant; the sign is applied based on the LFO position register per
/// spec). From FireLight-MOD-Player-Tutorial.txt §5.5.
#[rustfmt::skip]
pub const PROTRACKER_SINE_TABLE: [u8; 32] = [
      0,  24,  49,  74,  97, 120, 141, 161,
    180, 197, 212, 224, 235, 244, 250, 253,
    255, 253, 250, 244, 235, 224, 212, 197,
    180, 161, 141, 120,  97,  74,  49,  24,
];

/// 16-finetune × 36-note Protracker period table. Indexed as
/// `PERIOD_TABLE[finetune_index][note_index]` where
/// `finetune_index = finetune & 0xF` (0..=7 = +0..+7, 8..=15 = -8..-1)
/// and `note_index = 0..=35` (C-1 at 0, B-3 at 35).
///
/// Transcribed verbatim from FireLight-MOD-Player-Tutorial.txt §3.5
/// (identified there as a straight dump of the Protracker replayer's
/// `mt_PeriodTable`, which cross-references the spec's own
/// "Periodtable for Tuning 0, Normal" block in
/// Protracker-v1.1B-mod.txt.)
#[rustfmt::skip]
pub const PERIOD_TABLE: [[u16; 36]; 16] = [
    // Tuning  0
    [856,808,762,720,678,640,604,570,538,508,480,453,
     428,404,381,360,339,320,302,285,269,254,240,226,
     214,202,190,180,170,160,151,143,135,127,120,113],
    // Tuning +1
    [850,802,757,715,674,637,601,567,535,505,477,450,
     425,401,379,357,337,318,300,284,268,253,239,225,
     213,201,189,179,169,159,150,142,134,126,119,113],
    // Tuning +2
    [844,796,752,709,670,632,597,563,532,502,474,447,
     422,398,376,355,335,316,298,282,266,251,237,224,
     211,199,188,177,167,158,149,141,133,125,118,112],
    // Tuning +3
    [838,791,746,704,665,628,592,559,528,498,470,444,
     419,395,373,352,332,314,296,280,264,249,235,222,
     209,198,187,176,166,157,148,140,132,125,118,111],
    // Tuning +4
    [832,785,741,699,660,623,588,555,524,495,467,441,
     416,392,370,350,330,312,294,278,262,247,233,220,
     208,196,185,175,165,156,147,139,131,124,117,110],
    // Tuning +5
    [826,779,736,694,655,619,584,551,520,491,463,437,
     413,390,368,347,328,309,292,276,260,245,232,219,
     206,195,184,174,164,155,146,138,130,123,116,109],
    // Tuning +6
    [820,774,730,689,651,614,580,547,516,487,460,434,
     410,387,365,345,325,307,290,274,258,244,230,217,
     205,193,183,172,163,154,145,137,129,122,115,109],
    // Tuning +7
    [814,768,725,684,646,610,575,543,513,484,457,431,
     407,384,363,342,323,305,288,272,256,242,228,216,
     204,192,181,171,161,152,144,136,128,121,114,108],
    // Tuning -8 (wrapped as index 8)
    [907,856,808,762,720,678,640,604,570,538,508,480,
     453,428,404,381,360,339,320,302,285,269,254,240,
     226,214,202,190,180,170,160,151,143,135,127,120],
    // Tuning -7
    [900,850,802,757,715,675,636,601,567,535,505,477,
     450,425,401,379,357,337,318,300,284,268,253,238,
     225,212,200,189,179,169,159,150,142,134,126,119],
    // Tuning -6
    [894,844,796,752,709,670,632,597,563,532,502,474,
     447,422,398,376,355,335,316,298,282,266,251,237,
     223,211,199,188,177,167,158,149,141,133,125,118],
    // Tuning -5
    [887,838,791,746,704,665,628,592,559,528,498,470,
     444,419,395,373,352,332,314,296,280,264,249,235,
     222,209,198,187,176,166,157,148,140,132,125,118],
    // Tuning -4
    [881,832,785,741,699,660,623,588,555,524,494,467,
     441,416,392,370,350,330,312,294,278,262,247,233,
     220,208,196,185,175,165,156,147,139,131,123,117],
    // Tuning -3
    [875,826,779,736,694,655,619,584,551,520,491,463,
     437,413,390,368,347,328,309,292,276,260,245,232,
     219,206,195,184,174,164,155,146,138,130,123,116],
    // Tuning -2
    [868,820,774,730,689,651,614,580,547,516,487,460,
     434,410,387,365,345,325,307,290,274,258,244,230,
     217,205,193,183,172,163,154,145,137,129,122,115],
    // Tuning -1
    [862,814,768,725,684,646,610,575,543,513,484,457,
     431,407,384,363,342,323,305,288,272,256,242,228,
     216,203,192,181,171,161,152,144,136,128,121,114],
];

/// Convert a signed 4-bit finetune (-8..=7) into the row index of
/// [`PERIOD_TABLE`]. Positive finetunes map to 0..=7 unchanged; negative
/// finetunes land in 8..=15 (so -8 is row 8, -1 is row 15), matching the
/// nibble encoding the spec stores in sample header byte 44.
#[inline]
pub fn finetune_row(finetune: i8) -> usize {
    (finetune as u8 & 0x0F) as usize
}

/// Find a note index (0..=35, C-1..B-3) for the given period by scanning
/// all 16 finetune rows. Returns `None` if no row has this exact period —
/// used only by `E3x` glissando.
pub fn note_index_for_period(period: u16) -> Option<usize> {
    for row in PERIOD_TABLE.iter() {
        for (note_idx, &p) in row.iter().enumerate() {
            if p == period {
                return Some(note_idx);
            }
        }
    }
    None
}

/// A single decoded pattern row entry for one channel.
#[derive(Clone, Copy, Debug, Default)]
pub struct Note {
    /// Period value (0 means "no new note").
    pub period: u16,
    /// Sample index 1..=31 (0 means "no sample change").
    pub sample: u8,
    /// Effect command nibble (0..=0xF).
    pub effect: u8,
    /// Effect parameter byte.
    pub effect_param: u8,
}

impl Note {
    fn decode(raw: [u8; 4]) -> Self {
        // Byte 0: ssss pppp  (high nibble of sample, high nibble of period)
        // Byte 1: pppp pppp  (low 8 bits of period)
        // Byte 2: ssss eeee  (low nibble of sample, effect nibble)
        // Byte 3: xxxx xxxx  (effect parameter)
        let period = (((raw[0] & 0x0F) as u16) << 8) | raw[1] as u16;
        let sample = (raw[0] & 0xF0) | (raw[2] >> 4);
        let effect = raw[2] & 0x0F;
        let effect_param = raw[3];
        Note {
            period,
            sample,
            effect,
            effect_param,
        }
    }
}

/// A decoded pattern: 64 rows × N channels.
#[derive(Clone, Debug)]
pub struct Pattern {
    pub rows: Vec<Vec<Note>>, // rows[row][channel]
}

/// Parse all patterns from a MOD bytestream.
pub fn parse_patterns(header: &ModHeader, bytes: &[u8]) -> Vec<Pattern> {
    let channels = header.channels as usize;
    let mut patterns = Vec::with_capacity(header.n_patterns as usize);
    let base = header.pattern_data_offset();

    for p in 0..header.n_patterns as usize {
        let mut rows = Vec::with_capacity(PATTERN_ROWS);
        for r in 0..PATTERN_ROWS {
            let mut row = Vec::with_capacity(channels);
            for c in 0..channels {
                let off = base + (p * PATTERN_ROWS + r) * channels * 4 + c * 4;
                let raw = if off + 4 <= bytes.len() {
                    [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]
                } else {
                    [0; 4]
                };
                row.push(Note::decode(raw));
            }
            rows.push(row);
        }
        patterns.push(Pattern { rows });
    }
    patterns
}

/// Vibrato / tremolo waveform selector for E4x / E7x.
///
/// Per Protracker-v1.1B-mod.txt:
/// - low bits 0..=2 pick the shape (0 sine, 1 ramp-down, 2 square, 3 random —
///   random is ignored in PT).
/// - bit 2 (value 4/5/6/7) disables retrigger on new notes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Waveform {
    /// 0 = sine, 1 = ramp-down, 2 = square, 3 = random (treated as sine).
    pub shape: u8,
    /// If false, position is reset to 0 on every new note (default).
    pub no_retrigger: bool,
}

impl Waveform {
    fn set(&mut self, nibble: u8) {
        self.shape = nibble & 0x3;
        self.no_retrigger = nibble & 0x4 != 0;
    }
}

/// Per-channel playback state.
#[derive(Clone, Debug, Default)]
pub struct Channel {
    /// 1-based sample index (0 = no sample ever triggered).
    pub sample_index: u8,
    /// Fractional read position into the sample's pcm buffer.
    pub sample_pos: f32,
    /// Current period (0 = silent / not playing).
    pub period: u16,
    /// Current volume 0..=64.
    pub volume: u8,
    /// Whether this channel is currently sounding.
    pub active: bool,
    /// Current finetune for the most recently loaded sample.
    pub finetune: i8,

    /// Current effect command (0..=0xF).
    pub effect: u8,
    pub effect_param: u8,

    // ---- effect memory ----
    /// Arpeggio base period — the un-modulated period for this row.
    pub arp_base_period: u16,
    /// Last portamento-up param (1xx), used when param == 0.
    pub mem_porta_up: u8,
    /// Last portamento-down param (2xx), used when param == 0.
    pub mem_porta_down: u8,
    /// Tone portamento target period (set by a note on a 3xy / 5xy row).
    pub tone_porta_target: u16,
    /// Tone portamento speed (3xy / 5xy param; 0 reuses the last value).
    pub tone_porta_speed: u8,
    /// Last vibrato params — nibble format `rate<<4 | depth`.
    pub mem_vibrato: u8,
    /// Vibrato LFO position, signed -32..=31.
    pub vib_pos: i8,
    /// Vibrato waveform control.
    pub vib_wave: Waveform,
    /// Last tremolo params — nibble format `rate<<4 | depth`.
    pub mem_tremolo: u8,
    /// Tremolo LFO position, signed -32..=31.
    pub trem_pos: i8,
    /// Tremolo waveform control.
    pub trem_wave: Waveform,
    /// Last 9xx sample-offset param.
    pub mem_sample_offset: u8,
    /// Last volume-slide param (A/5/6).
    pub mem_volslide: u8,
    /// E9x retrigger period (ticks between retriggers).
    pub retrig_ticks: u8,
    /// ECx note-cut tick (0 = no cut pending).
    pub cut_tick: u8,
    /// EDx note-delay state — `Some(delay_tick)` while pending, filled in on
    /// row entry.
    pub delay: Option<DelayedTrigger>,
    /// Glissando flag (E3x): if set, tone portamento snaps to nearest
    /// semitone each tick rather than sliding smoothly.
    pub glissando: bool,

    /// Sample number written on a row that did not also trigger a note —
    /// the swap is deferred until the next note-on per Protracker quirk
    /// (see `enter_row` for the citation). 0 means no pending swap.
    pub pending_sample: u8,

    /// Pending LED-filter state from an `E0x` on this channel's row.
    /// `Some(true)` = filter ON (E00, LED on). `Some(false)` = filter
    /// OFF (E01, LED off). `None` = no E0 this row. Resolved at the
    /// end of `enter_row` after all four channels' tick-0 effects are
    /// processed: like Fxx, a later channel's E0 wins on the same row.
    pub pending_led: Option<bool>,

    /// Per-channel pan position, 0..=255. `0` = hard LEFT, `255` =
    /// hard RIGHT, `128` = centre. This is the FT-extension panning
    /// state set by `8xx` (full 8-bit pan, `Protracker-effects-
    /// MODFIL12.txt` lines 1201-1207: "Command 8: Set FINE Panning ...
    /// xxxxyyyy = panning position. (0=Most left, 255=most right.)")
    /// and by `E8x` (rough nibble pan, `Protracker-effects-
    /// MODFIL12.txt` lines 1503-1505: "Command $E8: Set (Rough)
    /// Panning ... yyyy = panning value. $0 = most left, $F = most
    /// right.") — the E8 nibble is replicated across both halves of
    /// the byte so `E80` → 0x00, `E8F` → 0xFF, `E87` → 0x77, matching
    /// the "rough" 16-step interpretation also documented in
    /// `multimedia-cx-protracker.html` ("$0 is hard left, $F is hard
    /// right"). Initial values follow the Amiga LRRL hard-pan
    /// convention (channels 0 & 3 → 0, 1 & 2 → 255, pattern repeats
    /// every 4) so a MOD with no panning commands renders identically
    /// to the pre-r75 build.
    pub pan: u8,

    /// Last post-volume sample emitted by this channel's mixer. Used
    /// as the starting point for the crossfade ramp on the next
    /// re-trigger (see `ramp_prev_sample`). Updated by `mix_one`
    /// every output frame.
    pub last_mixed_sample: f32,

    /// Volume that the previous note was being mixed at the moment
    /// this channel was *re-triggered*. Captured by `enter_row` on
    /// every fresh note-on (and on retrigger paths like E9x and the
    /// EDx delayed trigger) so the mixer can crossfade from this old
    /// value to the new note's first sample over a short ramp,
    /// instead of stepping discontinuously and producing the audible
    /// pop that a hard re-trigger leaves behind.
    ///
    /// Without the ramp, every note-on creates a discontinuity in the
    /// mixed bus equal to (last sample of the *old* note) → (first
    /// sample of the *new* note), which is typically a step of a few
    /// hundred i16 LSBs and accumulates into the per-trigger HF drift
    /// observed on real-world MODs (`halluc.mod`, `rhmst.mod`).
    pub ramp_prev_sample: f32,
    /// Number of output frames remaining in the per-trigger
    /// crossfade ramp. The ramp linearly interpolates from
    /// `ramp_prev_sample` to the new note's mixed sample. Set to
    /// [`PlayerState::RAMP_FRAMES`] on every fresh trigger; counted
    /// down by `mix_one` once per output frame.
    pub ramp_remaining_frames: u32,
}

/// Stores the note/sample/effect details of an EDy-delayed trigger so we
/// can fire it at the requested tick inside the same row.
#[derive(Clone, Copy, Debug, Default)]
pub struct DelayedTrigger {
    pub tick: u8,
    pub period: u16,
    pub sample: u8,
}

impl Channel {
    /// Return the period with any per-tick vibrato offset applied. Used
    /// only for the mixer step calculation; the raw `period` field is the
    /// un-modulated value (so chained effects like tone-porta keep
    /// compounding cleanly).
    ///
    /// We clamp to the *extended* period range `[108, 907]` here
    /// (`Protracker-effects-MODFIL12.txt` §3.2) rather than the porta
    /// limits `[113, 856]`: a vibrato peak / a finetune-extreme note
    /// must be free to land at e.g. period 108 (FT +7 B-3) without being
    /// re-clamped to 113. The mixer needs a positive period for the
    /// PAULA_CLOCK divide.
    fn effective_period(&self, vib_offset: i16) -> u16 {
        let p = self.period as i32 + vib_offset as i32;
        p.clamp(PERIOD_MIN_EXT as i32, PERIOD_MAX_EXT as i32) as u16
    }

    /// Mix one sample from this channel into the float accumulator.
    /// Returns the post-volume signal in `-1.0..=1.0`.
    fn mix_one(
        &mut self,
        samples: &[SampleBody],
        out_rate: f32,
        vib_offset: i16,
        trem_offset: i16,
    ) -> f32 {
        if !self.active || self.period == 0 {
            return 0.0;
        }
        let idx = self.sample_index as usize;
        if idx == 0 || idx > samples.len() {
            return 0.0;
        }
        let body = &samples[idx - 1];
        if body.pcm.is_empty() {
            return 0.0;
        }

        // Loop / end-of-sample wrap.
        //
        // Protracker quirk (Protracker-effects-MODFIL12.txt §2.2 + §2.8 +
        // Protracker-2.3A-misc-info.txt "Repeat point/length" notes):
        // for a looped sample the playable region is exactly
        // `loop_start..loop_start+loop_length`. Once the cursor reaches
        // `loop_end`, it wraps back into the loop region — it must NOT
        // continue reading past `loop_end` into the sample's "tail" (the
        // tail bytes are an artefact of how trackers store one-shot data
        // before the loop region starts and the looped tail; PT discards
        // them). The previous implementation only wrapped when
        // `pos >= pcm.len()`, which let the player read garbage past the
        // declared loop and produce audible glitches on real-world MODs
        // where loop_end < pcm.len().
        //
        // For a looped sample, also clamp loop_end to pcm.len() — some
        // real-world rips have slightly out-of-range repeat metadata.
        let pcm_len = body.pcm.len();
        let looped = body.is_looped();
        let effective_end_f = if looped {
            ((body.loop_start as usize + body.loop_length as usize).min(pcm_len)) as f32
        } else {
            pcm_len as f32
        };

        let pos = self.sample_pos;
        if pos >= effective_end_f {
            if looped {
                let loop_start = body.loop_start as f32;
                let span = effective_end_f - loop_start;
                if span > 0.0 {
                    let over = pos - loop_start;
                    self.sample_pos = loop_start + over.rem_euclid(span);
                } else {
                    self.active = false;
                    return 0.0;
                }
            } else {
                self.active = false;
                return 0.0;
            }
        }

        // Linear interpolation between two nearest samples. The next-sample
        // fetch must respect the loop boundary: if `i+1` lands on or past
        // `loop_end`, wrap to `loop_start` (looped) rather than reading the
        // tail.
        let effective_end_idx = effective_end_f as usize;
        let i = self.sample_pos as usize;
        let frac = self.sample_pos - i as f32;
        let s0_idx = i.min(pcm_len.saturating_sub(1));
        let s0 = body.pcm[s0_idx] as f32 / 128.0;
        let s1_idx = if i + 1 < effective_end_idx {
            i + 1
        } else if looped {
            body.loop_start as usize
        } else {
            s0_idx
        };
        let s1 = body.pcm[s1_idx.min(pcm_len.saturating_sub(1))] as f32 / 128.0;
        let interp = s0 + (s1 - s0) * frac;

        // Apply tremolo to the effective volume; clamp 0..=64.
        let eff_vol = (self.volume as i16 + trem_offset).clamp(0, 64);
        let out_raw = interp * (eff_vol as f32 / 64.0);

        // Per-trigger volume ramp (1 ms linear crossfade from
        // `ramp_prev_sample` to the live mix value). See
        // `PlayerState::RAMP_FRAMES` for the rationale + measurement
        // notes.
        let out = if self.ramp_remaining_frames > 0 {
            let total = PlayerState::RAMP_FRAMES as f32;
            let consumed = total - self.ramp_remaining_frames as f32;
            let t = (consumed / total).clamp(0.0, 1.0);
            let mixed = self.ramp_prev_sample * (1.0 - t) + out_raw * t;
            self.ramp_remaining_frames -= 1;
            if self.ramp_remaining_frames == 0 {
                // Once the ramp completes, stop tracking the previous
                // sample so a future re-trigger captures a fresh
                // baseline rather than this stale value.
                self.ramp_prev_sample = 0.0;
            }
            mixed
        } else {
            out_raw
        };

        // Track the last emitted sample so a future re-trigger can
        // crossfade from it (see `ramp_prev_sample`).
        self.last_mixed_sample = out;

        // Advance sample_pos by output-rate-scaled increment, using the
        // vibrato-modulated period for pitch.
        let eff_period = self.effective_period(vib_offset) as f32;
        let chan_rate = PAULA_CLOCK / eff_period;
        let step = chan_rate / out_rate;
        self.sample_pos += step;

        out
    }
}

/// Pending order/row jump scheduled by Bxx, Dxy, or E6x.
#[derive(Clone, Copy, Debug)]
struct Jump {
    /// Next order index (None = next order + 1).
    order: Option<u8>,
    /// Row to start at in the new pattern (default 0).
    row: u8,
}

/// Top-level player state. Owns samples, patterns, order, and the
/// per-channel mixer. Feeds `render(dst)` to fill an interleaved stereo
/// S16 buffer.
pub struct PlayerState {
    pub samples: Vec<SampleBody>,
    pub patterns: Vec<Pattern>,
    pub order: Vec<u8>,
    pub song_length: u8,

    pub channels: Vec<Channel>,
    pub speed: u8,
    pub bpm: u8,

    /// Current position in the order table (0..song_length).
    pub order_index: u8,
    /// Current row inside the current pattern (0..64).
    pub row: u8,
    /// Current tick inside the current row (0..speed).
    pub tick: u8,
    /// Samples emitted so far within the current tick.
    pub tick_sample_cursor: u32,

    pub sample_rate: u32,
    /// Flag set when the song has wrapped past its last order.
    pub ended: bool,

    /// Pending pattern break / position jump (consumed on tick advance).
    pending_jump: Option<Jump>,

    /// Per-pattern loop state for E6x. Four channels each track their own
    /// start row + remaining count independently (per spec).
    loop_rows: Vec<u8>,
    loop_counts: Vec<u8>,

    /// EEx pattern-delay: rows to repeat after the current one completes.
    pattern_delay: u8,

    /// True while we're inside an EE-induced repeat of the current row.
    /// Per Pro-Noise-Soundtracker-rev4.txt §[14][14] ("Delay pattern"):
    /// "all effects and previous notes continue during delay" — i.e. the
    /// note must NOT re-trigger and tick-0 effects must NOT re-fire on the
    /// repeated passes; only the per-tick (tick > 0) effect handler keeps
    /// running. Without this flag, `enter_row` would re-execute on every
    /// repeat and reset `sample_pos` / fine-volume slides would compound
    /// once per repeat — both audible regressions on real-world MODs that
    /// use EE for held-note textures.
    in_pattern_delay_repeat: bool,

    /// Amiga LED filter state. When ON, the mixer applies a 2-pole
    /// low-pass to all output streams (`Protracker-v1.1B-mod.txt` Cmd
    /// E0: "C-300E00 connects filter (turns power LED on)" — i.e.
    /// LED ON is the *default* on real Amigas, and the filter
    /// attenuates HF content). E01 disconnects the second (LED-gated
    /// Sallen-Key) pole at ~3.3 kHz, but the always-on first RC
    /// pole at ~5 kHz stays in place — that pole models the
    /// always-on anti-alias RC stage that sits between Paula's DAC
    /// and the audio jacks on every A500 / A1200 motherboard
    /// (documented in the MilkyTracker reference doc and the
    /// Polynominal "Amiga filter test" page — both are documentation,
    /// not third-party source).
    led_filter: bool,
    /// First filter pole (always on). For mixed stereo the first two
    /// slots hold L/R state; for planar per-channel output one slot
    /// per MOD channel. Don't mix `render` and `render_per_channel`
    /// calls on the same instance — the slot semantics differ and the
    /// filter state would smear across modes.
    led_filter_state: Vec<f32>,
    /// Second filter pole (only applied when LED is ON). Same slot
    /// shape as `led_filter_state`.
    led_filter_state2: Vec<f32>,
    /// Pre-computed filter coefficient for the always-on first pole
    /// (`FIXED_RC_CUTOFF_HZ`). Computed lazily on first render once
    /// `sample_rate` is known (NaN = "not yet computed").
    led_filter_alpha: f32,
    /// Pre-computed filter coefficient for the LED-controlled second
    /// pole (`LED_FILTER_CUTOFF_HZ`). NaN until first render.
    led_filter_alpha2: f32,

    /// Stereo pan separation in `0.0..=1.0`.
    ///
    /// `1.0` = full Amiga hard pan (channels 0/3 → only LEFT, 1/2 →
    /// only RIGHT, per `Protracker-effects-MODFIL12.txt` §11
    /// "Channels 1 and 4 are left, and 2 and 3 are right"). `0.0` =
    /// full mono (all channels in both speakers, like the legacy
    /// behaviour of `coreaudio` mono out). Defaults to `0.5` (see
    /// [`Self::DEFAULT_PAN_SEPARATION`]) — empirically the value
    /// that minimises cross-correlation drift versus a black-box
    /// reference render on real-world MODs (`halluc.mod`,
    /// `rhmst.mod`). `MODFIL12.txt` §11 itself recommends NOT
    /// pushing balance "all the way over to the left or to the
    /// right" ("Especially when using headphones"); a real-world
    /// MOD whose intro uses only the right-panned channels (1 + 2)
    /// — common in compositions like "hallucinations" — would
    /// otherwise produce a dead left ear for the entire intro,
    /// which sounds broken to listeners even though it is
    /// technically per-spec. Adjust via
    /// [`PlayerState::set_pan_separation`] if a strict hard-pan
    /// render (1.0) is required.
    pan_separation: f32,
}

impl PlayerState {
    pub fn new(
        header: &ModHeader,
        samples: Vec<SampleBody>,
        patterns: Vec<Pattern>,
        sample_rate: u32,
    ) -> Self {
        let channels = (0..header.channels)
            .map(|i| {
                // Initialise per-channel pan to the Amiga LRRL
                // hard-pan layout (channels 0 & 3 → 0/LEFT, 1 & 2 →
                // 255/RIGHT, repeating every 4). This matches
                // `channel_is_left` and keeps a MOD that never issues
                // 8xx / E8x rendering identically to the pre-r75
                // build (the per-channel `pan` simply re-derives the
                // hard-LRRL mix that `sample_all_channels` used to
                // compute from `channel_is_left(i)`).
                let pan = if Self::channel_is_left(i as usize) {
                    0
                } else {
                    255
                };
                Channel {
                    pan,
                    ..Channel::default()
                }
            })
            .collect::<Vec<_>>();
        let n_ch = channels.len();
        PlayerState {
            samples,
            patterns,
            order: header.order.clone(),
            song_length: header.song_length,
            channels,
            speed: DEFAULT_SPEED,
            bpm: DEFAULT_BPM,
            order_index: 0,
            row: 0,
            tick: 0,
            tick_sample_cursor: 0,
            sample_rate,
            ended: false,
            pending_jump: None,
            loop_rows: vec![0; n_ch],
            loop_counts: vec![0; n_ch],
            pattern_delay: 0,
            in_pattern_delay_repeat: false,
            // LED ON is the Amiga default at power-up — and the
            // ProTracker replayer leaves it on unless an explicit E01
            // toggles it off. See `Protracker-v1.1B-mod.txt` Cmd E0
            // ("C-300E00 connects filter (turns power LED on)").
            led_filter: true,
            led_filter_state: Vec::new(),
            led_filter_state2: Vec::new(),
            led_filter_alpha: f32::NAN,
            led_filter_alpha2: f32::NAN,
            pan_separation: Self::DEFAULT_PAN_SEPARATION,
        }
    }

    /// Default stereo pan separation (`0.5` = 50 %).
    ///
    /// See the `pan_separation` field doc-comment for the rationale.
    /// We default to **0.5** — half-way between the
    /// `Protracker-effects-MODFIL12.txt` §11 hard pan recommendation
    /// (which itself warns against full hard pan "especially when
    /// using headphones") and full mono. Empirically this is the
    /// value that minimises cross-correlation drift versus a
    /// black-box reference render on real-world MODs (`halluc.mod`,
    /// `rhmst.mod`) — a partial-bleed default is common practice.
    /// An r14 trial used 0.7 but on these specific fixtures 0.5
    /// yielded ~3 % better xcorr per 1 s window across the entire
    /// song. Override with [`PlayerState::set_pan_separation`].
    pub const DEFAULT_PAN_SEPARATION: f32 = 0.5;

    /// Override the stereo pan separation. `1.0` = full Amiga hard
    /// pan (channels 0/3 → LEFT only, 1/2 → RIGHT only). `0.0` =
    /// full mono. Values outside `0.0..=1.0` are clamped.
    pub fn set_pan_separation(&mut self, sep: f32) {
        self.pan_separation = sep.clamp(0.0, 1.0);
    }

    /// Read the current stereo pan separation.
    pub fn pan_separation(&self) -> f32 {
        self.pan_separation
    }

    /// Length of the per-trigger volume ramp, in output frames.
    /// 44 frames at 44.1 kHz ≈ 1 ms — enough to smooth the
    /// discontinuity at a fresh note-on without smearing percussive
    /// transients audibly.
    ///
    /// Without this ramp, every note re-trigger emits a single-sample
    /// step of magnitude `(prev_mixed - new_mixed)` into the post-mix
    /// L/R bus. On real-world MODs (`halluc.mod`, `rhmst.mod`) those
    /// steps were measured at ~5775 LSB and were the dominant
    /// contributor to per-second cross-correlation drift versus a
    /// black-box reference render. A 1 ms linear crossfade
    /// corresponds to the `Protracker-effects-MODFIL12.txt` §1.6
    /// note about Paula taking ~140 µs to settle on a fresh DMA
    /// fetch (we round up to one full output millisecond so the
    /// ramp is robust at any reasonable output sample rate).
    pub const RAMP_FRAMES: u32 = 44;

    /// Cutoff of the always-on first RC stage that sits between
    /// Paula's DAC and the audio jacks.
    ///
    /// Real-hardware schematic R/C values yield 1/(2πRC) ≈ 4400 Hz
    /// (rounded to **5000 Hz** in r14 of this module to model an
    /// "authentic" Amiga sound). However, the prevailing modern
    /// rendering convention effectively bypasses this stage in
    /// the default render path — only the LED-gated stage is
    /// modelled — because the real-hardware corner lops off audible
    /// content that listeners expect to hear back from a MOD
    /// render. With the strict 5 kHz value we measured
    /// cross-correlation drift against a black-box reference render
    /// on `rhmst.mod` of about 5 % (~0.94 vs ~0.99) — i.e. the
    /// always-on filter was the dominant contributor to per-second
    /// drift versus the modern reference.
    ///
    /// We default to **16000 Hz** to make the always-on stage
    /// audibly transparent (it still rolls off ultrasonic content
    /// past the resampler, which is the only PT-faithful purpose
    /// it serves at modern output rates).
    ///
    /// Documentation references: `paula-filter-notes.md` and the
    /// in-tree wiki snapshot under `docs/audio/trackers/mod/`
    /// (Resampling section).
    pub const FIXED_RC_CUTOFF_HZ: f32 = 16_000.0;

    /// Cutoff of the LED-controlled second RC stage (the one
    /// toggled by `E00` / `E01`).
    ///
    /// The real Amiga "Power LED" filter is a 12 dB/oct Sallen-Key
    /// cell with ~3.275 kHz nominal corner — that's the
    /// "spec-strict" value previously used here. However,
    /// `multimedia-cx-protracker.html` documents an
    /// **11500 Hz 1-pole approximation** that the modern PT
    /// rendering convention converges on, and every
    /// cross-correlation measurement against a black-box reference
    /// render confirms 11500 Hz is much closer to the modern-player
    /// ground truth than 3300 Hz is.
    /// The Sallen-Key value lops off so much HF content that
    /// real-world MOD files render audibly muffled compared to
    /// what their authors were probably listening to in their
    /// final mixdown — and this manifests as cross-correlation
    /// dips of 0.05–0.10 across most of `rhmst.mod` and
    /// `halluc.mod`.
    pub const LED_FILTER_CUTOFF_HZ: f32 = 11_500.0;

    /// Compute a 1-pole IIR alpha for the given cutoff at the
    /// player's output sample rate.
    /// `y[n] = a*x[n] + (1-a)*y[n-1]` with `a = 1 - exp(-2π·fc/fs)`.
    fn compute_alpha(sample_rate: u32, fc: f32) -> f32 {
        let fs = sample_rate as f32;
        let two_pi = 2.0 * std::f32::consts::PI;
        1.0 - (-two_pi * fc / fs).exp()
    }

    /// Ensure both filter state vectors have at least `n_outputs`
    /// slots and both alphas are initialised.
    fn ensure_led_filter(&mut self, n_outputs: usize) {
        if self.led_filter_state.len() < n_outputs {
            self.led_filter_state.resize(n_outputs, 0.0);
        }
        if self.led_filter_state2.len() < n_outputs {
            self.led_filter_state2.resize(n_outputs, 0.0);
        }
        if self.led_filter_alpha.is_nan() {
            self.led_filter_alpha = Self::compute_alpha(self.sample_rate, Self::FIXED_RC_CUTOFF_HZ);
        }
        if self.led_filter_alpha2.is_nan() {
            self.led_filter_alpha2 =
                Self::compute_alpha(self.sample_rate, Self::LED_FILTER_CUTOFF_HZ);
        }
    }

    /// Apply the two-stage Amiga output filter on slot `idx`. The
    /// first RC stage is *always* on (it models the always-on
    /// 4-5 kHz anti-alias post-DAC pole that exists on every
    /// A500 / A1200 motherboard regardless of LED state). The
    /// second pole is gated on `led_filter`, modelling the
    /// `E00 / E01` LED-controlled Sallen-Key corner at ~3.3 kHz.
    #[inline]
    fn led_filter_step(&mut self, idx: usize, x: f32) -> f32 {
        // Stage 1 — always on.
        let a1 = self.led_filter_alpha;
        let prev1 = self.led_filter_state[idx];
        let y1 = a1 * x + (1.0 - a1) * prev1;
        self.led_filter_state[idx] = y1;

        if !self.led_filter {
            return y1;
        }

        // Stage 2 — gated on LED.
        let a2 = self.led_filter_alpha2;
        let prev2 = self.led_filter_state2[idx];
        let y2 = a2 * y1 + (1.0 - a2) * prev2;
        self.led_filter_state2[idx] = y2;
        y2
    }

    /// Samples-per-tick rounded down. Classic formula is
    /// `sample_rate * 2.5 / BPM`.
    pub fn samples_per_tick(&self) -> u32 {
        ((self.sample_rate as f32) * 2.5 / self.bpm as f32) as u32
    }

    /// Process the row at the current position (called at tick 0).
    fn enter_row(&mut self) {
        let pattern_idx = self
            .order
            .get(self.order_index as usize)
            .copied()
            .unwrap_or(0) as usize;
        if pattern_idx >= self.patterns.len() {
            self.ended = true;
            return;
        }
        let row_notes: Vec<Note> = self.patterns[pattern_idx].rows[self.row as usize].clone();
        for (ch_idx, note) in row_notes.iter().enumerate() {
            if ch_idx >= self.channels.len() {
                break;
            }

            let effect = note.effect;
            let param = note.effect_param;
            let x = param >> 4;
            let y = param & 0x0F;

            let ch = &mut self.channels[ch_idx];

            // Arpeggio cleanup: if the previous row was running arpeggio
            // (effect 0 with non-zero param), `ch.period` was left at a
            // semitone-shifted value by the last tick (per FireLight
            // tutorial §5.1: tick%3==1 → +x, tick%3==2 → +y). At a row
            // boundary the spec calls for "Tick 0 set frequency to
            // normal value" (FireLight §5.1) — i.e. the un-modulated
            // base. Restore `ch.period` to `ch.arp_base_period` before
            // any new-row decisions so:
            //   1. The mixer plays tick 0 of the new row at the correct
            //      base pitch instead of whatever leftover modulated
            //      value the prior row's last arp tick left behind.
            //   2. The "no new note" branch below does not capture the
            //      stale modulated period as a NEW `arp_base_period` —
            //      that would compound the modulation across every row
            //      with continuing arpeggio (audible on `cyber.mod`'s
            //      pat-1 ch-2 lead, rows 32-58, where the effect-0 cell
            //      pattern is "trigger / continue / trigger / trigger
            //      / trigger / continue / ..." — every "continue" row
            //      played at the prior tick's modulated period and
            //      then re-modulated from that, raising the pitch by
            //      ~3 semitones per non-trigger row).
            //   3. Other effects starting on this row (porta up/down,
            //      tone porta, fine slides) operate on the true base
            //      period rather than a modulated one.
            // See `Protracker-effects-MODFIL12.txt` 0:Arpeggio and
            // FireLight-MOD-Player-Tutorial.txt §5.1.
            if ch.effect == 0x0 && ch.effect_param != 0 && ch.arp_base_period != 0 {
                ch.period = ch.arp_base_period;
            }

            ch.effect = effect;
            ch.effect_param = param;
            ch.cut_tick = 0;
            ch.delay = None;
            ch.retrig_ticks = 0;

            // Sample change.
            //
            // Protracker quirk (Protracker-effects-MODFIL12.txt §3.2 +
            // Pro-Noise-Soundtracker-rev4.txt:113-118): when a sample
            // number is specified WITHOUT a note, PT loads the new
            // sample's default volume + finetune immediately, but does
            // NOT swap the currently-playing sample on the channel —
            // the swap happens at the next note trigger. Loading the
            // sample index too early was an audible bug: a row with
            // "C-2 05 ___ : --- 03" would suddenly play sample 3's PCM
            // at sample 5's pitch, producing wrong-instrument artefacts
            // on real-world MODs that use this idiom (e.g. setting up
            // the next note's volume on the row before the trigger).
            //
            // We track the "pending" sample number in `sample_index`
            // separately from the actively-playing sample, but since the
            // mixer reads `sample_index`, the safest fix is: only update
            // `sample_index` when a note also triggers (or on a tone-
            // portamento / note-delay row, where the trigger is
            // explicit). Otherwise, defer the swap and only latch the
            // default volume + finetune (used by the next note).
            let has_note = note.period != 0;
            let is_note_delay_pre = note.effect == 0xE && (note.effect_param >> 4) == 0xD;
            if note.sample != 0 {
                let idx = note.sample as usize;
                if idx >= 1 && idx <= self.samples.len() {
                    let body = &self.samples[idx - 1];
                    ch.volume = body.volume;
                    ch.finetune = body.finetune;
                }
                if has_note || is_note_delay_pre {
                    // A trigger is happening on this row — latch the
                    // sample index so the new note plays the new sample.
                    ch.sample_index = note.sample;
                } else {
                    // No trigger: remember the requested swap for the
                    // next note. Volume + finetune are already applied
                    // (PT behaviour); the active sample stays.
                    ch.pending_sample = note.sample;
                }
            }

            let is_tone_porta = matches!(effect, 0x3 | 0x5);
            let is_note_delay = effect == 0xE && x == 0xD;

            // Tone portamento: record target, but DO NOT retrigger.
            if note.period != 0 && is_tone_porta {
                ch.tone_porta_target = note.period;
                if effect == 0x3 && param != 0 {
                    ch.tone_porta_speed = param;
                }
                // Speed 0 on Cmd 3 inherits last; Cmd 5 never sets speed.
                ch.arp_base_period = ch.period;
            } else if note.period != 0 && is_note_delay {
                // Delay the trigger to tick y; continue previous note until then.
                ch.delay = Some(DelayedTrigger {
                    tick: y,
                    period: note.period,
                    sample: note.sample,
                });
                ch.arp_base_period = ch.period;
            } else if note.period != 0 {
                // Normal note trigger — apply E5 finetune if it lands on this row.
                let mut note_period = note.period;
                if effect == 0xE && x == 0x5 {
                    // E5x: set finetune and re-derive the period from the
                    // note index.
                    let new_ft = y as i8;
                    let signed_ft = if new_ft & 0x8 != 0 {
                        new_ft - 16
                    } else {
                        new_ft
                    };
                    ch.finetune = signed_ft;
                    if let Some(note_idx) = note_index_for_period(note.period) {
                        note_period = PERIOD_TABLE[finetune_row(signed_ft)][note_idx];
                    }
                }
                ch.period = note_period;

                // Consume any deferred sample swap from a previous row
                // that wrote a sample number without a note.
                if note.sample == 0 && ch.pending_sample != 0 {
                    ch.sample_index = ch.pending_sample;
                }
                ch.pending_sample = 0;

                // 9xx: start from an offset instead of the sample start.
                let mut offset_frames: u32 = 0;
                if effect == 0x9 {
                    let used = if param == 0 {
                        ch.mem_sample_offset
                    } else {
                        param
                    };
                    ch.mem_sample_offset = used;
                    offset_frames = (used as u32) * 0x100;
                }

                // 9xx out-of-range quirk: if the offset lands at or past
                // the end of the sample body, ProTracker plays NO NOTE at
                // all on this channel (Protracker-effects-MODFIL12.txt
                // 9:Set-sample-offset, lines 1240-1242: "Note that if the
                // effect is out of range (e.g. if it tries to jump beyond
                // the end of the sample) NO NOTE WILL BE PLAYED!"). We
                // detect this at trigger time and silence the channel
                // instead of letting the mixer wrap a looped sample's
                // over-range cursor back into the loop region (which would
                // audibly play the loop from a fresh trigger — exactly the
                // artefact the spec says should not happen). The check
                // applies only when a 9xx offset was actually requested;
                // a plain note with no 9xx keeps the original semantics.
                if effect == 0x9 {
                    let sample_len = (ch.sample_index as usize)
                        .checked_sub(1)
                        .and_then(|i| self.samples.get(i))
                        .map(|b| b.pcm.len())
                        .unwrap_or(0);
                    if offset_frames as usize >= sample_len {
                        // No note: leave the channel inactive and skip the
                        // rest of the trigger (position / ramp / LFO
                        // retrigger). arp_base_period still anchors to the
                        // intended note period so a following effect-0 row
                        // has a sane base, matching the "note info is still
                        // updated, just not played" reading.
                        ch.active = false;
                        ch.sample_pos = 0.0;
                        ch.arp_base_period = note_period;
                        continue;
                    }
                }

                ch.sample_pos = offset_frames as f32;
                ch.active = true;
                ch.arp_base_period = note_period;

                // Per-trigger volume ramp: capture the value the
                // mixer last emitted for this channel and arm a short
                // crossfade. See `PlayerState::RAMP_FRAMES` for why.
                ch.ramp_prev_sample = ch.last_mixed_sample;
                ch.ramp_remaining_frames = PlayerState::RAMP_FRAMES;

                // Retrigger vibrato / tremolo unless waveform says otherwise.
                if !ch.vib_wave.no_retrigger {
                    ch.vib_pos = 0;
                }
                if !ch.trem_wave.no_retrigger {
                    ch.trem_pos = 0;
                }
            } else {
                // No note — keep arp base anchored to the current period.
                ch.arp_base_period = ch.period;
            }

            // Tick-0 effects.
            apply_tick0_effect(
                ch_idx,
                effect,
                param,
                &mut self.channels,
                &mut self.pending_jump,
                &mut self.loop_rows,
                &mut self.loop_counts,
                &mut self.pattern_delay,
                self.order_index,
                self.row,
            );
        }

        // Fxx applies immediately on tick 0 — handle after per-channel dispatch.
        // A later channel's Fxx supersedes an earlier one (PT behaviour, per
        // `Pro-Noise-Soundtracker-rev4.txt:375-377`: "the ones on
        // higher-numbered channels take precedence over the
        // ones on lower-numbered channels").
        //
        // Boundary check (Pro-Noise-Soundtracker-rev4.txt:362-365): the
        // doc reads "If z<=32, then it means 'set ticks/division to z'
        // otherwise it means 'set beats/minute to z' (convention says
        // that this should read 'If z<32...')". We follow the
        // convention (z < 0x20 → speed, else BPM), so 0x20 (= 32) is
        // the smallest BPM value, matching `Protracker-v1.1B-mod.txt`
        // and the FireLight tutorial. 0x1F is the largest speed value.
        for ch in &self.channels {
            if ch.effect == 0xF {
                let p = ch.effect_param;
                if p == 0 {
                    // Fxx 00 — ProTracker treats this as "halt" (set
                    // order_index past the end). Safer default: ignore.
                } else if p < 0x20 {
                    self.speed = p;
                } else {
                    self.bpm = p;
                }
            }
        }

        // E0x LED filter: same per-row last-channel-wins resolution.
        // `Protracker-v1.1B-mod.txt` Cmd E0: E00 connects filter (LED on),
        // E01 disconnects (LED off).
        let mut new_led: Option<bool> = None;
        for ch in &mut self.channels {
            if let Some(new_state) = ch.pending_led.take() {
                new_led = Some(new_state);
            }
        }
        if let Some(s) = new_led {
            self.led_filter = s;
        }
    }

    /// Advance one tick (called at the start of every tick).
    fn advance_tick(&mut self) {
        if self.tick == 0 {
            // Pattern-delay repeat: per Pro-Noise §[14][14] ("all effects
            // and previous notes continue during delay") we must NOT
            // re-execute `enter_row` on the repeated passes — that would
            // re-trigger notes (resetting sample_pos to 0) and re-fire
            // tick-0 effects (compounding fine vol slides etc.). The
            // currently-playing notes simply continue; tick-N effects on
            // the inbetween ticks still run normally below.
            if !self.in_pattern_delay_repeat {
                self.enter_row();
            }
        } else {
            // Tick-N effects run per channel.
            for ch_idx in 0..self.channels.len() {
                apply_tickn_effect(ch_idx, self.tick, &mut self.channels, &self.samples);
            }
        }
    }

    /// Move to next row (or jump).
    fn next_row(&mut self) {
        // EEx pattern delay: repeat the current row `pattern_delay` more
        // times. Set `in_pattern_delay_repeat` so the next pass through
        // tick 0 skips `enter_row` (no note re-trigger, no tick-0 effect
        // re-fire) per Pro-Noise-Soundtracker-rev4.txt §[14][14].
        if self.pattern_delay > 0 {
            self.pattern_delay -= 1;
            self.in_pattern_delay_repeat = true;
            return;
        }
        // Real row advance — clear the repeat flag so the next row's
        // tick-0 processing runs normally.
        self.in_pattern_delay_repeat = false;
        if let Some(jump) = self.pending_jump.take() {
            if let Some(order) = jump.order {
                self.order_index = order;
            } else {
                self.order_index = self.order_index.saturating_add(1);
            }
            self.row = jump.row;
        } else {
            self.row += 1;
            if self.row as usize >= PATTERN_ROWS {
                self.row = 0;
                self.order_index = self.order_index.saturating_add(1);
            }
        }
        if self.order_index >= self.song_length {
            self.ended = true;
        }
    }

    /// True if track index `i` is hard-panned left under the classic
    /// Amiga convention (channels 0 & 3 left, 1 & 2 right; for >4 channels
    /// the pattern repeats every 4).
    pub fn channel_is_left(i: usize) -> bool {
        matches!(i % 4, 0 | 3)
    }

    /// Compute the instantaneous vibrato period offset for this channel.
    fn vibrato_offset(ch: &Channel) -> i16 {
        let rate = ch.mem_vibrato >> 4;
        let depth = ch.mem_vibrato & 0x0F;
        if depth == 0 || ch.effect != 0x4 && ch.effect != 0x6 {
            // Vibrato is only active on ticks where 4xy or 6xy is the current
            // effect. A subsequent non-vibrato effect on the same channel
            // stops the modulation.
            let _ = rate; // keep rate referenced even in this path
            return 0;
        }
        let idx = (ch.vib_pos.unsigned_abs() & 31) as usize;
        let base = match ch.vib_wave.shape {
            0 | 3 => PROTRACKER_SINE_TABLE[idx] as i32,
            1 => {
                // Ramp down: |pos|<<3 with 255-x on the negative half.
                let raw = (idx << 3) as i32;
                if ch.vib_pos < 0 {
                    255 - raw
                } else {
                    raw
                }
            }
            _ => 255, // square
        };
        let delta = (base * depth as i32) >> 7;
        if ch.vib_pos < 0 {
            -(delta as i16)
        } else {
            delta as i16
        }
    }

    /// Compute the instantaneous tremolo volume offset for this channel.
    fn tremolo_offset(ch: &Channel) -> i16 {
        let depth = ch.mem_tremolo & 0x0F;
        if depth == 0 || ch.effect != 0x7 {
            return 0;
        }
        let idx = (ch.trem_pos.unsigned_abs() & 31) as usize;
        let base = match ch.trem_wave.shape {
            0 | 3 => PROTRACKER_SINE_TABLE[idx] as i32,
            1 => {
                let raw = (idx << 3) as i32;
                if ch.trem_pos < 0 {
                    255 - raw
                } else {
                    raw
                }
            }
            _ => 255,
        };
        // Tremolo divides by 64 (half the vibrato denominator) so the
        // peak delta maps to volume units.
        let delta = (base * depth as i32) >> 6;
        if ch.trem_pos < 0 {
            -(delta as i16)
        } else {
            delta as i16
        }
    }

    /// Sample all channels once, returning per-channel floats in
    /// `-1.0..=1.0` range (pre-pan, pre-mix) and a stereo mix scaled so
    /// that a fully-saturated 4-channel MOD stays within the -1..1 range.
    ///
    /// Pan model: with separation `s ∈ [0, 1]`, a hard-left channel
    /// contributes `(1 + s) / 2` to L and `(1 - s) / 2` to R; vice
    /// versa for hard-right. So `s = 1` reproduces pure Amiga hard
    /// pan (channels 0/3 → only L, 1/2 → only R) and `s = 0` is
    /// full mono. The default `0.5` (see `pan_separation` field)
    /// keeps an intro that uses only right-panned channels (1 + 2,
    /// per the convention in `Protracker-effects-MODFIL12.txt` §11)
    /// audible on the left speaker too — the modern PT rendering
    /// convention applies the same partial bleed for the
    /// headphone-fatigue reason called out in MODFIL12.txt itself
    /// ("Especially when using headphones").
    /// Without it, real-world MODs whose intros use only
    /// right-panned voices (e.g. "hallucinations" by ???) sound
    /// broken to a listener whose left ear receives no signal at
    /// all for several seconds.
    fn sample_all_channels(&mut self, per_channel: &mut [f32]) -> (f32, f32) {
        let out_rate = self.sample_rate as f32;
        let mut l = 0.0f32;
        let mut r = 0.0f32;
        let n_ch = self.channels.len();
        let s = self.pan_separation;
        // Per-channel pan derives the L/R gain pair from `ch.pan`
        // (0 = hard LEFT, 128 = centre, 255 = hard RIGHT) per the
        // 8xx / E8x spec. The `pan_separation` global narrows the L↔R
        // gap symmetrically around the centre: a fully panned channel
        // contributes `(1 + s) / 2` to its near speaker and
        // `(1 - s) / 2` to the far one. A centred channel (pan = 128)
        // splits evenly regardless of separation. This collapses to
        // the pre-r75 hard-LRRL formula whenever `ch.pan` is 0 or
        // 255 (which is the LRRL default initialised by `new`), so
        // the black-box reference-render calibration in the divisor
        // below still holds bit-for-bit.
        for (i, ch) in self.channels.iter_mut().enumerate() {
            let vib = Self::vibrato_offset(ch);
            let trem = Self::tremolo_offset(ch);
            let smp = ch.mix_one(&self.samples, out_rate, vib, trem);
            per_channel[i] = smp;
            let (gl, gr) = pan_gains(ch.pan, s);
            l += smp * gl;
            r += smp * gr;
        }
        // Mix-bus headroom divisor.
        //
        // Round 19 calibration against a trace reference impl (a
        // pure black-box runtime-loaded reference renderer driven
        // through its published C entry-points by
        // `tests/tracker_reference_compare.rs` — see that file's
        // header for the dlopen flow + clean-config protocol).
        // With every audio-shaping colouration disabled on the
        // reference (oversampling, bass-boost, surround, noise
        // reduction off; LINEAR resampling; 44100 Hz / 16-bit /
        // stereo; master volume default 128/512; stereo_separation
        // 128/256 = 50 %), a single max-volume hard-left channel
        // triggered on a 4-channel `M.K.` MOD lands the reference
        // L peak at 8500 / 32767 = 0.2594 — which only works out
        // arithmetically if the reference's per-channel headroom
        // divisor is **3**, not the **2** we previously used. The
        // pan formula itself (near = (1+s)/2, far = (1-s)/2)
        // matches across the two engines bit-exact; only the
        // headroom divisor differs.
        //
        // Empirically the reference formula is `n_ch/2 + 1` — i.e.
        // 3 for 4-ch, 4 for 6-ch, 5 for 8-ch, etc. That gives a
        // ~1.5× lower per-channel max gain than the strict `n_ch/2`
        // headroom we shipped before, so a single channel never
        // dominates the bus at 84 % of i16 peak the way ours did
        // (which the user-reported "scrambled audio at 4.5s" hunt
        // bracketed as the most-likely cause of downstream-clipping
        // perceived as scrambling on real-world MODs `halluc.mod`
        // and `rhmst.mod`). The peak ratio measured against the
        // reference drops from 1.506× to roughly 1.0× after this
        // change — confirmed by the `tracker_reference_calibration_*`
        // test in `tests/tracker_reference_compare.rs`.
        //
        // No reference source code is read; the divisor is derived
        // purely from black-box measurement of the published-API
        // output PCM at known input parameters. See the in-tree
        // wiki snapshot under `docs/audio/trackers/mod/`
        // ("Resampling and mixing") which documents the same
        // "channel count + safety margin" pattern as the modern
        // ProTracker rendering convention — though no third-party
        // source code is referenced.
        let norm = ((n_ch as f32 / 2.0) + 1.0).max(1.0);
        (l / norm, r / norm)
    }

    /// Render one stereo S16 interleaved sample pair by mixing all
    /// channels. Channels 0/3 pan toward LEFT, 1/2 toward RIGHT
    /// (Amiga convention) — the strength of the L↔R separation is
    /// controlled by [`pan_separation`](Self::pan_separation), which
    /// defaults to `DEFAULT_PAN_SEPARATION` (`0.5`) rather than full
    /// hard pan to keep intros that only use one side audible on
    /// both ears. Set to `1.0` for strict spec-faithful Amiga
    /// hard pan.
    fn render_one(&mut self, out: &mut [i16]) {
        let mut per_channel = vec![0.0f32; self.channels.len()];
        let (l, r) = self.sample_all_channels(&mut per_channel);
        // Amiga LED low-pass filter — applied post-mix on the L/R bus
        // (the real Amiga filter sits between Paula's DAC and the audio
        // jacks, common to both stereo channels). See
        // `multimedia-cx-protracker.html` E0x.
        self.ensure_led_filter(2);
        let l = self.led_filter_step(0, l);
        let r = self.led_filter_step(1, r);
        let l = l.clamp(-1.0, 1.0);
        let r = r.clamp(-1.0, 1.0);
        out[0] = (l * 32767.0) as i16;
        out[1] = (r * 32767.0) as i16;
    }

    /// Render `n_frames` stereo samples into `dst` (interleaved S16,
    /// length = n_frames * 2). Returns samples actually rendered (may be
    /// less than requested if song ends).
    pub fn render(&mut self, dst: &mut [i16]) -> usize {
        assert!(dst.len() % 2 == 0);
        let mut produced = 0usize;
        let total_frames = dst.len() / 2;

        while produced < total_frames {
            if self.ended {
                break;
            }
            if self.tick_sample_cursor == 0 {
                self.advance_tick();
            }
            let spt = self.samples_per_tick().max(1);
            let remaining_in_tick = spt.saturating_sub(self.tick_sample_cursor);
            let want = (total_frames - produced).min(remaining_in_tick as usize);

            for _ in 0..want {
                let off = produced * 2;
                self.render_one(&mut dst[off..off + 2]);
                produced += 1;
            }

            self.tick_sample_cursor += want as u32;
            if self.tick_sample_cursor >= spt {
                self.tick_sample_cursor = 0;
                self.tick += 1;
                if self.tick >= self.speed {
                    self.tick = 0;
                    self.next_row();
                }
            }
        }
        produced
    }

    /// Render into one S16 plane per MOD channel. `planes.len()` must
    /// equal `self.channels.len()`; each plane receives the same number
    /// of samples, and all planes must be at least `n_frames` long.
    pub fn render_per_channel(&mut self, planes: &mut [&mut [i16]], n_frames: usize) -> usize {
        assert_eq!(
            planes.len(),
            self.channels.len(),
            "render_per_channel: plane count must equal MOD channel count"
        );
        for p in planes.iter() {
            assert!(
                p.len() >= n_frames,
                "render_per_channel: every plane must hold at least n_frames samples"
            );
        }

        let mut produced = 0usize;
        let mut scratch = vec![0.0f32; self.channels.len()];

        while produced < n_frames {
            if self.ended {
                break;
            }
            if self.tick_sample_cursor == 0 {
                self.advance_tick();
            }
            let spt = self.samples_per_tick().max(1);
            let remaining_in_tick = spt.saturating_sub(self.tick_sample_cursor);
            let want = (n_frames - produced).min(remaining_in_tick as usize);

            // For per-channel output the Amiga LED filter is applied
            // independently on each plane — a downstream consumer that
            // re-mixes the planes still sees the same global filter
            // shape that mixed playback would produce. We allocate one
            // filter slot per plane.
            let n_planes = planes.len();
            for _ in 0..want {
                // Discard the stereo mix; we only need per-channel here.
                let _ = self.sample_all_channels(&mut scratch);
                self.ensure_led_filter(n_planes);
                for (ch_idx, plane) in planes.iter_mut().enumerate() {
                    let raw = scratch[ch_idx];
                    let filtered = self.led_filter_step(ch_idx, raw);
                    let v = filtered.clamp(-1.0, 1.0);
                    plane[produced] = (v * 32767.0) as i16;
                }
                produced += 1;
            }

            self.tick_sample_cursor += want as u32;
            if self.tick_sample_cursor >= spt {
                self.tick_sample_cursor = 0;
                self.tick += 1;
                if self.tick >= self.speed {
                    self.tick = 0;
                    self.next_row();
                }
            }
        }
        produced
    }
}

/// Apply tick-0 (row-start) effects. `ch_idx` identifies the channel
/// inside `channels`; shared song-level state (pending_jump, pattern delay
/// etc.) is passed by &mut so a single effect can mutate both.
#[allow(clippy::too_many_arguments)]
fn apply_tick0_effect(
    ch_idx: usize,
    effect: u8,
    param: u8,
    channels: &mut [Channel],
    pending_jump: &mut Option<Jump>,
    loop_rows: &mut [u8],
    loop_counts: &mut [u8],
    pattern_delay: &mut u8,
    order_index: u8,
    row: u8,
) {
    let x = param >> 4;
    let y = param & 0x0F;
    let ch = &mut channels[ch_idx];
    match effect {
        0x0 => {
            // Arpeggio — parameter memory is just the row's x/y; no state
            // needed on tick 0 beyond setting the arp base period (done in
            // enter_row) and remembering the param for subsequent ticks.
        }
        0x1
            // Portamento up: remember param for tick-N dispatch.
            if param != 0 => {
                ch.mem_porta_up = param;
            }
        0x2
            // Portamento down: remember param.
            if param != 0 => {
                ch.mem_porta_down = param;
            }
        0x3
            // Tone portamento: speed 0 reuses the stored value.
            if param != 0 => {
                ch.tone_porta_speed = param;
            }
        0x4 => {
            // Vibrato: nibble param memory (0 nibbles reuse previous values).
            let mut rate = x;
            let mut depth = y;
            if rate == 0 {
                rate = ch.mem_vibrato >> 4;
            }
            if depth == 0 {
                depth = ch.mem_vibrato & 0x0F;
            }
            ch.mem_vibrato = (rate << 4) | depth;
        }
        0x5
            // Tone portamento + volume slide. Reuse stored tone-porta speed;
            // param here is the volume-slide nibble pair.
            if param != 0 => {
                ch.mem_volslide = param;
            }
        0x6
            // Vibrato + volume slide. Vibrato params are inherited.
            if param != 0 => {
                ch.mem_volslide = param;
            }
        0x7 => {
            // Tremolo: nibble param memory.
            let mut rate = x;
            let mut depth = y;
            if rate == 0 {
                rate = ch.mem_tremolo >> 4;
            }
            if depth == 0 {
                depth = ch.mem_tremolo & 0x0F;
            }
            ch.mem_tremolo = (rate << 4) | depth;
        }
        0x8 => {
            // 8xx: Set FINE Panning (FT2 extension).
            // `Protracker-effects-MODFIL12.txt` lines 1201-1207:
            //   "Command 8: Set FINE Panning.
            //    Even if the AMIGA PROTracker does not support it,
            //    this effect is used by modern MOD's and supported
            //    by nearly all modern PC MOD player routines.
            //    xxxxyyyy = panning position. (0=Most left,
            //    255=most right.)"
            // The classic Amiga ProTracker hard-pan layout (LRRL,
            // see `channel_is_left`) is overridden per channel by
            // this command until a new 8xx / E8x sets a different
            // value. No memory semantics in the spec (8xx is
            // explicitly not memorised between rows).
            ch.pan = param;
        }
        0x9 => {
            // 9xx: handled at note-trigger time (enter_row). Nothing to do here
            // since we already latched the memory and the position.
        }
        0xA
            // Volume slide — per-tick param memory.
            if param != 0 => {
                ch.mem_volslide = param;
            }
        0xB => {
            // Position jump.
            *pending_jump = Some(Jump {
                order: Some(param),
                row: 0,
            });
        }
        0xC => {
            // Set volume, clamped to 64.
            ch.volume = param.min(64);
        }
        0xD => {
            // Pattern break: row x*10 + y in the NEXT order.
            let next_row = (x * 10 + y).min(63);
            *pending_jump = Some(Jump {
                order: None,
                row: next_row,
            });
        }
        0xE => apply_extended_tick0(
            ch_idx,
            x,
            y,
            channels,
            pending_jump,
            loop_rows,
            loop_counts,
            pattern_delay,
            order_index,
            row,
        ),
        0xF => {
            // Handled at the song level in enter_row (speed vs BPM split).
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_extended_tick0(
    ch_idx: usize,
    x: u8,
    y: u8,
    channels: &mut [Channel],
    pending_jump: &mut Option<Jump>,
    loop_rows: &mut [u8],
    loop_counts: &mut [u8],
    pattern_delay: &mut u8,
    order_index: u8,
    row: u8,
) {
    let ch = &mut channels[ch_idx];
    match x {
        0x0 => {
            // E0x: set Amiga LED filter on/off. Per
            // `Protracker-v1.1B-mod.txt` Cmd E0:
            //   E00 = "connects filter (turns power LED on)"
            //   E01 = "disconnects filter (turns power LED off)"
            // Real-world MODs use this for tonal contrast (filter ON
            // gives warmth via a 1-pole lowpass at ~11.5 kHz). We can't
            // reach the song-level state from here, so we annotate the
            // channel and the caller propagates it after the per-channel
            // dispatch loop completes.
            ch.pending_led = Some(y == 0);
        }
        0x1 => {
            // E1x: fine portamento up — one-shot slide on tick 0.
            ch.period = ch.period.saturating_sub(y as u16).max(PERIOD_MIN);
        }
        0x2 => {
            // E2x: fine portamento down — clamp at C-1 (period 856) per
            // `Protracker-v1.1B-mod.txt` Cmd E2 ("works just like the
            // normal portamento down" -> 2xx limit applies).
            ch.period = (ch.period + y as u16).min(PERIOD_MAX);
        }
        0x3 => {
            // E3x: glissando control.
            ch.glissando = y != 0;
        }
        0x4 => {
            // E4x: set vibrato waveform.
            ch.vib_wave.set(y);
        }
        0x5 => {
            // E5x: set finetune (note retrigger path handled in enter_row).
            // If no note triggered this row, update finetune for future notes.
            let raw = y;
            let signed = if raw & 0x8 != 0 {
                (raw as i8) - 16
            } else {
                raw as i8
            };
            ch.finetune = signed;
        }
        0x6 => {
            // E6x: pattern loop (per-channel). When looping back, schedule
            // the jump via `pending_jump` so the rest of the row's ticks
            // still complete before we rewind.
            if y == 0 {
                loop_rows[ch_idx] = row;
            } else if loop_counts[ch_idx] == 0 {
                loop_counts[ch_idx] = y;
                *pending_jump = Some(Jump {
                    order: Some(order_index),
                    row: loop_rows[ch_idx],
                });
            } else {
                loop_counts[ch_idx] -= 1;
                if loop_counts[ch_idx] > 0 {
                    *pending_jump = Some(Jump {
                        order: Some(order_index),
                        row: loop_rows[ch_idx],
                    });
                }
            }
        }
        0x7 => {
            // E7x: set tremolo waveform.
            ch.trem_wave.set(y);
        }
        0x8 => {
            // E8x: Set (Rough) Panning.
            // `Protracker-effects-MODFIL12.txt` lines 1503-1505:
            //   "Command $E8: Set (Rough) Panning. (also called
            //    MTM panning)
            //    yyyy = panning value. $0 = most left, $F = most
            //    right."
            // The spec doesn't pin a specific 4 → 8 bit upscale, but
            // the convention echoed in `multimedia-cx-protracker.html`
            // E8x ("Another stereo extension. $0 is hard left, $F is
            // hard right.") is for the nibble to span the same range
            // as 8xx. Replicating the nibble into both halves of the
            // byte (`y << 4 | y`) gives the correct endpoints (0x00,
            // 0xFF) and a monotonic centre (0x77/0x88 either side of
            // the midpoint), which is the standard "nibble panning"
            // mapping used by every modern PT-compatible player.
            ch.pan = (y << 4) | y;
        }
        0x9 => {
            // E9x: retrig note — parameter captured here, per-tick handler
            // actually replays the sample.
            ch.retrig_ticks = y;
        }
        0xA => {
            // EAx: fine volume slide up.
            ch.volume = (ch.volume as u16 + y as u16).min(64) as u8;
        }
        0xB => {
            // EBx: fine volume slide down.
            ch.volume = ch.volume.saturating_sub(y);
        }
        0xC => {
            // ECx: note cut — the *cut* actually happens at tick x.
            ch.cut_tick = y;
        }
        0xD => { /* EDx: note delay — handled in enter_row (ch.delay). */ }
        0xE => {
            // EEx: pattern delay — delay row by y more rows' worth of ticks.
            *pattern_delay = y;
        }
        0xF => { /* EFx: invert loop — deliberately not implemented (spec
             says "This effect is not supported in any player or
             tracker. Don't bother with it"). */
        }
        _ => {}
    }
}

/// Apply per-tick (tick > 0) effects for one channel.
fn apply_tickn_effect(ch_idx: usize, tick: u8, channels: &mut [Channel], samples: &[SampleBody]) {
    let ch = &mut channels[ch_idx];
    let effect = ch.effect;
    let param = ch.effect_param;
    let x = param >> 4;
    let y = param & 0x0F;

    // EDx (note delay): when we reach the stored tick, trigger the note.
    if let Some(delayed) = ch.delay {
        if tick == delayed.tick {
            if delayed.sample != 0 {
                let idx = delayed.sample as usize;
                ch.sample_index = delayed.sample;
                if idx >= 1 && idx <= samples.len() {
                    let body = &samples[idx - 1];
                    ch.volume = body.volume;
                    ch.finetune = body.finetune;
                }
            } else if ch.pending_sample != 0 {
                // Consume any deferred sample swap from a previous row.
                ch.sample_index = ch.pending_sample;
            }
            ch.pending_sample = 0;
            ch.period = delayed.period;
            ch.sample_pos = 0.0;
            ch.active = true;
            ch.arp_base_period = delayed.period;
            // Per-trigger ramp on the EDx delayed retrigger.
            ch.ramp_prev_sample = ch.last_mixed_sample;
            ch.ramp_remaining_frames = PlayerState::RAMP_FRAMES;
            if !ch.vib_wave.no_retrigger {
                ch.vib_pos = 0;
            }
            if !ch.trem_wave.no_retrigger {
                ch.trem_pos = 0;
            }
            ch.delay = None;
        }
    }

    // ECx: cut — zeros the volume at the specified tick.
    if effect == 0xE && x == 0xC && tick == y && y != 0 {
        ch.volume = 0;
    }

    // E9x: retrig every `retrig_ticks` ticks (non-zero).
    if effect == 0xE && x == 0x9 && ch.retrig_ticks != 0 && tick % ch.retrig_ticks == 0 {
        ch.sample_pos = 0.0;
        ch.active = true;
        // Per-trigger ramp on the E9x periodic retrigger.
        ch.ramp_prev_sample = ch.last_mixed_sample;
        ch.ramp_remaining_frames = PlayerState::RAMP_FRAMES;
    }

    match effect {
        0x0
            // Arpeggio: tick%3 cycles 0 / +x / +y semitones.
            if param != 0 => {
                let semis = match tick % 3 {
                    0 => 0,
                    1 => x as i32,
                    2 => y as i32,
                    _ => 0,
                };
                if semis == 0 {
                    ch.period = ch.arp_base_period;
                } else {
                    // Protracker expresses arpeggio via the period table:
                    // move `semis` semitones up within the current finetune
                    // row (semitone index + semis). Fallback to the
                    // equal-temperament approximation when we can't find
                    // the base note in the table.
                    let ft_row = finetune_row(ch.finetune);
                    let mut matched = None;
                    for (i, &p) in PERIOD_TABLE[ft_row].iter().enumerate() {
                        if p == ch.arp_base_period {
                            matched = Some(i);
                            break;
                        }
                    }
                    if let Some(base_idx) = matched {
                        let target = (base_idx as i32 + semis).clamp(0, 35) as usize;
                        ch.period = PERIOD_TABLE[ft_row][target];
                    } else {
                        let factor = 2.0f32.powf(semis as f32 / 12.0);
                        let p = (ch.arp_base_period as f32 / factor) as u16;
                        ch.period = p.max(PERIOD_MIN);
                    }
                }
            }
        0x1 => {
            let used = if param == 0 { ch.mem_porta_up } else { param };
            ch.period = ch.period.saturating_sub(used as u16).max(PERIOD_MIN);
        }
        0x2 => {
            let used = if param == 0 { ch.mem_porta_down } else { param };
            ch.period = (ch.period + used as u16).min(PERIOD_MAX);
        }
        0x3 => {
            // Tone portamento: slide toward target at stored speed.
            tone_porta_step(ch);
        }
        0x4 => {
            // Vibrato: advance LFO position; period stays un-modulated,
            // mixing picks up the delta via `vibrato_offset`.
            let rate = ch.mem_vibrato >> 4;
            advance_lfo(&mut ch.vib_pos, rate);
        }
        0x5 => {
            // Tone portamento + volume slide: both at once.
            tone_porta_step(ch);
            volume_slide_step(ch, ch.mem_volslide);
        }
        0x6 => {
            // Vibrato + volume slide.
            let rate = ch.mem_vibrato >> 4;
            advance_lfo(&mut ch.vib_pos, rate);
            volume_slide_step(ch, ch.mem_volslide);
        }
        0x7 => {
            // Tremolo: advance LFO position.
            let rate = ch.mem_tremolo >> 4;
            advance_lfo(&mut ch.trem_pos, rate);
        }
        0xA => {
            let slide = if param == 0 { ch.mem_volslide } else { param };
            volume_slide_step(ch, slide);
        }
        _ => {}
    }
}

/// Slide `period` toward `tone_porta_target` by `tone_porta_speed` units
/// per tick. Clamps on crossing the target. If glissando is enabled,
/// additionally snap to the nearest note in the current finetune row.
fn tone_porta_step(ch: &mut Channel) {
    if ch.tone_porta_target == 0 || ch.tone_porta_speed == 0 {
        return;
    }
    let target = ch.tone_porta_target;
    let step = ch.tone_porta_speed as i32;
    let cur = ch.period as i32;
    let new = if cur < target as i32 {
        (cur + step).min(target as i32)
    } else if cur > target as i32 {
        (cur - step).max(target as i32)
    } else {
        cur
    };
    // Tone porta clamps only to the extended period range
    // `[108, 907]`. Tightening to `[113, 856]` would break finetune
    // extremes — e.g. a slide whose target is FT +7 B-3 (period 108)
    // would otherwise clamp short at 113 (`Protracker-effects-MODFIL12.txt`
    // §3.2 "Normal Min Period = 108 / Max = 907").
    ch.period = new.clamp(PERIOD_MIN_EXT as i32, PERIOD_MAX_EXT as i32) as u16;

    if ch.glissando {
        // Snap to the nearest note in the current finetune row.
        let ft_row = finetune_row(ch.finetune);
        let row = &PERIOD_TABLE[ft_row];
        let mut best = ch.period;
        let mut best_diff = i32::MAX;
        for &p in row.iter() {
            let d = (p as i32 - ch.period as i32).abs();
            if d < best_diff {
                best_diff = d;
                best = p;
            }
        }
        ch.period = best;
    }
}

/// Apply one tick of an `Axy`-style volume slide. `slide` is the nibble
/// pair `x<<4 | y`: x raises, y lowers. If both are non-zero, up wins
/// (Protracker behaviour).
fn volume_slide_step(ch: &mut Channel, slide: u8) {
    let x = slide >> 4;
    let y = slide & 0x0F;
    if x != 0 {
        ch.volume = (ch.volume as u16 + x as u16).min(64) as u8;
    } else if y != 0 {
        ch.volume = ch.volume.saturating_sub(y);
    }
}

/// Compute the (left, right) gain pair for a channel with FT-extension
/// 8-bit pan `p` (0 = hard LEFT, 128 = centre, 255 = hard RIGHT) under
/// the player's global `pan_separation` `s ∈ [0, 1]` (1 = full Amiga
/// hard pan, 0 = collapse to mono).
///
/// Construction:
/// - Map `p` to a unit position `u = p / 255` in `[0, 1]`.
/// - Centre and scale to `[-1, 1]`: `c = 2*u - 1`.
/// - Scale by separation `s`: `c' = c * s`.
/// - Equal-power-by-cancellation linear pan:
///   `left  = (1 - c') / 2`
///   `right = (1 + c') / 2`
///
/// At endpoints (`p = 0`, `s = 1` → c' = -1) this collapses to
/// `(1, 0)`, identical to the pre-r75 `channel_is_left` hard-pan
/// formula via `near = (1 + s) / 2 = 1, far = (1 - s) / 2 = 0`. At
/// the centre (`p = 128`, c' ≈ 0) the channel contributes `(0.5,
/// 0.5)` regardless of `s`, which is the desired behaviour for a
/// centred channel under a global separation narrow. With `p = 0`
/// and `s = 0.5` we get `(0.75, 0.25)`, matching the prior
/// `near/far = ((1+s)/2, (1-s)/2)` split.
pub fn pan_gains(p: u8, s: f32) -> (f32, f32) {
    let u = p as f32 / 255.0;
    let c = 2.0 * u - 1.0;
    let c_eff = c * s.clamp(0.0, 1.0);
    let left = (1.0 - c_eff) * 0.5;
    let right = (1.0 + c_eff) * 0.5;
    (left, right)
}

/// Advance a vibrato / tremolo LFO position register per the Protracker
/// wraparound rule: add `rate`, and when `pos > 31` subtract 64 so the
/// signed position stays in `-32..=31`.
fn advance_lfo(pos: &mut i8, rate: u8) {
    let next = *pos as i32 + rate as i32;
    if next > 31 {
        *pos = (next - 64) as i8;
    } else {
        *pos = next as i8;
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::header::parse_header;
    use crate::samples::extract_samples;

    /// Build a tiny synthetic 4-channel M.K. MOD with one square-wave
    /// sample and one pattern that triggers notes on channel 0 across
    /// the first 4 rows.
    pub fn synth_square_mod() -> Vec<u8> {
        let mut out = vec![0u8; crate::header::HEADER_FIXED_SIZE];
        out[0..4].copy_from_slice(b"test");
        // Sample 1: 32 samples, length-in-words = 16.
        out[20 + 22..20 + 24].copy_from_slice(&16u16.to_be_bytes());
        // Finetune 0, volume 64.
        out[20 + 24] = 0;
        out[20 + 25] = 64;
        // Loop points: start 0, length 16 words (= 32 samples) — loops full.
        out[20 + 26..20 + 28].copy_from_slice(&0u16.to_be_bytes());
        out[20 + 28..20 + 30].copy_from_slice(&16u16.to_be_bytes());
        // Song length 1, order[0] = 0.
        out[950] = 1;
        out[951] = 0x7F;
        out[952] = 0;
        // Signature.
        out[1080..1084].copy_from_slice(b"M.K.");
        // Pattern 0: 64 rows × 4 channels × 4 bytes = 1024 bytes.
        let mut pat = vec![0u8; 64 * 4 * 4];
        // Rows 0,16,32,48 — trigger sample 1 on channel 0 with
        // descending periods (higher pitch first). Pick periods C-2, D-2,
        // E-2, F-2 — classic PT values: 428, 381, 339, 320.
        let rows_and_periods = [(0, 428u16), (16, 381), (32, 339), (48, 320)];
        for &(row, period) in &rows_and_periods {
            let off = row * 4 * 4;
            // Sample high nibble (sample = 1, high = 0, low = 1).
            // Byte 0 = (sample_hi << 4) | period_hi.
            let p_hi = ((period >> 8) & 0x0F) as u8;
            let p_lo = (period & 0xFF) as u8;
            let sample_hi = 0u8; // high nibble of sample index 1
            let sample_lo = 1u8;
            pat[off] = (sample_hi << 4) | p_hi;
            pat[off + 1] = p_lo;
            pat[off + 2] = sample_lo << 4; // effect 0
            pat[off + 3] = 0; // param
        }
        out.extend(pat);
        // Sample body: 32-sample square wave (16 hi, 16 lo).
        for i in 0..32 {
            let v: i8 = if i < 16 { 100 } else { -100 };
            out.push(v as u8);
        }
        out
    }

    /// Build a MOD with a caller-provided pattern. Sample 1 is a looping
    /// 32-byte square wave, volume 64, finetune 0. Only the first row's
    /// first channel is meaningful; other rows/channels default to empty.
    ///
    /// `pattern_rows` is a slice of `(row, channel, Note)` entries; each
    /// entry writes its Note into that cell of pattern 0.
    pub fn synth_mod_with_pattern(rows: &[(usize, usize, Note)]) -> Vec<u8> {
        let mut out = vec![0u8; crate::header::HEADER_FIXED_SIZE];
        out[0..4].copy_from_slice(b"test");
        out[20 + 22..20 + 24].copy_from_slice(&16u16.to_be_bytes());
        out[20 + 24] = 0;
        out[20 + 25] = 64;
        out[20 + 26..20 + 28].copy_from_slice(&0u16.to_be_bytes());
        out[20 + 28..20 + 30].copy_from_slice(&16u16.to_be_bytes());
        out[950] = 1;
        out[951] = 0x7F;
        out[952] = 0;
        out[1080..1084].copy_from_slice(b"M.K.");

        let mut pat = vec![0u8; 64 * 4 * 4];
        for &(row, channel, ref note) in rows {
            let off = row * 4 * 4 + channel * 4;
            let p_hi = ((note.period >> 8) & 0x0F) as u8;
            let p_lo = (note.period & 0xFF) as u8;
            let sample_hi = (note.sample & 0xF0) >> 4;
            let sample_lo = note.sample & 0x0F;
            pat[off] = (sample_hi << 4) | p_hi;
            pat[off + 1] = p_lo;
            pat[off + 2] = (sample_lo << 4) | note.effect;
            pat[off + 3] = note.effect_param;
        }
        out.extend(pat);

        for i in 0..32 {
            let v: i8 = if i < 16 { 100 } else { -100 };
            out.push(v as u8);
        }
        out
    }

    fn make_player(bytes: &[u8]) -> PlayerState {
        let header = parse_header(bytes).unwrap();
        let samples = extract_samples(&header, bytes);
        let patterns = parse_patterns(&header, bytes);
        PlayerState::new(&header, samples, patterns, 44_100)
    }

    #[test]
    fn decodes_patterns() {
        let bytes = synth_square_mod();
        let header = parse_header(&bytes).unwrap();
        let patterns = parse_patterns(&header, &bytes);
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].rows.len(), 64);
        assert_eq!(patterns[0].rows[0].len(), 4);
        let n = patterns[0].rows[0][0];
        assert_eq!(n.period, 428);
        assert_eq!(n.sample, 1);
    }

    #[test]
    fn player_renders_nonzero_audio() {
        let bytes = synth_square_mod();
        let mut player = make_player(&bytes);

        // Render ~0.1 s (4410 frames × 2 channels = 8820 samples).
        let mut buf = vec![0i16; 4410 * 2];
        let produced = player.render(&mut buf);
        assert_eq!(produced, 4410);

        // Must have at least some non-zero samples.
        let nonzero = buf.iter().filter(|&&x| x != 0).count();
        assert!(
            nonzero > 100,
            "expected non-silent PCM, got {nonzero} non-zero samples"
        );
    }

    #[test]
    fn samples_per_tick_default() {
        let bytes = synth_square_mod();
        let player = make_player(&bytes);
        assert_eq!(player.samples_per_tick(), 882);
    }

    #[test]
    fn render_per_channel_isolates_channels() {
        // The synth MOD triggers notes exclusively on channel 0, so any
        // per-channel stream other than 0 must be pure silence.
        let bytes = synth_square_mod();
        let mut player = make_player(&bytes);

        let n_frames = 4410;
        let mut planes: Vec<Vec<i16>> = (0..player.channels.len())
            .map(|_| vec![0i16; n_frames])
            .collect();
        let produced = {
            let mut views: Vec<&mut [i16]> = planes.iter_mut().map(|v| v.as_mut_slice()).collect();
            player.render_per_channel(&mut views, n_frames)
        };
        assert_eq!(produced, n_frames);

        let ch0_nonzero = planes[0].iter().filter(|&&s| s != 0).count();
        assert!(
            ch0_nonzero > 100,
            "channel 0 should carry audible signal, got {ch0_nonzero} non-zero samples"
        );
        for (i, plane) in planes.iter().enumerate().skip(1) {
            let nonzero = plane.iter().filter(|&&s| s != 0).count();
            assert_eq!(
                nonzero, 0,
                "channel {i} should be silent in synth_square_mod, got {nonzero} non-zero samples"
            );
        }
    }

    #[test]
    fn render_per_channel_matches_mixed_song_length() {
        let bytes = synth_square_mod();
        let mut player_mixed = make_player(&bytes);
        let mut player_planar = make_player(&bytes);

        let n_frames = 2205;
        let mut mixed = vec![0i16; n_frames * 2];
        let produced_mixed = player_mixed.render(&mut mixed);

        let mut planes: Vec<Vec<i16>> = (0..player_planar.channels.len())
            .map(|_| vec![0i16; n_frames])
            .collect();
        let produced_planar = {
            let mut views: Vec<&mut [i16]> = planes.iter_mut().map(|v| v.as_mut_slice()).collect();
            player_planar.render_per_channel(&mut views, n_frames)
        };

        assert_eq!(produced_mixed, n_frames);
        assert_eq!(produced_planar, n_frames);
    }

    // ---------- Spec-driven effect tests ----------

    #[test]
    fn period_table_cross_check_against_spec() {
        // Protracker-v1.1B-mod.txt: "Periodtable for Tuning 0, Normal".
        // C-1 = 856, B-1 = 453, C-2 = 428, A-3 = 127, B-3 = 113.
        let ft0 = &PERIOD_TABLE[0];
        assert_eq!(ft0[0], 856, "C-1 @ ft 0");
        assert_eq!(ft0[11], 453, "B-1 @ ft 0");
        assert_eq!(ft0[12], 428, "C-2 @ ft 0");
        assert_eq!(ft0[33], 127, "A-3 @ ft 0");
        assert_eq!(ft0[35], 113, "B-3 @ ft 0");
        // Finetune +1, C-2 = 425; finetune -1 (row 15), C-2 = 431.
        assert_eq!(PERIOD_TABLE[1][12], 425, "C-2 @ ft +1");
        assert_eq!(PERIOD_TABLE[15][12], 431, "C-2 @ ft -1");
    }

    #[test]
    fn sine_table_matches_protracker_half_wave() {
        // Spot-check: the 32-entry half-wave starts at 0, peaks at 255 at
        // index 16, dips back down to 24 at 31. These are spec values
        // from FireLight §5.5.
        assert_eq!(PROTRACKER_SINE_TABLE[0], 0);
        assert_eq!(PROTRACKER_SINE_TABLE[8], 180);
        assert_eq!(PROTRACKER_SINE_TABLE[16], 255);
        assert_eq!(PROTRACKER_SINE_TABLE[24], 180);
        assert_eq!(PROTRACKER_SINE_TABLE[31], 24);
    }

    /// Step the player forward one full tick. Uses a scratch stereo
    /// buffer of exactly one tick's frames.
    fn step_one_tick(player: &mut PlayerState) {
        let spt = player.samples_per_tick() as usize;
        let mut buf = vec![0i16; spt * 2];
        player.render(&mut buf);
    }

    #[test]
    fn tone_porta_reaches_target_period_exactly() {
        // Row 0: C-2 (period 428), no effect.
        // Row 1: A-2 (period 254), tone-porta with speed $10 = 16.
        //   Initial period 428, target 254, diff 174. After ⌈174/16⌉ = 11
        //   tick-N steps the period should equal 254. Default speed is 6,
        //   meaning each row has 5 non-tick-0 updates; so we need 3 rows
        //   (15 steps) of sustained tone-porta to finish the slide.
        let bytes = synth_mod_with_pattern(&[
            (
                0,
                0,
                Note {
                    period: 428,
                    sample: 1,
                    effect: 0,
                    effect_param: 0,
                },
            ),
            (
                1,
                0,
                Note {
                    period: 254,
                    sample: 0,
                    effect: 0x3,
                    effect_param: 0x10,
                },
            ),
            (
                2,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0x3,
                    effect_param: 0x00,
                },
            ),
            (
                3,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0x3,
                    effect_param: 0x00,
                },
            ),
        ]);
        let mut player = make_player(&bytes);
        // Walk 4 rows * 6 ticks = 24 ticks worth.
        for _ in 0..24 {
            step_one_tick(&mut player);
        }
        assert_eq!(
            player.channels[0].period, 254,
            "tone porta must clamp at target"
        );
    }

    #[test]
    fn vibrato_modulates_period_symmetrically() {
        // Trigger a note, then apply 4xy with depth 4 and rate 8. The LFO
        // should visit both positive and negative sides of the base period
        // over a single row's worth of ticks.
        let bytes = synth_mod_with_pattern(&[
            (
                0,
                0,
                Note {
                    period: 428,
                    sample: 1,
                    effect: 0x4,
                    effect_param: 0x84,
                },
            ),
            (
                1,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0x4,
                    effect_param: 0x00,
                },
            ),
        ]);
        let mut player = make_player(&bytes);

        let mut max_delta = 0i32;
        let mut min_delta = 0i32;
        // 2 rows × 6 ticks = 12 samples of the offset.
        for _ in 0..12 {
            step_one_tick(&mut player);
            let off = PlayerState::vibrato_offset(&player.channels[0]) as i32;
            max_delta = max_delta.max(off);
            min_delta = min_delta.min(off);
        }
        // Depth 4: peak sine*depth/128 = 255*4/128 = 7.9 ≈ 7.
        assert!(
            max_delta >= 4,
            "expected positive vibrato swing, got {max_delta}"
        );
        assert!(
            min_delta <= -4,
            "expected negative vibrato swing, got {min_delta}"
        );
    }

    #[test]
    fn tremolo_modulates_volume_symmetrically() {
        let bytes = synth_mod_with_pattern(&[
            // Cxx 20 — set volume to 32 so tremolo has headroom both sides.
            (
                0,
                0,
                Note {
                    period: 428,
                    sample: 1,
                    effect: 0xC,
                    effect_param: 0x20,
                },
            ),
            // 7xy with rate 8 depth 4.
            (
                1,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0x7,
                    effect_param: 0x84,
                },
            ),
            (
                2,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0x7,
                    effect_param: 0x00,
                },
            ),
        ]);
        let mut player = make_player(&bytes);
        let mut max_delta = 0i32;
        let mut min_delta = 0i32;
        for _ in 0..18 {
            step_one_tick(&mut player);
            let off = PlayerState::tremolo_offset(&player.channels[0]) as i32;
            max_delta = max_delta.max(off);
            min_delta = min_delta.min(off);
        }
        // Depth 4: peak = 255*4/64 = 15.94.
        assert!(
            max_delta >= 8,
            "expected positive tremolo swing, got {max_delta}"
        );
        assert!(
            min_delta <= -8,
            "expected negative tremolo swing, got {min_delta}"
        );
    }

    #[test]
    fn sample_offset_advances_into_sample() {
        // 9xx with param 0x01 -> offset = 0x0100 = 256 frames. We construct
        // a MOD with a 512-frame-long sample so the offset lands cleanly
        // inside the body. After the first tick the channel's sample_pos
        // should be at 256 plus the mixer-advance (a handful of samples).
        let mut bytes = synth_mod_with_pattern(&[(
            0,
            0,
            Note {
                period: 428,
                sample: 1,
                effect: 0x9,
                effect_param: 0x01,
            },
        )]);
        // Patch sample 1 length to 256 words (512 frames).
        bytes[20 + 22..20 + 24].copy_from_slice(&256u16.to_be_bytes());
        // Disable the loop (repeat length 0) so the 9xx offset does not
        // immediately wrap into the loop region — see the loop-boundary
        // PT quirk fix in `mix_one`.
        bytes[20 + 28..20 + 30].copy_from_slice(&0u16.to_be_bytes());
        // Append 480 more bytes (512 total) to the sample body at the tail.
        bytes.extend(std::iter::repeat_n(0u8, 480));

        let mut player = make_player(&bytes);
        step_one_tick(&mut player);
        assert_eq!(
            player.channels[0].mem_sample_offset, 0x01,
            "9xx memory not latched"
        );
        // sample_pos should have started at 256 (0x01 * 0x100) plus
        // whatever the mixer advanced during the tick.
        assert!(
            player.channels[0].sample_pos >= 256.0,
            "expected sample_pos >= 256, got {}",
            player.channels[0].sample_pos
        );
    }

    #[test]
    fn sample_offset_out_of_range_plays_no_note() {
        // ProTracker quirk (Protracker-effects-MODFIL12.txt 9:Set-sample-
        // offset, lines 1240-1242): if a 9xx offset lands at or past the
        // end of the sample, NO NOTE is played at all. The synth helper
        // builds a 32-frame sample; 901 → offset 0x100 = 256 frames, far
        // beyond the end, so the channel must stay inactive and emit
        // silence rather than wrapping a looped sample back into its loop.
        let bytes = synth_mod_with_pattern(&[(
            0,
            0,
            Note {
                period: 428,
                sample: 1,
                effect: 0x9,
                effect_param: 0x01,
            },
        )]);
        let mut player = make_player(&bytes);
        // The 9xx memory is still latched even though no note plays
        // (the note info is updated; only playback is suppressed).
        step_one_tick(&mut player);
        assert_eq!(
            player.channels[0].mem_sample_offset, 0x01,
            "9xx memory should still latch even when the offset is OOR"
        );
        assert!(
            !player.channels[0].active,
            "out-of-range 9xx must leave the channel inactive (no note)"
        );
        assert_eq!(
            player.channels[0].sample_pos, 0.0,
            "out-of-range 9xx must not advance the sample cursor"
        );

        // Render a chunk and confirm the channel produced silence (no
        // wrapped-loop artefact). A fresh player so we re-enter row 0.
        let mut player = make_player(&bytes);
        let mut buf = vec![0i16; 2048 * 2];
        player.render(&mut buf);
        assert!(
            buf.iter().all(|&s| s == 0),
            "out-of-range 9xx must render silence, found non-zero PCM"
        );
    }

    #[test]
    fn sample_offset_at_exact_end_plays_no_note() {
        // Boundary: an offset landing EXACTLY at the sample length is
        // still "at or past the end" per the spec wording, so it must
        // also suppress the note. Build a 256-frame sample and request
        // 901 (offset = 256 = the length).
        let mut bytes = synth_mod_with_pattern(&[(
            0,
            0,
            Note {
                period: 428,
                sample: 1,
                effect: 0x9,
                effect_param: 0x01,
            },
        )]);
        // Sample length 128 words = 256 frames; disable loop so the body
        // is a plain one-shot.
        bytes[20 + 22..20 + 24].copy_from_slice(&128u16.to_be_bytes());
        bytes[20 + 28..20 + 30].copy_from_slice(&0u16.to_be_bytes());
        // Append the extra body bytes (synth helper writes 32; need 256).
        bytes.extend(std::iter::repeat_n(0u8, 256 - 32));

        let mut player = make_player(&bytes);
        step_one_tick(&mut player);
        assert!(
            !player.channels[0].active,
            "9xx offset == sample length must suppress the note (>= end)"
        );
    }

    #[test]
    fn sample_offset_just_inside_end_plays() {
        // Contrast: an offset comfortably inside the sample DOES play.
        // Build a 512-frame one-shot so 901 (offset 256) seeks to the
        // middle and the note survives a full tick of playback. This
        // pins the boundary direction of the OOR check (only >= end
        // suppresses; in-range still triggers).
        let mut bytes = synth_mod_with_pattern(&[(
            0,
            0,
            Note {
                period: 428,
                sample: 1,
                effect: 0x9,
                effect_param: 0x01,
            },
        )]);
        // Sample length 256 words = 512 frames; disable loop.
        bytes[20 + 22..20 + 24].copy_from_slice(&256u16.to_be_bytes());
        bytes[20 + 28..20 + 30].copy_from_slice(&0u16.to_be_bytes());
        bytes.extend(std::iter::repeat_n(7u8, 512 - 32));

        let mut player = make_player(&bytes);
        step_one_tick(&mut player);
        assert!(
            player.channels[0].active,
            "9xx offset inside the sample must still play the note"
        );
        assert!(
            player.channels[0].sample_pos >= 256.0,
            "in-range 9xx should seek to >= 256, got {}",
            player.channels[0].sample_pos
        );
    }

    #[test]
    fn fine_porta_up_and_down_shift_period_once() {
        let bytes = synth_mod_with_pattern(&[
            (
                0,
                0,
                Note {
                    period: 428,
                    sample: 1,
                    effect: 0,
                    effect_param: 0,
                },
            ),
            // E12: fine porta up by 2.
            (
                1,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0xE,
                    effect_param: 0x12,
                },
            ),
            // E23: fine porta down by 3.
            (
                2,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0xE,
                    effect_param: 0x23,
                },
            ),
        ]);
        let mut player = make_player(&bytes);

        step_one_tick(&mut player); // row 0, tick 0
        assert_eq!(player.channels[0].period, 428);
        // Finish row 0 so we reach row 1's tick 0.
        for _ in 0..5 {
            step_one_tick(&mut player);
        }
        step_one_tick(&mut player); // row 1, tick 0 — E12 fires.
        assert_eq!(player.channels[0].period, 426, "E12 must slide up by 2");
        for _ in 0..5 {
            step_one_tick(&mut player);
        }
        step_one_tick(&mut player); // row 2, tick 0 — E23 fires.
        assert_eq!(
            player.channels[0].period, 429,
            "E23 must slide down by 3 from 426"
        );
    }

    #[test]
    fn pattern_loop_e6_loops_then_advances() {
        // Row 0: trigger note. Row 1: E60 (set loop start). Row 2: E62
        // (loop back to row 1 twice). After 2 extra rounds, the player
        // must advance to row 3.
        //
        // Per spec (FireLight §5.22): E6 param != 0 on first visit sets
        // loop_count = param and jumps back; subsequent visits decrement
        // until zero and advance. So with param=2: visit1 sets counter=2
        // jumps back, visit2 counter=1 jumps back, visit3 counter=0
        // advances. We should traverse rows 1→2→1→2→1→2→3.
        let bytes = synth_mod_with_pattern(&[
            (
                0,
                0,
                Note {
                    period: 428,
                    sample: 1,
                    effect: 0,
                    effect_param: 0,
                },
            ),
            (
                1,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0xE,
                    effect_param: 0x60,
                },
            ),
            (
                2,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0xE,
                    effect_param: 0x62,
                },
            ),
            (
                3,
                0,
                Note {
                    period: 339,
                    sample: 1,
                    effect: 0,
                    effect_param: 0,
                },
            ),
        ]);
        let mut player = make_player(&bytes);

        // Visited rows in order so we can inspect. Render one tick per
        // step and snapshot the current row number.
        let mut visited: Vec<u8> = Vec::new();
        for _ in 0..60 {
            step_one_tick(&mut player);
            if let Some(&last) = visited.last() {
                if last != player.row {
                    visited.push(player.row);
                }
            } else {
                visited.push(player.row);
            }
            if player.row > 3 || player.ended {
                break;
            }
        }

        // Expected sequence: 0,1,2,1,2,1,2,3 (with possible trailing rows)
        let prefix = &visited[..visited.len().min(8)];
        assert_eq!(
            prefix,
            &[0, 1, 2, 1, 2, 1, 2, 3][..prefix.len()],
            "E62 should loop rows 1..=2 twice before advancing; got {visited:?}"
        );
    }

    #[test]
    fn retrig_e9_restarts_sample_cursor() {
        // E91: retrig on every tick. Without retrig the sample_pos would
        // be cumulatively advanced across ticks; with retrig it resets on
        // every tick boundary, so the post-tick-2 position stays
        // equal to the post-tick-1 position (both are exactly one tick's
        // worth of advance starting from 0).
        let bytes = synth_mod_with_pattern(&[(
            0,
            0,
            Note {
                period: 428,
                sample: 1,
                effect: 0xE,
                effect_param: 0x91,
            },
        )]);
        let mut player = make_player(&bytes);
        // Tick 0: note triggers at pos 0, advances for ~882 samples.
        step_one_tick(&mut player);
        let pos_after_t0 = player.channels[0].sample_pos;
        // Tick 1: E91 resets pos to 0 then advances — should equal t0 value.
        step_one_tick(&mut player);
        let pos_after_t1 = player.channels[0].sample_pos;
        // Tick 2: same behaviour.
        step_one_tick(&mut player);
        let pos_after_t2 = player.channels[0].sample_pos;
        assert!(
            (pos_after_t1 - pos_after_t0).abs() < 1.0,
            "E91 should retrigger; pos_after_t0={pos_after_t0}, pos_after_t1={pos_after_t1}"
        );
        assert!(
            (pos_after_t2 - pos_after_t1).abs() < 1.0,
            "E91 should retrigger again; pos_after_t1={pos_after_t1}, pos_after_t2={pos_after_t2}"
        );
    }

    #[test]
    fn note_cut_ec_zeros_volume_at_tick() {
        // Cxx 40, EC3: cut at tick 3.
        let bytes = synth_mod_with_pattern(&[(
            0,
            0,
            Note {
                period: 428,
                sample: 1,
                effect: 0xE,
                effect_param: 0xC3,
            },
        )]);
        let mut player = make_player(&bytes);
        // Tick 0: volume loaded (64 from sample), note triggered.
        step_one_tick(&mut player);
        assert_eq!(player.channels[0].volume, 64);
        step_one_tick(&mut player); // tick 1
        step_one_tick(&mut player); // tick 2
        step_one_tick(&mut player); // tick 3: EC3 fires.
        assert_eq!(player.channels[0].volume, 0, "EC3 must cut volume at t=3");
    }

    #[test]
    fn note_delay_ed_postpones_trigger() {
        // ED3 with a fresh note. The sample should only start at tick 3.
        let bytes = synth_mod_with_pattern(&[(
            0,
            0,
            Note {
                period: 428,
                sample: 1,
                effect: 0xE,
                effect_param: 0xD3,
            },
        )]);
        let mut player = make_player(&bytes);
        // Tick 0 shouldn't trigger — channel remains inactive (no prior note).
        step_one_tick(&mut player);
        assert!(!player.channels[0].active, "ED3 must not trigger on tick 0");
        step_one_tick(&mut player);
        step_one_tick(&mut player);
        // Tick 3: note fires.
        step_one_tick(&mut player);
        assert!(
            player.channels[0].active,
            "ED3 must trigger at tick 3; state={:?}",
            player.channels[0]
        );
        assert_eq!(player.channels[0].period, 428);
    }

    #[test]
    fn fine_volume_slide_ea_eb_shifts_volume_once() {
        let bytes = synth_mod_with_pattern(&[
            (
                0,
                0,
                Note {
                    period: 428,
                    sample: 1,
                    effect: 0xC,
                    effect_param: 0x20,
                },
            ),
            (
                1,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0xE,
                    effect_param: 0xA3,
                },
            ),
            (
                2,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0xE,
                    effect_param: 0xB5,
                },
            ),
        ]);
        let mut player = make_player(&bytes);
        // Row 0.
        for _ in 0..6 {
            step_one_tick(&mut player);
        }
        // After row 0 complete → row 1 tick 0: EA3 fires; volume 32 + 3 = 35.
        step_one_tick(&mut player);
        assert_eq!(player.channels[0].volume, 0x23);
        for _ in 0..5 {
            step_one_tick(&mut player);
        }
        // Row 2 tick 0: EB5 fires; 35 - 5 = 30.
        step_one_tick(&mut player);
        assert_eq!(player.channels[0].volume, 0x1E);
    }

    #[test]
    fn e5_finetune_applies_on_note_row() {
        // Row 0: play C-2 (period 428) with E50 — finetune 0.
        // Row 1: play C-2 with E51 — finetune +1. Under finetune +1 the
        // C-2 period is 425, so the channel's period should be 425 at tick 0.
        let bytes = synth_mod_with_pattern(&[
            (
                0,
                0,
                Note {
                    period: 428,
                    sample: 1,
                    effect: 0xE,
                    effect_param: 0x50,
                },
            ),
            (
                1,
                0,
                Note {
                    period: 428,
                    sample: 1,
                    effect: 0xE,
                    effect_param: 0x51,
                },
            ),
        ]);
        let mut player = make_player(&bytes);
        step_one_tick(&mut player);
        assert_eq!(player.channels[0].period, 428);
        assert_eq!(player.channels[0].finetune, 0);
        for _ in 0..5 {
            step_one_tick(&mut player);
        }
        step_one_tick(&mut player);
        assert_eq!(player.channels[0].finetune, 1);
        assert_eq!(
            player.channels[0].period, 425,
            "finetune +1 should retune C-2 to 425"
        );
    }

    #[test]
    fn pattern_delay_ee_repeats_row_without_retriggering_effects() {
        // Per `Pro-Noise-Soundtracker-rev4.txt` §[14][14] ("Delay
        // pattern"): "all effects and previous notes continue during
        // delay". The row's tick-0 effects (e.g. EAx fine vol slide,
        // Cxx set volume, note triggers) must NOT re-fire on the
        // repeated passes — only per-tick effects (vol slides, vibrato,
        // tone porta) keep ticking. Without this guarantee a held note
        // on the row would re-trigger from sample_pos=0 on every repeat
        // and EAx would compound, both audible regressions on real-
        // world MODs that use EE for held-note textures.
        let bytes = synth_mod_with_pattern(&[
            (
                0,
                0,
                Note {
                    period: 428,
                    sample: 1,
                    effect: 0xC,
                    effect_param: 0x00,
                },
            ),
            // Row 1: EE1 pattern delay + EA2 fine vol slide up by 2.
            // These are *separate channels* in PT, so put the EE1 on ch1.
            (
                1,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0xE,
                    effect_param: 0xA2,
                },
            ),
            (
                1,
                1,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0xE,
                    effect_param: 0xE1,
                },
            ),
        ]);
        let mut player = make_player(&bytes);
        // Walk row 0 (6 ticks).
        for _ in 0..6 {
            step_one_tick(&mut player);
        }
        // Row 1 tick 0: EA2 increments volume from 0 → 2. EE1 sets delay=1.
        step_one_tick(&mut player);
        assert_eq!(player.channels[0].volume, 2);
        // Finish row 1 (5 more ticks) — pattern delay counter ticks down.
        for _ in 0..5 {
            step_one_tick(&mut player);
        }
        // Row "1 again" tick 0: per spec, EA2 must NOT re-fire — volume
        // stays at 2, not 4.
        step_one_tick(&mut player);
        assert_eq!(
            player.channels[0].volume, 2,
            "EEx pattern-delay repeat must not re-fire EA2 (volume must stay 2)"
        );
    }

    #[test]
    fn pattern_delay_ee_does_not_retrigger_held_note() {
        // Trigger a long, looping sample on row 0, then on row 1 emit
        // EE2 (delay 2 row passes) with no note. Across the two delay
        // repeats, the channel's `sample_pos` must keep advancing
        // monotonically (the previous note "continues during delay" per
        // spec). If `enter_row` were re-invoked it would not reset the
        // sample (no note in row 1) but the tick-0 effect machinery
        // would fire again — for the bug we're really chasing, drop a
        // note onto row 1 and confirm it does NOT re-trigger across the
        // repeats either.
        let bytes = synth_mod_with_pattern(&[
            // Row 0: trigger.
            (
                0,
                0,
                Note {
                    period: 428,
                    sample: 1,
                    effect: 0,
                    effect_param: 0,
                },
            ),
            // Row 1: a note + EE3. The note must trigger on the first
            // pass (and only the first pass).
            (
                1,
                0,
                Note {
                    period: 339,
                    sample: 1,
                    effect: 0xE,
                    effect_param: 0xE3,
                },
            ),
        ]);
        let mut player = make_player(&bytes);

        // Walk row 0 (6 ticks).
        for _ in 0..6 {
            step_one_tick(&mut player);
        }
        // Row 1 tick 0: note triggers (sample_pos resets to 0), EE3
        // sets pattern_delay = 3.
        step_one_tick(&mut player);
        assert_eq!(player.channels[0].period, 339, "row 1 note must trigger");
        let pos_after_first_trigger = player.channels[0].sample_pos;

        // Walk the rest of row 1's ticks plus all 3 delay repeats. Each
        // pass has 6 ticks; we've consumed 1 of the first pass, leaving
        // 5 + 3*6 = 23 ticks before the song advances out of row 1.
        let mut prev_pos = pos_after_first_trigger;
        for tick_idx in 0..23 {
            step_one_tick(&mut player);
            let cur_pos = player.channels[0].sample_pos;
            // Sample is a 32-frame loop; the position wraps inside the
            // loop region. We just need to confirm we never reset to 0
            // (which would happen if enter_row were re-invoked on the
            // repeats and re-triggered the note).
            // Allow position == 0 only on the very first iteration (none
            // here, so flag any zero immediately). Account for the loop
            // wrap by checking we don't drop to a value strictly less
            // than the loop_start (which is 0 — so the only invalid
            // state is sample_pos == 0.0 followed by another 0.0, i.e.
            // an enforced reset, not a wrap).
            assert!(
                cur_pos != 0.0 || prev_pos == 0.0,
                "tick {tick_idx}: sample_pos jumped back to 0 \
                 (prev={prev_pos}, cur={cur_pos}) — the EE pattern-delay \
                 repeat must NOT re-trigger a note that was already \
                 played on the first pass through the row"
            );
            prev_pos = cur_pos;
        }
    }

    /// Build a 1-channel-style synthetic MOD with a custom sample body so
    /// we can probe the mixer's loop-wrap behaviour. The sample is given a
    /// loop region from `loop_start` to `loop_start + loop_length`; the
    /// PCM tail past `loop_end` is filled with a sentinel value so test
    /// code can detect any read past the loop boundary.
    fn synth_mod_with_loop_sample(
        pcm: &[i8],
        loop_start_words: u16,
        loop_length_words: u16,
    ) -> Vec<u8> {
        let mut out = vec![0u8; crate::header::HEADER_FIXED_SIZE];
        out[0..4].copy_from_slice(b"loop");
        let length_words = (pcm.len() / 2) as u16;
        out[20 + 22..20 + 24].copy_from_slice(&length_words.to_be_bytes());
        out[20 + 24] = 0;
        out[20 + 25] = 64;
        out[20 + 26..20 + 28].copy_from_slice(&loop_start_words.to_be_bytes());
        out[20 + 28..20 + 30].copy_from_slice(&loop_length_words.to_be_bytes());
        out[950] = 1;
        out[951] = 0x7F;
        out[952] = 0;
        out[1080..1084].copy_from_slice(b"M.K.");

        let mut pat = vec![0u8; 64 * 4 * 4];
        // Row 0, ch 0: trigger sample 1 at C-2 (period 428).
        let p_hi = ((428u16 >> 8) & 0x0F) as u8;
        let p_lo = (428u16 & 0xFF) as u8;
        pat[0] = p_hi;
        pat[1] = p_lo;
        pat[2] = 1u8 << 4;
        pat[3] = 0;
        out.extend(pat);
        out.extend(pcm.iter().map(|&s| s as u8));
        out
    }

    #[test]
    fn loop_wrap_stays_inside_loop_region() {
        // Protracker-effects-MODFIL12.txt §2.2: looped samples play only
        // the loop_start..loop_start+loop_length region. The "tail" past
        // loop_end is decay data that PT discards. Before the fix in
        // mix_one, the mixer wrapped only when sample_pos >= pcm.len(),
        // producing audible glitches when loop_end < pcm.len(). This
        // test sets up loop_end = 64 and pcm.len() = 200, fills the tail
        // 64..200 with a distinct sentinel, and verifies that the mixer
        // never reads any sample whose magnitude matches the sentinel.
        //
        // Sample layout (200 bytes total, all i8):
        //   0..64   : value +50 (signal in the loop region)
        //   64..200 : value -100 (sentinel — must NEVER be read)
        // Loop: start=0, length=64.
        let mut pcm: Vec<i8> = vec![50; 64];
        pcm.extend(std::iter::repeat_n(-100i8, 136));
        // pcm.len() must be even (the header stores length in words).
        assert!(pcm.len().is_multiple_of(2));
        let bytes = synth_mod_with_loop_sample(&pcm, 0, 32);
        let mut player = make_player(&bytes);

        // Render ~0.05 seconds (2205 frames). At C-2 (period 428) the
        // playback rate is PAULA_CLOCK / 428 ≈ 8287 Hz, so this renders
        // ~414 sample frames of the source — well past the 64-frame loop
        // boundary, exercising several wrap cycles.
        let n_frames = 2205;
        let mut planes: Vec<Vec<i16>> = (0..player.channels.len())
            .map(|_| vec![0i16; n_frames])
            .collect();
        let _ = {
            let mut views: Vec<&mut [i16]> = planes.iter_mut().map(|v| v.as_mut_slice()).collect();
            player.render_per_channel(&mut views, n_frames)
        };

        // Channel 0 carries the signal. Each output frame is a linear
        // interpolation of two adjacent sample values; if both are inside
        // the loop region they sit around +50/128 ≈ +0.39 (i16 ≈ +12800).
        // If the mixer ever reads the sentinel (-100) we would see a
        // strongly negative sample. With the 2-pole always-on Amiga
        // output filter we need to (1) skip the first ~64 frames while
        // the filter accumulators ramp up to steady state, and (2)
        // accept any positive value — a sentinel leak would manifest
        // as a strongly NEGATIVE excursion (-100/128 → ≈-25600), so
        // `v >= 0` is the actual loop-correctness invariant.
        for (i, &v) in planes[0].iter().enumerate().skip(64) {
            assert!(
                v >= 0,
                "frame {i}: expected positive +50/128 sample, got {v} \
                 — mixer leaked past loop boundary into sentinel tail \
                 (negative value implies the -100 sentinel was read)"
            );
        }
    }

    #[test]
    fn loop_wrap_handles_loop_end_at_pcm_end() {
        // Sanity: the classic case where loop_end == pcm.len() must
        // continue to work. Sample is a 32-byte square wave looping for
        // its full length.
        let mut pcm: Vec<i8> = (0..16).map(|_| 100).collect();
        pcm.extend(std::iter::repeat_n(-100i8, 16));
        let bytes = synth_mod_with_loop_sample(&pcm, 0, 16);
        let mut player = make_player(&bytes);
        let mut buf = vec![0i16; 4410 * 2];
        let produced = player.render(&mut buf);
        assert_eq!(produced, 4410);
        let nonzero = buf.iter().filter(|&&x| x != 0).count();
        assert!(nonzero > 4000, "expected loud square wave output");
    }

    #[test]
    fn sample_swap_without_note_is_deferred() {
        // PT quirk (Protracker-effects-MODFIL12.txt §3.2 +
        // Pro-Noise-Soundtracker-rev4.txt:113-118): writing a sample
        // number on a row that has NO note must NOT swap the active
        // sample on the channel — the swap is deferred until the next
        // note-on. The row's volume + finetune are still updated.
        //
        // Setup: two samples in the file. Row 0 triggers sample 1 at
        // C-2. Row 1 writes "sample 2" with no note — the channel must
        // continue mixing sample 1 (the active one), with sample 2's
        // default volume now applied. Row 2 retriggers (no sample
        // number, just a note) — this must consume the pending swap and
        // start playing sample 2.
        let mut bytes = vec![0u8; crate::header::HEADER_FIXED_SIZE];
        bytes[0..4].copy_from_slice(b"swap");
        // Sample 1: 32 frames, finetune 0, volume 64, no loop.
        bytes[20 + 22..20 + 24].copy_from_slice(&16u16.to_be_bytes());
        bytes[20 + 24] = 0;
        bytes[20 + 25] = 64;
        bytes[20 + 26..20 + 28].copy_from_slice(&0u16.to_be_bytes());
        bytes[20 + 28..20 + 30].copy_from_slice(&0u16.to_be_bytes());
        // Sample 2: 32 frames, finetune +3, volume 32, no loop.
        bytes[50 + 22..50 + 24].copy_from_slice(&16u16.to_be_bytes());
        bytes[50 + 24] = 3;
        bytes[50 + 25] = 32;
        bytes[50 + 26..50 + 28].copy_from_slice(&0u16.to_be_bytes());
        bytes[50 + 28..50 + 30].copy_from_slice(&0u16.to_be_bytes());

        bytes[950] = 1;
        bytes[951] = 0x7F;
        bytes[952] = 0;
        bytes[1080..1084].copy_from_slice(b"M.K.");

        // Pattern.
        let mut pat = vec![0u8; 64 * 4 * 4];
        // Row 0, ch 0: C-2 (428) sample 1.
        let off = 0;
        pat[off] = ((428u16 >> 8) & 0x0F) as u8;
        pat[off + 1] = (428u16 & 0xFF) as u8;
        pat[off + 2] = 1u8 << 4;
        // Row 1, ch 0: sample 2, no period.
        let off = 4 * 4;
        pat[off] = 0; // no period high nibble, no sample high nibble
        pat[off + 1] = 0;
        pat[off + 2] = 2u8 << 4; // sample lo = 2, effect 0
        pat[off + 3] = 0;
        // Row 2, ch 0: D-2 (381), no sample number — should consume pending swap.
        let off = 2 * 4 * 4;
        pat[off] = ((381u16 >> 8) & 0x0F) as u8;
        pat[off + 1] = (381u16 & 0xFF) as u8;
        pat[off + 2] = 0; // no sample number
        pat[off + 3] = 0;
        bytes.extend(pat);

        // Sample 1 body: 32 bytes of +50.
        bytes.extend(std::iter::repeat_n(50i8 as u8, 32));
        // Sample 2 body: 32 bytes of -50.
        bytes.extend(std::iter::repeat_n((-50i8) as u8, 32));

        let mut player = make_player(&bytes);

        // After row 0: channel plays sample 1, volume 64.
        for _ in 0..6 {
            step_one_tick(&mut player);
        }
        assert_eq!(player.channels[0].sample_index, 1, "row 0: sample 1 active");
        assert_eq!(player.channels[0].volume, 64, "row 0: vol from sample 1");

        // Row 1: sample 2 written, no note. Active sample stays at 1,
        // but volume + finetune update to sample 2's defaults.
        for _ in 0..6 {
            step_one_tick(&mut player);
        }
        assert_eq!(
            player.channels[0].sample_index, 1,
            "row 1: sample 2 written without note — active sample MUST still be 1"
        );
        assert_eq!(
            player.channels[0].volume, 32,
            "row 1: sample 2's default volume must apply immediately"
        );
        assert_eq!(
            player.channels[0].finetune, 3,
            "row 1: sample 2's finetune must apply immediately"
        );
        assert_eq!(
            player.channels[0].pending_sample, 2,
            "row 1: pending sample swap should be queued"
        );

        // Row 2: D-2 with no sample number — consumes the pending swap.
        for _ in 0..6 {
            step_one_tick(&mut player);
        }
        assert_eq!(
            player.channels[0].sample_index, 2,
            "row 2: pending sample 2 swap must be consumed by note trigger"
        );
        assert_eq!(
            player.channels[0].pending_sample, 0,
            "row 2: pending_sample must clear on consumption"
        );
    }

    // ---------- Round-15 PT-fidelity regression tests ----------

    #[test]
    fn period_clamp_constants_match_pt_spec() {
        // Cross-check the four published period limits.
        // Standard porta range: B-3 = 113, C-1 = 856 (Protracker-v1.1B-mod.txt
        // Cmd 1/2). Extended range covering finetune ±8: 108..907 per
        // Protracker-effects-MODFIL12.txt §3.2 + the period table's
        // first/last cells.
        assert_eq!(PERIOD_MIN, 113);
        assert_eq!(PERIOD_MAX, 856);
        assert_eq!(PERIOD_MIN_EXT, 108);
        assert_eq!(PERIOD_MAX_EXT, 907);
        // Period table corners must agree.
        assert_eq!(PERIOD_TABLE[7][35], 108, "FT +7 B-3 must equal 108");
        assert_eq!(PERIOD_TABLE[8][0], 907, "FT -8 C-1 must equal 907");
    }

    #[test]
    fn porta_up_clamps_at_period_113() {
        // Trigger a note near B-3 (period 120, A#3), then porta-up
        // aggressively for a couple of rows. The period must clamp at
        // 113 (B-3) and not overshoot below it. Per
        // `Protracker-v1.1B-mod.txt` Cmd 1: "You can NOT slide higher
        // than B-3! (Period 113)".
        let bytes = synth_mod_with_pattern(&[
            (
                0,
                0,
                Note {
                    period: 120,
                    sample: 1,
                    effect: 0,
                    effect_param: 0,
                },
            ),
            // 1xx with param 0xFF: tries to slide period down by 255
            // every tick — would overshoot massively without the clamp.
            (
                1,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0x1,
                    effect_param: 0xFF,
                },
            ),
        ]);
        let mut player = make_player(&bytes);
        for _ in 0..12 {
            step_one_tick(&mut player);
        }
        assert_eq!(
            player.channels[0].period, 113,
            "1xx must clamp at period 113 (B-3)"
        );
    }

    #[test]
    fn porta_down_clamps_at_period_856() {
        // Symmetric test for 2xx — must clamp at 856 (C-1).
        let bytes = synth_mod_with_pattern(&[
            (
                0,
                0,
                Note {
                    period: 800,
                    sample: 1,
                    effect: 0,
                    effect_param: 0,
                },
            ),
            (
                1,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0x2,
                    effect_param: 0xFF,
                },
            ),
        ]);
        let mut player = make_player(&bytes);
        for _ in 0..12 {
            step_one_tick(&mut player);
        }
        assert_eq!(
            player.channels[0].period, 856,
            "2xx must clamp at period 856 (C-1)"
        );
    }

    #[test]
    fn effective_period_accepts_finetune_extreme_below_113() {
        // A note at FT +7 B-3 has period 108. Without the extended
        // clamp the mixer would force-clamp to 113 and detune the note
        // by ~5%. With the new clamp at PERIOD_MIN_EXT = 108 the
        // effective_period must pass 108 through unchanged.
        let mut ch = Channel {
            period: 108,
            ..Channel::default()
        };
        // No vibrato active.
        ch.effect = 0;
        ch.mem_vibrato = 0;
        let eff = ch.effective_period(0);
        assert_eq!(eff, 108, "FT +7 B-3 (period 108) must not be clamped");
    }

    #[test]
    fn led_filter_alpha_matches_one_pole_lowpass_at_cutoff() {
        // Sanity-check the analytical filter coefficient.
        // The LED-controlled second pole's alpha at 44.1 kHz with
        // cutoff `LED_FILTER_CUTOFF_HZ`.
        let a = PlayerState::compute_alpha(44_100, PlayerState::LED_FILTER_CUTOFF_HZ);
        let two_pi = 2.0 * std::f32::consts::PI;
        let expected = 1.0 - (-two_pi * PlayerState::LED_FILTER_CUTOFF_HZ / 44_100.0).exp();
        let diff = (a - expected).abs();
        assert!(
            diff < 1e-6,
            "LED alpha mismatch: got {a}, expected {expected}"
        );
        // The 1-pole IIR is stable iff 0 < alpha <= 1.
        assert!(a > 0.0 && a <= 1.0, "alpha {a} outside (0, 1]");
    }

    #[test]
    fn led_filter_attenuates_nyquist_input() {
        // Drive a +1/-1 alternating signal (the worst-case Nyquist
        // content at 44.1 kHz) through the filter directly. The
        // 2-pole model: the always-on first RC pole at
        // `FIXED_RC_CUTOFF_HZ` attenuates HF whether the LED is on
        // or off; the second LED-controlled pole adds further
        // attenuation when ON. Both branches therefore reduce the
        // alternating signal — but the LED-ON branch must reduce
        // it strictly MORE than the LED-OFF branch.
        let make_player_with_led = |led: bool| -> PlayerState {
            let bytes = synth_square_mod();
            let mut player = make_player(&bytes);
            player.led_filter = led;
            player.led_filter_state.clear();
            player.led_filter_state2.clear();
            player.led_filter_alpha = f32::NAN;
            player.led_filter_alpha2 = f32::NAN;
            // Force alpha computation now.
            player.ensure_led_filter(2);
            player
        };

        let drive = |player: &mut PlayerState, n: usize| -> Vec<f32> {
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let x = if i % 2 == 0 { 1.0 } else { -1.0 };
                out.push(player.led_filter_step(0, x));
            }
            out
        };

        let mut p_on = make_player_with_led(true);
        let mut p_off = make_player_with_led(false);
        let on = drive(&mut p_on, 256);
        let off = drive(&mut p_off, 256);

        // Use the second half of the trace (after transient).
        let on_pp: f32 = on[128..].iter().map(|x| x.abs()).fold(0.0f32, f32::max);
        let off_pp: f32 = off[128..].iter().map(|x| x.abs()).fold(0.0f32, f32::max);

        // Both branches attenuate Nyquist (the always-on first RC
        // pole cuts HF unconditionally). LED-ON adds the second
        // pole and must attenuate strictly more.
        assert!(
            off_pp < 1.0,
            "filter-off magnitude {off_pp} should attenuate via the \
             always-on RC pole even when LED is off"
        );
        assert!(
            on_pp < off_pp,
            "filter-on magnitude {on_pp} should be strictly < \
             filter-off magnitude {off_pp} (LED adds a second pole)"
        );
    }

    #[test]
    fn led_filter_default_is_on_at_song_start() {
        // Real Amiga power-on default leaves LED on. PT inherits this
        // and assumes the filter is engaged for the very first note.
        // See `Protracker-v1.1B-mod.txt` Cmd E0 ("E00 connects filter
        // (turns power LED on)").
        let bytes = synth_square_mod();
        let player = make_player(&bytes);
        assert!(
            player.led_filter,
            "LED filter must default to ON (Amiga power-on state)"
        );
    }

    #[test]
    fn e0x_toggles_led_filter() {
        // Row 0: trigger a note. Row 1: E01 (LED OFF). Row 2: E00
        // (LED ON). The player.led_filter flag should follow.
        let bytes = synth_mod_with_pattern(&[
            (
                0,
                0,
                Note {
                    period: 428,
                    sample: 1,
                    effect: 0,
                    effect_param: 0,
                },
            ),
            (
                1,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0xE,
                    effect_param: 0x01,
                },
            ),
            (
                2,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0xE,
                    effect_param: 0x00,
                },
            ),
        ]);
        let mut player = make_player(&bytes);
        // Row 0 — LED should remain ON (default).
        for _ in 0..6 {
            step_one_tick(&mut player);
        }
        assert!(player.led_filter, "row 0: LED still ON");
        // Row 1 — E01 fires on tick 0, LED goes OFF.
        step_one_tick(&mut player);
        assert!(!player.led_filter, "row 1: E01 must clear LED");
        for _ in 0..5 {
            step_one_tick(&mut player);
        }
        // Row 2 — E00 fires, LED back ON.
        step_one_tick(&mut player);
        assert!(player.led_filter, "row 2: E00 must restore LED");
    }

    #[test]
    fn fxx_speed_bpm_split_at_0x20() {
        // Per the convention noted in `Pro-Noise-Soundtracker-rev4.txt`
        // (lines 362-365) and `Protracker-v1.1B-mod.txt` Cmd F:
        //   z < 0x20 → set ticks/division (speed)
        //   z >= 0x20 → set BPM
        // 0x1F (= 31) is the largest speed value, 0x20 (= 32) is the
        // smallest BPM value. Verify both by running the song-level
        // resolution path from `enter_row`.
        let bytes_speed = synth_mod_with_pattern(&[(
            0,
            0,
            Note {
                period: 0,
                sample: 0,
                effect: 0xF,
                effect_param: 0x1F,
            },
        )]);
        let mut player = make_player(&bytes_speed);
        step_one_tick(&mut player);
        assert_eq!(player.speed, 0x1F, "F1F must set speed to 31");
        assert_eq!(player.bpm, DEFAULT_BPM, "F1F must NOT touch BPM");

        let bytes_bpm = synth_mod_with_pattern(&[(
            0,
            0,
            Note {
                period: 0,
                sample: 0,
                effect: 0xF,
                effect_param: 0x20,
            },
        )]);
        let mut player = make_player(&bytes_bpm);
        step_one_tick(&mut player);
        assert_eq!(player.bpm, 0x20, "F20 must set BPM to 32");
        assert_eq!(player.speed, DEFAULT_SPEED, "F20 must NOT touch speed");
    }

    #[test]
    fn e6_dxy_same_row_last_channel_wins() {
        // Place E6x on channel 0 (would loop) and Dxy on channel 1
        // (forces pattern break to specified row). Per the per-channel
        // dispatch in `apply_tick0_effect`, both write to
        // `pending_jump`; the later channel wins (PT idiom).
        // Verify Dxy on a higher channel takes precedence over E6 on
        // a lower channel — the song advances to the break-row, not
        // to the loop-target.
        let bytes = synth_mod_with_pattern(&[
            // Row 0: trigger note on ch0.
            (
                0,
                0,
                Note {
                    period: 428,
                    sample: 1,
                    effect: 0,
                    effect_param: 0,
                },
            ),
            // Row 1: E60 on ch0 (set loop start = row 1).
            (
                1,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0xE,
                    effect_param: 0x60,
                },
            ),
            // Row 2: E61 on ch0 + D05 on ch1. E61 would loop to row 1
            // (jump to row 1 in current order); D05 jumps to row 5 in
            // next order. Per PT (channel 1 > channel 0), Dxy wins.
            (
                2,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0xE,
                    effect_param: 0x61,
                },
            ),
            (
                2,
                1,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0xD,
                    effect_param: 0x05,
                },
            ),
        ]);
        let mut player = make_player(&bytes);
        // Walk rows 0..=2 (3 rows × 6 ticks).
        for _ in 0..18 {
            step_one_tick(&mut player);
        }
        // Then advance one more tick to fire next_row().
        step_one_tick(&mut player);
        // Pattern-break wins: we should now be on row 5 of the next
        // order (and `ended` since there's only one pattern).
        // For our 1-pattern synth, advancing past order 0 sets `ended`.
        assert!(
            player.ended || player.row == 5,
            "Dxy must override E6x when on a higher-numbered channel; \
             got row={}, order={}, ended={}",
            player.row,
            player.order_index,
            player.ended,
        );
    }

    #[test]
    fn vibrato_first_half_lowers_pitch_per_firelight_pseudocode() {
        // FireLight §5.5 says the sine-table values are ADDED to the
        // "AMIGA frequency" (= the period) in the first half-cycle,
        // and SUBTRACTED in the second. Adding to the period LOWERS
        // the audible pitch (pitch ∝ PAULA_CLOCK / period). Verify
        // our implementation matches this convention.
        //
        // This test pins down the canonical interpretation we follow,
        // disambiguating against the alternative C-snippet reading
        // some legacy docs cite.
        let mut ch = Channel {
            period: 428,
            sample_index: 1,
            volume: 64,
            active: true,
            effect: 0x4,
            mem_vibrato: 0x84, // rate=8, depth=4
            vib_pos: 8,        // mid first half-cycle (positive)
            ..Channel::default()
        };
        ch.vib_wave.shape = 0; // sine
        let off = PlayerState::vibrato_offset(&ch);
        assert!(
            off > 0,
            "Per FireLight §5.5: positive vib_pos must ADD to period \
             (lowering pitch). Got offset {off}."
        );
        // Effective period should be > base period.
        let eff = ch.effective_period(off);
        assert!(
            eff > ch.period,
            "effective_period must rise on positive vib_pos"
        );

        // Now drive it negative — second half-cycle subtracts.
        ch.vib_pos = -8;
        let off = PlayerState::vibrato_offset(&ch);
        assert!(
            off < 0,
            "Per FireLight §5.5: negative vib_pos must SUBTRACT from period \
             (raising pitch). Got offset {off}."
        );
    }

    #[test]
    fn loop_metadata_clamped_when_out_of_range() {
        // Defensive: real-world MOD rips sometimes have repeat metadata
        // that extends past the actual sample length. extract_samples
        // must clamp to keep the mixer from reading past the buffer.
        let mut bytes = vec![0u8; crate::header::HEADER_FIXED_SIZE];
        bytes[0..4].copy_from_slice(b"clmp");
        // Sample length = 16 words = 32 frames.
        bytes[20 + 22..20 + 24].copy_from_slice(&16u16.to_be_bytes());
        bytes[20 + 25] = 64;
        // Repeat start = 100 words (way past the sample). Repeat length = 200.
        bytes[20 + 26..20 + 28].copy_from_slice(&100u16.to_be_bytes());
        bytes[20 + 28..20 + 30].copy_from_slice(&200u16.to_be_bytes());
        bytes[950] = 1;
        bytes[951] = 0x7F;
        bytes[952] = 0;
        bytes[1080..1084].copy_from_slice(b"M.K.");
        bytes.extend(std::iter::repeat_n(0u8, 64 * 4 * 4));
        bytes.extend(std::iter::repeat_n(0u8, 32));
        let header = crate::header::parse_header(&bytes).unwrap();
        let samples = crate::samples::extract_samples(&header, &bytes);
        // After clamping, loop_start/length must fit within pcm.len() = 32.
        let s = &samples[0];
        let end = s.loop_start + s.loop_length;
        assert!(
            end as usize <= s.pcm.len(),
            "loop_end ({end}) must be clamped to pcm.len() ({})",
            s.pcm.len()
        );
    }

    /// Regression for the round-105 cyber.mod arpeggio bug:
    ///
    /// When effect 0xy (arpeggio) carries across rows — i.e. row N
    /// triggers a note + arpeggio, and row N+1 has no new note but
    /// continues the same effect 0xy — the channel's
    /// `arp_base_period` MUST stay anchored to the original triggered
    /// note. Without the fix, the previous row's last tick left
    /// `ch.period` at a semitone-shifted value (FireLight §5.1
    /// pseudo: tick%3==2 → +y); the "no note" branch in `enter_row`
    /// then captured that modulated period as the NEW arp base, so
    /// every continuation row shifted the chord up another (x, y)
    /// step. On `cyber.mod` pat-1 ch-2 (rows 32-58, sample 5
    /// `st-07:buzzshot`) this produced an audibly out-of-tune lead
    /// in the 12-14 s region, which the user reported as "effects a
    /// bit off". See `Protracker-effects-MODFIL12.txt` 0:Arpeggio
    /// ("This effect means to play the note specified, then the
    /// note+xxxx half-steps, then the note+yyyy half-steps, and then
    /// return to the original note"; the "original note" is the
    /// triggered note, not the previous tick's modulated value) and
    /// FireLight-MOD-Player-Tutorial.txt §5.1 ("Tick 0 set frequency
    /// to normal value").
    #[test]
    fn arpeggio_base_persists_across_rows_without_new_note() {
        // Row 0: trigger C-2 (period 428) on channel 0 with arpeggio
        //        x=3, y=4 (effect 034).
        // Row 1: NO new note, same arpeggio 034 still active.
        // Expected at row 1, tick 1 (after enter_row + 1 tickN): the
        // period must equal `PERIOD_TABLE[0][12 + 3]` (C-2 + 3 semis
        // = D#-2 = 339), NOT the bug-shifted value
        // `PERIOD_TABLE[0][15 + 3]` (which would be a fifth higher).
        // Row 1 tick 0: must equal the un-modulated base (period 428),
        // not the leftover modulated period from row 0 tick 5
        // (which would be 339 = base + x semis).
        let bytes = synth_mod_with_pattern(&[
            (
                0,
                0,
                Note {
                    period: 428,
                    sample: 1,
                    effect: 0,
                    effect_param: 0x34,
                },
            ),
            (
                1,
                0,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0,
                    effect_param: 0x34,
                },
            ),
        ]);
        let mut player = make_player(&bytes);

        // Step 8 ticks: ticks 0..5 of row 0 (6 ticks) + ticks 0..1 of
        // row 1 (2 ticks). step_one_tick advances state at the end of
        // each call; the 7th call's `advance_tick` calls
        // `enter_row(row=1)` (where the round-105 fix restores period
        // to the un-modulated base), and the 8th call's
        // `advance_tick` runs `apply_tickn` for tick 1 of row 1
        // (which applies the +x arpeggio offset).
        for _ in 0..8 {
            step_one_tick(&mut player);
        }
        // After the round-105 fix, `arp_base_period` must still be
        // anchored to the original triggered period (428). Pre-fix
        // this would equal the previous row's last-tick modulated
        // period (e.g. 320 = C-2 + y=4 semis = E-2), causing the
        // chord to shift up by (x, y) on every continuation row.
        assert_eq!(
            player.channels[0].arp_base_period, 428,
            "arpeggio base must persist across rows that have no new \
             note — got {}, expected 428 (the original note period). \
             Pre-fix this would equal the previous row's last-tick \
             modulated period.",
            player.channels[0].arp_base_period
        );

        // After 7 ticks total: tick 1 of row 1 has just been rendered
        // (tick=2 in the player's internal counter). Arpeggio sets
        // period = base + x semitones = `PERIOD_TABLE[0][12 + 3]`
        // = 360 (D#-2 in finetune-0 row). With the bug this would be
        // `PERIOD_TABLE[0][15 + 3]` ≈ 285 — a fifth higher than the
        // intended +3 semis.
        assert_eq!(
            player.channels[0].period,
            PERIOD_TABLE[0][12 + 3],
            "row 1 tick 1 of a continuation arpeggio must land on \
             base + x semitones (PERIOD_TABLE[0][15] = {}), not on \
             a doubly-shifted value",
            PERIOD_TABLE[0][12 + 3]
        );
    }

    /// Default per-channel pan in a freshly-built `PlayerState` follows
    /// the Amiga LRRL hard-pan convention (channels 0 & 3 → 0/LEFT,
    /// 1 & 2 → 255/RIGHT, repeating every 4) so that a MOD with no
    /// 8xx / E8x commands renders identically to the pre-r75 build.
    /// Cited in `Protracker-effects-MODFIL12.txt` §11 (the same hard-pan
    /// layout that motivates `pan_separation < 1.0` for headphone
    /// listeners).
    #[test]
    fn channel_pan_defaults_to_amiga_lrrl() {
        let bytes = synth_square_mod();
        let player = make_player(&bytes);
        // 4-channel synth: ch 0 & 3 → 0 (LEFT), ch 1 & 2 → 255 (RIGHT).
        assert_eq!(player.channels[0].pan, 0, "ch 0 default = LEFT (0)");
        assert_eq!(player.channels[1].pan, 255, "ch 1 default = RIGHT (255)");
        assert_eq!(player.channels[2].pan, 255, "ch 2 default = RIGHT (255)");
        assert_eq!(player.channels[3].pan, 0, "ch 3 default = LEFT (0)");
    }

    /// `8xx` is the FT-extension Set Fine Panning command:
    /// `Protracker-effects-MODFIL12.txt` lines 1201-1207
    ///   "Command 8: Set FINE Panning. xxxxyyyy = panning position.
    ///    (0=Most left, 255=most right.)"
    /// Verify tick-0 dispatch latches the byte verbatim into `ch.pan`.
    #[test]
    fn effect_8xx_sets_per_channel_pan() {
        let bytes = synth_mod_with_pattern(&[
            // Row 0: trigger note on ch 1, then 8xx with param 0x40 on ch 1.
            (
                0,
                1,
                Note {
                    period: 428,
                    sample: 1,
                    effect: 0x8,
                    effect_param: 0x40,
                },
            ),
        ]);
        let mut player = make_player(&bytes);
        // Sanity — Amiga default for ch 1 = RIGHT (255).
        assert_eq!(player.channels[1].pan, 255);
        // Tick 0 dispatches the row's tick-0 effects.
        step_one_tick(&mut player);
        assert_eq!(
            player.channels[1].pan, 0x40,
            "8xx must overwrite ch.pan with the raw param byte"
        );
        // Endpoints both directions.
        let bytes_left = synth_mod_with_pattern(&[(
            0,
            2,
            Note {
                period: 0,
                sample: 0,
                effect: 0x8,
                effect_param: 0x00,
            },
        )]);
        let mut p2 = make_player(&bytes_left);
        step_one_tick(&mut p2);
        assert_eq!(p2.channels[2].pan, 0x00, "800 = hard LEFT");

        let bytes_right = synth_mod_with_pattern(&[(
            0,
            0,
            Note {
                period: 0,
                sample: 0,
                effect: 0x8,
                effect_param: 0xFF,
            },
        )]);
        let mut p3 = make_player(&bytes_right);
        step_one_tick(&mut p3);
        assert_eq!(p3.channels[0].pan, 0xFF, "8FF = hard RIGHT");
    }

    /// `E8x` is the rough nibble-pan extension:
    /// `Protracker-effects-MODFIL12.txt` lines 1503-1505
    ///   "Command $E8: Set (Rough) Panning. yyyy = panning value.
    ///    $0 = most left, $F = most right."
    /// The nibble is replicated into both halves of the byte
    /// (`y << 4 | y`) so E80 → 0x00, E8F → 0xFF, E87 → 0x77, E88 →
    /// 0x88 — matching the same endpoint mapping as 8xx and the
    /// monotonic "rough" 16-step ramp documented in
    /// `multimedia-cx-protracker.html` E8x.
    #[test]
    fn effect_e8x_sets_rough_pan_from_nibble() {
        let table = [
            (0x0u8, 0x00u8),
            (0x1, 0x11),
            (0x7, 0x77),
            (0x8, 0x88),
            (0xF, 0xFF),
        ];
        for (nibble, expected) in table {
            let bytes = synth_mod_with_pattern(&[(
                0,
                1,
                Note {
                    period: 0,
                    sample: 0,
                    effect: 0xE,
                    effect_param: 0x80 | nibble,
                },
            )]);
            let mut player = make_player(&bytes);
            step_one_tick(&mut player);
            assert_eq!(
                player.channels[1].pan, expected,
                "E8{:X} must set ch.pan to 0x{:02X} (nibble replicated)",
                nibble, expected,
            );
        }
    }

    /// `pan_gains` must collapse to the pre-r75 hard-pan formula at the
    /// LRRL endpoints (pan = 0 or 255) for any `s` — that's the
    /// invariant that keeps the trace-reference-impl calibration in
    /// `sample_all_channels` valid bit-for-bit on MODs that don't use
    /// 8xx / E8x. The centre (pan = 128) must split evenly regardless
    /// of `s`; that's the property that lets per-channel pan coexist
    /// with the global `pan_separation` narrow.
    #[test]
    fn pan_gains_matches_legacy_hard_pan_at_endpoints() {
        for s_int in 0..=10 {
            let s = s_int as f32 / 10.0;
            // p = 0: hard LEFT. Legacy formula for a hard-LEFT channel:
            //   l_gain = (1 + s) / 2, r_gain = (1 - s) / 2.
            let (l, r) = pan_gains(0, s);
            let expected_l = (1.0 + s) * 0.5;
            let expected_r = (1.0 - s) * 0.5;
            assert!(
                (l - expected_l).abs() < 1e-6 && (r - expected_r).abs() < 1e-6,
                "pan=0 s={s}: got ({l},{r}), expected ({expected_l},{expected_r})",
            );
            // p = 255: hard RIGHT. Symmetric: l = (1 - s) / 2,
            // r = (1 + s) / 2.
            let (l, r) = pan_gains(255, s);
            assert!(
                (l - (1.0 - s) * 0.5).abs() < 1e-6 && (r - (1.0 + s) * 0.5).abs() < 1e-6,
                "pan=255 s={s}: got ({l},{r}), expected hard-RIGHT mirror"
            );
        }
        // Centre (p = 128 ≈ 0.5 + ε) splits roughly evenly for any s.
        for s_int in 0..=10 {
            let s = s_int as f32 / 10.0;
            let (l, r) = pan_gains(128, s);
            // Drift from exact 0.5 is tiny (128/255 ≠ 0.5 exactly).
            let drift = (l - r).abs();
            assert!(
                drift < 0.01,
                "centred pan must yield near-equal L/R for s={s}; got ({l},{r}) drift={drift}",
            );
        }
    }

    /// End-to-end smoke: a synth MOD with no 8xx / E8x commands must
    /// render exactly the same bytes after the per-channel-pan rework
    /// as before (the LRRL initialisation + endpoint collapse of
    /// `pan_gains` guarantees this). We verify by checking that
    /// `render` produces non-trivial signal on BOTH stereo lanes for
    /// a 4-ch MOD whose only triggered channel is ch 0 (LEFT-panned)
    /// — the `pan_separation = 0.5` default bleeds it to the right
    /// at 25 % gain, which is exactly the prior behaviour.
    #[test]
    fn render_with_no_pan_commands_preserves_legacy_bleed() {
        let bytes = synth_square_mod();
        let mut player = make_player(&bytes);
        let mut buf = vec![0i16; 1024 * 2];
        let produced = player.render(&mut buf);
        assert!(produced > 0, "must render some samples");
        let peak_l = buf
            .chunks_exact(2)
            .map(|c| c[0].unsigned_abs() as u32)
            .max()
            .unwrap_or(0);
        let peak_r = buf
            .chunks_exact(2)
            .map(|c| c[1].unsigned_abs() as u32)
            .max()
            .unwrap_or(0);
        assert!(peak_l > 0, "LEFT lane must carry signal");
        assert!(
            peak_r > 0,
            "RIGHT lane must carry bleed signal (default pan_separation = 0.5)"
        );
        // Bleed ratio: hard-LEFT ch under separation 0.5 → 25 % on R,
        // 75 % on L. Tolerate ramp + filter / interpolation slack.
        let ratio = peak_r as f32 / peak_l as f32;
        assert!(
            (0.25..=0.4).contains(&ratio),
            "RIGHT-to-LEFT bleed ratio for hard-LEFT ch at s=0.5 must be ~0.33; got {ratio}"
        );
    }
}
