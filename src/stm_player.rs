//! Scream Tracker v1 (`.stm`) playback engine.
//!
//! Drives the shared [`crate::mixer::MixerVoice`] core over STM's
//! 4-channel pattern engine. STM uses per-instrument C3 frequencies
//! rather than Amiga periods (see [`crate::mixer::StmC3Pitch`]) and
//! ProTracker-like effect columns.
//!
//! Effects implemented (round 9):
//!
//!  - Cxx: set volume.
//!  - Axy: volume slide (+x per tick, else -y).
//!  - Fxx: set speed / tempo (<0x20 = speed, >=0x20 = tempo).
//!  - Bxy: order / position jump.
//!  - Dxy: pattern break (FT2-style decimal `x*10+y` landing row —
//!    matches XM parity; STM files in the wild typically encode row 0
//!    as plain `D00` so the decimal quirk is a non-issue in practice).
//!  - 1xy: porta up — shift pitch up by `p` units per tick.
//!  - 2xy: porta down — shift pitch down.
//!  - 3xy: tone portamento — glide semitone-offset toward the target
//!    note without retriggering; per-channel `porta_target` + memory.
//!  - 4xy: vibrato — sine LFO on pitch in semitone units; per-nibble
//!    speed + depth memory.
//!  - 5xy: tone porta + volume slide (combined).
//!  - 6xy: vibrato + volume slide.
//!  - Exy subcommands: E1x / E2x fine porta, EA x / EB x fine volume
//!    slide, EC x note cut, ED x note delay.
//!
//! STM lacks period representation (pitch is derived from C3 Hz and a
//! `(octave, semitone)` token), so pitch effects operate in the
//! **semitone** domain:
//!
//!   `freq = base_c3_hz * 2 ^ ((cur_semitone_offset - 3*12) / 12)`
//!
//! where `cur_semitone_offset` is a fractional semitone count measured
//! from C-0. Porta "period units" in STM are very loose; we treat the
//! ProTracker-style parameter as *centisemitones* (1 unit = 1/16 of a
//! semitone) which gives musically-plausible slide rates — a 3xy speed
//! of 0x80 traverses an octave in ~12 ticks.

use crate::mixer::{MixerVoice, PitchModel, StmC3Pitch};
use crate::stm::{
    StmCell, StmHeader, StmNoteKind, StmPattern, StmSampleBody, PATTERN_ROWS, STM_CHANNELS,
};

/// Classic tracker pacing constants. STM's tempo field is related to the
/// ticks-per-row and BPM-equivalent values; we keep the MOD defaults
/// unless the file overrides them.
pub const DEFAULT_SPEED_TICKS: u8 = 6;

/// 64-entry signed sine table for vibrato — one quarter-wave reaches
/// ±127 at indices 16 and 48. Matches the XM vibrato table so the STM
/// and XM vibrato implementations behave identically.
#[rustfmt::skip]
const SINE_TABLE: [i8; 64] = [
      0,  12,  25,  37,  49,  60,  71,  81,
     90,  98, 106, 112, 117, 122, 125, 126,
    127, 126, 125, 122, 117, 112, 106,  98,
     90,  81,  71,  60,  49,  37,  25,  12,
      0, -12, -25, -37, -49, -60, -71, -81,
    -90, -98,-106,-112,-117,-122,-125,-126,
   -127,-126,-125,-122,-117,-112,-106, -98,
    -90, -81, -71, -60, -49, -37, -25, -12,
];

/// Number of "porta units" per semitone. ProTracker's porta parameters
/// are in Amiga period units, where one semitone ≈ 38 units near C-4.
/// STM has no periods, so we pick a scale that matches musical
/// expectations: `SEMITONE_UNITS = 16` means a 3xy speed of 16 walks
/// exactly one semitone per tick, so speed 0x08 ≈ half a semitone.
const SEMITONE_UNITS: f32 = 16.0;

