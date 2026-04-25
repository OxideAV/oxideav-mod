//! ProTracker playback engine.
//!
//! Drives a `PlayerState` forward one tick at a time. Two output modes
//! share the same mixing core:
//!
//! - [`PlayerState::render`] writes interleaved stereo S16 PCM, applying
//!   the Amiga hard-pan convention (channels 0 & 3 left, 1 & 2 right;
//!   repeats every 4 for >4-channel files) and a 1/(N/2) headroom scale.
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

/// Protracker period floor: B-3 at finetune 0 is 113. `1xx` and `3xy`
/// must not slide below this value.
pub const PERIOD_MIN: u16 = 113;
/// Period ceiling used by `2xx` clamps. Classic C-1 at finetune 0 is 856.
/// Protracker's replayer further allows down-slides into the
/// extended-period region; we keep the spec-documented upper bound for
/// clean playback (FireLight §5.3 suggests "stop at C-1 or C-0").
pub const PERIOD_MAX: u16 = 856;

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
    fn effective_period(&self, vib_offset: i16) -> u16 {
        let p = self.period as i32 + vib_offset as i32;
        p.clamp(PERIOD_MIN as i32, 10_000) as u16
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

        let pos = self.sample_pos;
        let len = body.pcm.len() as f32;
        if pos >= len {
            // Either loop or stop.
            if body.is_looped() {
                let loop_end = (body.loop_start + body.loop_length) as f32;
                let loop_start = body.loop_start as f32;
                let span = loop_end - loop_start;
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

        // Linear interpolation between two nearest samples.
        let i = self.sample_pos as usize;
        let frac = self.sample_pos - i as f32;
        let s0 = body.pcm[i.min(body.pcm.len() - 1)] as f32 / 128.0;
        let s1_idx = if i + 1 < body.pcm.len() {
            i + 1
        } else if body.is_looped() {
            body.loop_start as usize
        } else {
            i
        };
        let s1 = body.pcm[s1_idx.min(body.pcm.len() - 1)] as f32 / 128.0;
        let interp = s0 + (s1 - s0) * frac;

        // Apply tremolo to the effective volume; clamp 0..=64.
        let eff_vol = (self.volume as i16 + trem_offset).clamp(0, 64);
        let out = interp * (eff_vol as f32 / 64.0);

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
}

impl PlayerState {
    pub fn new(
        header: &ModHeader,
        samples: Vec<SampleBody>,
        patterns: Vec<Pattern>,
        sample_rate: u32,
    ) -> Self {
        let channels = (0..header.channels)
            .map(|_| Channel::default())
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
        }
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
            ch.effect = effect;
            ch.effect_param = param;
            ch.cut_tick = 0;
            ch.delay = None;
            ch.retrig_ticks = 0;

            // Sample change: loads default volume + finetune even without a
            // note. The new sample starts playing only if a note is present
            // on the same row (standard PT behaviour).
            if note.sample != 0 {
                let idx = note.sample as usize;
                ch.sample_index = note.sample;
                if idx >= 1 && idx <= self.samples.len() {
                    let body = &self.samples[idx - 1];
                    ch.volume = body.volume;
                    ch.finetune = body.finetune;
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
                ch.sample_pos = offset_frames as f32;
                ch.active = true;
                ch.arp_base_period = note_period;

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
        // A later channel's Fxx supersedes an earlier one (PT behaviour).
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
    }

    /// Advance one tick (called at the start of every tick).
    fn advance_tick(&mut self) {
        if self.tick == 0 {
            self.enter_row();
        } else {
            // Tick-N effects run per channel.
            for ch_idx in 0..self.channels.len() {
                apply_tickn_effect(ch_idx, self.tick, &mut self.channels, &self.samples);
            }
        }
    }

    /// Move to next row (or jump).
    fn next_row(&mut self) {
        // EEx pattern delay: repeat the current row `pattern_delay` more times.
        if self.pattern_delay > 0 {
            self.pattern_delay -= 1;
            return;
        }
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
    fn sample_all_channels(&mut self, per_channel: &mut [f32]) -> (f32, f32) {
        let out_rate = self.sample_rate as f32;
        let mut l = 0.0f32;
        let mut r = 0.0f32;
        let n_ch = self.channels.len();
        for (i, ch) in self.channels.iter_mut().enumerate() {
            let vib = Self::vibrato_offset(ch);
            let trem = Self::tremolo_offset(ch);
            let s = ch.mix_one(&self.samples, out_rate, vib, trem);
            per_channel[i] = s;
            if Self::channel_is_left(i) {
                l += s;
            } else {
                r += s;
            }
        }
        let norm = (n_ch as f32 / 2.0).max(1.0);
        (l / norm, r / norm)
    }

    /// Render one stereo S16 interleaved sample pair by mixing all channels.
    /// Channels 0 and 3 pan hard-left, 1 and 2 hard-right (Amiga convention).
    fn render_one(&mut self, out: &mut [i16]) {
        let mut per_channel = vec![0.0f32; self.channels.len()];
        let (l, r) = self.sample_all_channels(&mut per_channel);
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

            for _ in 0..want {
                // Discard the stereo mix; we only need per-channel here.
                let _ = self.sample_all_channels(&mut scratch);
                for (ch_idx, plane) in planes.iter_mut().enumerate() {
                    let v = scratch[ch_idx].clamp(-1.0, 1.0);
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
        0x0 => { /* E0x set filter — audible only on real A500 hardware. */ }
        0x1 => {
            // E1x: fine portamento up — one-shot slide on tick 0.
            ch.period = ch.period.saturating_sub(y as u16).max(PERIOD_MIN);
        }
        0x2 => {
            // E2x: fine portamento down.
            ch.period = (ch.period + y as u16).min(1023);
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
        0x8 => { /* E8x — unused in classic PT. */ }
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
            }
            ch.period = delayed.period;
            ch.sample_pos = 0.0;
            ch.active = true;
            ch.arp_base_period = delayed.period;
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
    ch.period = new.clamp(PERIOD_MIN as i32, PERIOD_MAX as i32) as u16;

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
        // Patch sample 1 length to 256 words (512 frames). Keep loop at 0/16.
        bytes[20 + 22..20 + 24].copy_from_slice(&256u16.to_be_bytes());
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
    fn pattern_delay_ee_repeats_row() {
        // EE1 with a trigger that increments volume on each row via EA1;
        // the row should "play" twice before advancing to the next.
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
        // Row "1 again" tick 0: EA2 increments volume from 2 → 4.
        step_one_tick(&mut player);
        assert_eq!(
            player.channels[0].volume, 4,
            "EE1 must cause row 1 to play twice"
        );
    }
}