/// Convert a `(octave, semitone)` note into a continuous semitone index
/// from C-0. `real = octave*12 + semitone`. STM encodes octave in the
/// high nibble and semitone in the low nibble.
fn note_to_semis(octave: u8, semitone: u8) -> f32 {
    (octave as f32) * 12.0 + (semitone as f32)
}

/// Convert a fractional semitone index from C-0 into a playback
/// frequency using the given C3 reference Hz. The STM pitch model
/// maps (octave=3, semitone=0) → c3_hz, so we subtract 3 octaves.
fn semis_to_freq(c3_hz: f32, semis_from_c0: f32) -> f32 {
    if c3_hz <= 0.0 {
        return 0.0;
    }
    c3_hz * 2.0f32.powf((semis_from_c0 - 3.0 * 12.0) / 12.0)
}

/// Per-channel playback state for STM.
#[derive(Clone, Debug, Default)]
pub struct StmChannel {
    /// Currently-loaded instrument index (1..=31, 0 = none).
    pub instrument: u8,
    /// Current note (octave, semitone) for the currently playing sample.
    pub note: (u8, u8),
    /// Volume 0..=64.
    pub volume: u8,
    /// Shared mixer voice.
    pub voice: MixerVoice,
    /// Current effect command 0..=0xF.
    pub effect: u8,
    /// Current effect parameter.
    pub effect_param: u8,

    // -------- pitch state (semitone-space) --------
    /// Current live semitone index (fractional, measured from C-0).
    /// Porta / vibrato / fine porta all mutate this; the voice
    /// frequency is derived from it every tick.
    pub cur_semis: f32,
    /// Target semitone index for tone portamento (3xy / 5xy).
    pub porta_target_semis: f32,
    /// Tone-porta speed memory (shared between 3xy and 5xy).
    pub porta_speed: u8,
    /// 1xy / 2xy memory — last non-zero parameter (shared).
    pub porta_updown_mem: u8,
    /// Vibrato sine-table position 0..=63.
    pub vib_pos: u8,
    /// Vibrato speed memory (last non-zero 4xy `x` nibble).
    pub vib_speed: u8,
    /// Vibrato depth memory (last non-zero 4xy `y` nibble).
    pub vib_depth: u8,
    /// Axy volume-slide parameter memory (shared with 5xy / 6xy).
    pub vol_slide_mem: u8,

    // -------- scheduling --------
    /// Pending note-cut tick (ECx): if >0, volume forced to 0 on that
    /// tick.
    pub note_cut_tick: u8,
    /// Pending note-delay tick (EDx): if >0, the cell's note is
    /// triggered on this tick instead of tick 0.
    pub note_delay_tick: u8,
    /// Saved trigger data for a pending note-delay.
    pub pending_note: (u8, u8),
    pub pending_instrument: u8,
    pub pending_volume: u8,
    /// True if the pending note-delay slot is populated.
    pub has_pending_delay: bool,
}

/// STM player — owns decoded patterns / samples and a
/// row/tick/BPM-style state machine.
pub struct StmPlayerState {
    pub samples: Vec<StmSampleBody>,
    pub patterns: Vec<StmPattern>,
    pub order: Vec<u8>,
    pub n_patterns: u8,
    pub channels: [StmChannel; STM_CHANNELS],
    pub speed: u8,
    pub tempo: u8,
    pub sample_rate: u32,

    pub order_index: usize,
    pub row: u8,
    pub tick: u8,
    pub tick_sample_cursor: u32,
    pub ended: bool,
    pub global_volume: u8,

    /// Pending pattern jump (Bxy): set on tick 0 of a row, consumed by
    /// `next_row`.
    pub pending_order_jump: Option<u8>,
    /// Pending pattern-break row (Dxy): set on tick 0, consumed by
    /// `next_row`.
    pub pending_break_row: Option<u8>,
}

impl StmPlayerState {
    pub fn new(
        header: &StmHeader,
        samples: Vec<StmSampleBody>,
        patterns: Vec<StmPattern>,
        sample_rate: u32,
    ) -> Self {
        // Trim order to entries that actually reference a valid pattern.
        let order: Vec<u8> = header
            .order
            .iter()
            .copied()
            .take_while(|&b| b != 255)
            .collect();
        StmPlayerState {
            samples,
            patterns,
            order,
            n_patterns: header.n_patterns,
            channels: Default::default(),
            speed: DEFAULT_SPEED_TICKS,
            tempo: header.tempo.max(1),
            sample_rate,
            order_index: 0,
            row: 0,
            tick: 0,
            tick_sample_cursor: 0,
            ended: false,
            global_volume: header.global_volume.max(1),
            pending_order_jump: None,
            pending_break_row: None,
        }
    }

    /// Samples-per-tick using the MOD-style formula, with tempo treated
    /// as a BPM-ish equivalent. Scream Tracker v1's tempo register is
    /// historically `tempo * 2` compared to the S3M / MOD scale; we
    /// approximate with `bpm_equiv = tempo * 125 / 0x60`, matching the
    /// `estimate_duration_micros` heuristic in [`crate::stm`].
    pub fn samples_per_tick(&self) -> u32 {
        let bpm_equiv = ((self.tempo as u32) * 125 / 0x60).max(30);
        ((self.sample_rate as f32) * 2.5 / bpm_equiv as f32).max(1.0) as u32
    }

    /// Retrieve the cell at (current order → pattern, row, channel).
    fn cell_at(&self, row: u8, ch: usize) -> Option<StmCell> {
        let pat_idx = *self.order.get(self.order_index)? as usize;
        let pattern = self.patterns.get(pat_idx)?;
        pattern.rows.get(row as usize)?.get(ch).copied()
    }

    /// Enter a row: load note / volume / effect into each channel.
    fn enter_row(&mut self) {
        for ch_idx in 0..STM_CHANNELS {
            let Some(cell) = self.cell_at(self.row, ch_idx) else {
                continue;
            };
            let ch = &mut self.channels[ch_idx];
            ch.effect = cell.command;
            ch.effect_param = cell.command_param;
            // Reset per-row scheduling.
            ch.note_cut_tick = 0;
            ch.note_delay_tick = 0;
            ch.has_pending_delay = false;

            // Tone-porta detection: 3xy / 5xy turn the note (if any)
            // into a glide target, not a retrigger.
            let is_tone_porta_cell = cell.command == 0x3 || cell.command == 0x5;

            // Sample change — update the current instrument and pull
            // volume from the sample body.
            if cell.instrument != 0 {
                ch.instrument = cell.instrument;
                if let Some(body) = self.samples.get(cell.instrument as usize - 1) {
                    ch.volume = body.volume.min(64);
                }
            }

            // Cell volume overrides sample default.
            if cell.volume > 0 && cell.volume <= 64 {
                ch.volume = cell.volume;
            }

            // Memorise effect parameters (zero nibble = reuse last).
            let ep = ch.effect_param;
            match ch.effect {
                0x1 | 0x2 if ep != 0 => {
                    ch.porta_updown_mem = ep;
                }
                0x3 if ep != 0 => {
                    ch.porta_speed = ep;
                }
                0x4 => {
                    let vx = ep >> 4;
                    let vy = ep & 0x0F;
                    if vx != 0 {
                        ch.vib_speed = vx;
                    }
                    if vy != 0 {
                        ch.vib_depth = vy;
                    }
                }
                0x5 | 0x6 | 0xA if ep != 0 => {
                    ch.vol_slide_mem = ep;
                }
                _ => {}
            }

            // Note trigger.
            match cell.kind() {
                StmNoteKind::Note { octave, semitone } if semitone <= 11 => {
                    let target = note_to_semis(octave, semitone);
                    if is_tone_porta_cell && ch.voice.active && ch.cur_semis > 0.0 {
                        // Set glide target without retriggering; keep
                        // current cur_semis so the slide starts from
                        // wherever we are.
                        ch.note = (octave, semitone);
                        ch.porta_target_semis = target;
                    } else {
                        ch.note = (octave, semitone);
                        let is_delay = cell.command == 0xE
                            && (cell.command_param >> 4) == 0xD
                            && (cell.command_param & 0x0F) != 0;
                        if is_delay {
                            ch.note_delay_tick = cell.command_param & 0x0F;
                            ch.pending_note = (octave, semitone);
                            ch.pending_instrument = cell.instrument;
                            ch.pending_volume = cell.volume;
                            ch.has_pending_delay = true;
                        } else {
                            let inst_idx = match (ch.instrument as usize).checked_sub(1) {
                                Some(i) => i,
                                None => continue,
                            };
                            if let Some(body) = self.samples.get(inst_idx) {
                                let pitch = StmC3Pitch {
                                    c3_hz: body.c3_hz as f32,
                                };
                                let freq = pitch.note_to_freq(ch.note);
                                let vol =
                                    (ch.volume as f32 / 64.0) * (self.global_volume as f32 / 64.0);
                                ch.voice.trigger(freq, vol);
                                ch.cur_semis = target;
                                ch.porta_target_semis = target;
                                // Fresh note resets vibrato phase.
                                ch.vib_pos = 0;
                            }
                        }
                    }
                }
                StmNoteKind::DashNote | StmNoteKind::Dots => {
                    // Per spec: "note off" style markers. Silence the voice.
                    ch.voice.active = false;
                }
                _ => {}
            }

            // Tick-0 effects.
            apply_tick0_effect(ch);
        }

        // Row-level song-state effects: Bxy / Dxy / Fxx.
        for ch in self.channels.iter() {
            match ch.effect {
                0xB => {
                    self.pending_order_jump = Some(ch.effect_param);
                    if self.pending_break_row.is_none() {
                        self.pending_break_row = Some(0);
                    }
                }
                0xD => {
                    // FT2-style decimal row (matches XM parity).
                    let row = (ch.effect_param >> 4) * 10 + (ch.effect_param & 0x0F);
                    self.pending_break_row = Some(row.min((PATTERN_ROWS - 1) as u8));
                }
                0xF => {
                    if ch.effect_param == 0 {
                        self.ended = true;
                    } else if ch.effect_param < 0x20 {
                        self.speed = ch.effect_param;
                    } else {
                        self.tempo = ch.effect_param;
                    }
                }
                _ => {}
            }
        }
    }

    fn advance_tick(&mut self) {
        if self.tick == 0 {
            self.enter_row();
        } else {
            for ch in self.channels.iter_mut() {
                apply_tickn_effect(ch);
            }
        }

        // Per-tick pitch recompute: vibrato + (schedule-based) note
        // cut / note delay. Runs on every tick (including tick 0).
        let cur_tick = self.tick;
        let global_vol = self.global_volume;
        for ch_idx in 0..STM_CHANNELS {
            let ch = &mut self.channels[ch_idx];

            // Note cut: force volume to 0 at tick x.
            if ch.note_cut_tick > 0 && cur_tick == ch.note_cut_tick {
                ch.volume = 0;
            }

            // Note delay: trigger voice at tick x.
            if ch.has_pending_delay && ch.note_delay_tick > 0 && cur_tick == ch.note_delay_tick {
                let inst = ch.pending_instrument;
                if inst != 0 {
                    ch.instrument = inst;
                }
                let (po, ps) = ch.pending_note;
                let idx = ch.instrument.saturating_sub(1) as usize;
                if ch.pending_volume > 0 && ch.pending_volume <= 64 {
                    ch.volume = ch.pending_volume;
                }
                if let Some(body) = self.samples.get(idx) {
                    let pitch = StmC3Pitch {
                        c3_hz: body.c3_hz as f32,
                    };
                    let freq = pitch.note_to_freq((po, ps));
                    let vol = (ch.volume as f32 / 64.0) * (global_vol as f32 / 64.0);
                    ch.voice.trigger(freq, vol);
                    ch.note = (po, ps);
                    ch.cur_semis = note_to_semis(po, ps);
                    ch.porta_target_semis = ch.cur_semis;
                    ch.vib_pos = 0;
                }
                ch.has_pending_delay = false;
                ch.note_delay_tick = 0;
            }

            // Derive the voice frequency from the current semitone state
            // plus any live vibrato offset.
            let mut semis = ch.cur_semis;
            let vib_active = (ch.effect == 0x4 || ch.effect == 0x6) && ch.vib_depth > 0;
            if vib_active {
                let lfo = SINE_TABLE[(ch.vib_pos & 0x3F) as usize] as f32;
                // Peak deviation: depth * 16 / 128 / SEMITONE_UNITS semitones
                // Simplify: (lfo / 128) * depth * 16 / SEMITONE_UNITS.
                // With SEMITONE_UNITS=16, this is (lfo/128)*depth — i.e.
                // depth=15 gives ≈ ±15/128 ≈ ±0.117 semitones peak which
                // is much too subtle; XM uses a wider depth. Match XM:
                // treat (depth * sine / 32) as period-unit offset, convert
                // period-units-to-semitones by dividing by SEMITONE_UNITS.
                let off_units = (lfo * ch.vib_depth as f32) / 32.0;
                let off_semis = off_units / SEMITONE_UNITS;
                semis += off_semis;
                if cur_tick > 0 {
                    ch.vib_pos = ch.vib_pos.wrapping_add(ch.vib_speed.wrapping_mul(4)) & 0x3F;
                }
            }

            // If the channel is actively playing, recompute the voice
            // frequency from the (possibly-modulated) semitone position.
            let inst_idx = ch.instrument.saturating_sub(1) as usize;
            if let Some(body) = self.samples.get(inst_idx) {
                if ch.voice.active && ch.cur_semis > 0.0 {
                    let new_freq = semis_to_freq(body.c3_hz as f32, semis);
                    if new_freq > 0.0 {
                        ch.voice.freq = new_freq;
                    }
                }
            }

            // Update the voice volume scalar (in case Axy / Exx modified
            // `ch.volume` this tick).
            ch.voice.volume = (ch.volume as f32 / 64.0) * (global_vol as f32 / 64.0);
        }
    }

    fn next_row(&mut self) {
        // Consume pending Bxy / Dxy.
        if let Some(order) = self.pending_order_jump.take() {
            self.order_index = order as usize;
            self.row = self.pending_break_row.take().unwrap_or(0);
            if self.order_index >= self.order.len() {
                self.ended = true;
            }
            return;
        }
        if let Some(row) = self.pending_break_row.take() {
            self.row = row;
            self.order_index += 1;
            if self.order_index >= self.order.len() {
                self.ended = true;
            }
            return;
        }

        self.row += 1;
        if (self.row as usize) >= PATTERN_ROWS {
            self.row = 0;
            self.order_index += 1;
            if self.order_index >= self.order.len() {
                self.ended = true;
            }
        }
    }

    /// Render interleaved stereo S16 PCM into `dst` (length must be
    /// even). STM uses hard-pan LRRL like MOD.
    pub fn render(&mut self, dst: &mut [i16]) -> usize {
        assert!(dst.len() % 2 == 0);
        let mut produced = 0usize;
        let total_frames = dst.len() / 2;
        let out_rate = self.sample_rate as f32;

        while produced < total_frames {
            if self.ended {
                break;
            }
            if self.tick_sample_cursor == 0 {
                self.advance_tick();
            }
            let spt = self.samples_per_tick().max(1);
            let remaining = spt.saturating_sub(self.tick_sample_cursor);
            let want = (total_frames - produced).min(remaining as usize);

            for _ in 0..want {
                let mut l = 0.0f32;
                let mut r = 0.0f32;
                for (i, ch) in self.channels.iter_mut().enumerate() {
                    let s = match (ch.instrument as usize).checked_sub(1) {
                        Some(idx) if idx < self.samples.len() => {
                            let body = &self.samples[idx];
                            ch.voice.render_one(body, out_rate)
                        }
                        _ => 0.0,
                    };
                    // Hard-pan LRRL (channels 0 & 3 → left).
                    if matches!(i % 4, 0 | 3) {
                        l += s;
                    } else {
                        r += s;
                    }
                }
                // Headroom scale for 4-channel STM → divide by 2.
                let l = (l / 2.0).clamp(-1.0, 1.0);
                let r = (r / 2.0).clamp(-1.0, 1.0);
                let off = produced * 2;
                dst[off] = (l * 32767.0) as i16;
                dst[off + 1] = (r * 32767.0) as i16;
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

/// Run tick-0 portion of effects: set-volume, Exy subcommands (fine
/// porta, fine volume slide, note cut/delay scheduling).
fn apply_tick0_effect(ch: &mut StmChannel) {
    let ep = ch.effect_param;
    let x = ep >> 4;
    let y = ep & 0x0F;
    match ch.effect {
        0xC => {
            // Cxx: set volume.
            ch.volume = ep.min(64);
        }
        0xE => {
            // Exy subcommands.
            match x {
                0x1
                    // E1x: fine porta up (tick 0 only) — shift
                    // semitone-position up by `y / SEMITONE_UNITS`.
                    if y != 0 => {
                        ch.cur_semis += y as f32 / SEMITONE_UNITS;
                    }
                0x2
                    // E2x: fine porta down.
                    if y != 0 => {
                        ch.cur_semis = (ch.cur_semis - y as f32 / SEMITONE_UNITS).max(0.0);
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
                    // ECx: note cut at tick x (0 = immediate).
                    if y == 0 {
                        ch.volume = 0;
                    } else {
                        ch.note_cut_tick = y;
                    }
                }
                0xD => {
                    // EDx: note delay — handled in enter_row by routing
                    // the trigger through `pending_*` + `note_delay_tick`.
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Run per-tick effects (ticks > 0): continuous pitch slides + volume
/// slides. Vibrato motion is applied in `advance_tick` alongside the
/// pitch recompute so it can stack with tone-porta.
fn apply_tickn_effect(ch: &mut StmChannel) {
    let effect = ch.effect;
    let ep = ch.effect_param;
    let x = ep >> 4;
    let y = ep & 0x0F;
    match effect {
        0x1 => {
            // 1xy: porta up.
            let p = if ep != 0 { ep } else { ch.porta_updown_mem };
            if p != 0 {
                ch.cur_semis += p as f32 / SEMITONE_UNITS;
            }
        }
        0x2 => {
            // 2xy: porta down.
            let p = if ep != 0 { ep } else { ch.porta_updown_mem };
            if p != 0 {
                ch.cur_semis = (ch.cur_semis - p as f32 / SEMITONE_UNITS).max(0.0);
            }
        }
        0x3 => {
            // 3xy: tone porta — glide toward target.
            let speed = (ch.porta_speed as f32) / SEMITONE_UNITS;
            if (ch.cur_semis - ch.porta_target_semis).abs() <= speed {
                ch.cur_semis = ch.porta_target_semis;
            } else if ch.cur_semis < ch.porta_target_semis {
                ch.cur_semis += speed;
            } else {
                ch.cur_semis -= speed;
            }
        }
        0x5 => {
            // 5xy: tone porta + volume slide.
            let speed = (ch.porta_speed as f32) / SEMITONE_UNITS;
            if (ch.cur_semis - ch.porta_target_semis).abs() <= speed {
                ch.cur_semis = ch.porta_target_semis;
            } else if ch.cur_semis < ch.porta_target_semis {
                ch.cur_semis += speed;
            } else {
                ch.cur_semis -= speed;
            }
            apply_vol_slide(ch, ch.vol_slide_mem);
        }
        0x6 => {
            // 6xy: vibrato + volume slide. Vibrato motion is in
            // advance_tick; the vol-slide piece is applied here.
            apply_vol_slide(ch, ch.vol_slide_mem);
        }
        0xA => {
            // Axy: volume slide.
            if x != 0 {
                ch.volume = (ch.volume as u16 + x as u16).min(64) as u8;
            } else if y != 0 {
                ch.volume = ch.volume.saturating_sub(y);
            }
        }
        _ => {}
    }
}

fn apply_vol_slide(ch: &mut StmChannel, mem: u8) {
    let hi = mem >> 4;
    let lo = mem & 0x0F;
    if hi != 0 {
        ch.volume = (ch.volume as u16 + hi as u16).min(64) as u8;
    } else if lo != 0 {
        ch.volume = ch.volume.saturating_sub(lo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stm::{extract_samples, parse_header, parse_patterns};

    /// Build a tiny STM file with a single note-on on row 0, channel 0.
    pub fn build_ping_stm() -> Vec<u8> {
        const HEADER_PREFIX: usize = 0x30;
        const ORDER_OFF: usize = 0x3D0;
        const ORDER_SIZE: usize = 64;
        const PATTERN_OFF: usize = 0x410;
        const BYTES_PER_PATTERN: usize = 64 * 4 * 4;
        let n_patterns = 1u8;
        let mut out = vec![0u8; PATTERN_OFF];
        out[0..4].copy_from_slice(b"ping");
        out[0x14..0x1C].copy_from_slice(b"!Scream!");
        out[0x1C] = 0x1A;
        out[0x1D] = 2;
        out[0x1E] = 2;
        out[0x20] = 0x60;
        out[0x21] = n_patterns;
        out[0x22] = 64;
        // Instrument 0: 64-byte sample, volume 64, C3 = 8363 Hz.
        let inst_off = HEADER_PREFIX;
        out[inst_off..inst_off + 3].copy_from_slice(b"snd");
        out[inst_off + 16..inst_off + 18].copy_from_slice(&64u16.to_le_bytes());
        out[inst_off + 22] = 64;
        out[inst_off + 24..inst_off + 26].copy_from_slice(&8363u16.to_le_bytes());
        // Order table: pattern 0, then 255-terminated.
        for i in 0..ORDER_SIZE {
            out[ORDER_OFF + i] = if i == 0 { 0 } else { 255 };
        }
        // Pattern 0: row 0 / ch 0 = note C-4 (octave 4, semitone 0), instrument 1.
        let mut pattern = vec![0u8; BYTES_PER_PATTERN];
        pattern[0] = 0x40; // octave 4, semitone 0
                           // vol_lo 0, instrument 1.
        pattern[1] = 1 << 3;
        pattern[2] = 0;
        pattern[3] = 0;
        out.extend(pattern);
        // 64-sample square wave body.
        for i in 0..64 {
            let v: i8 = if i < 32 { 100 } else { -100 };
            out.push(v as u8);
        }
        out
    }

    #[test]
    fn stm_player_emits_nonzero_audio() {
        let bytes = build_ping_stm();
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        // Render ~0.1s.
        let mut buf = vec![0i16; 4410 * 2];
        let produced = p.render(&mut buf);
        assert_eq!(produced, 4410);
        let nonzero = buf.iter().filter(|&&x| x != 0).count();
        assert!(nonzero > 100, "expected audible PCM, got {nonzero} nonzero");
    }

    #[test]
    fn note_to_semis_places_c3_at_36() {
        assert_eq!(note_to_semis(3, 0), 36.0);
        assert_eq!(note_to_semis(4, 0), 48.0);
        assert_eq!(note_to_semis(4, 7), 55.0);
    }

    #[test]
    fn semis_to_freq_round_trips_c3() {
        let f = semis_to_freq(8363.0, 36.0);
        assert!((f - 8363.0).abs() < 0.5);
        let f = semis_to_freq(8363.0, 48.0);
        assert!((f - 16726.0).abs() < 1.0);
    }
}
