//! Scream Tracker v1 (`.stm`) playback engine.
//!
//! Drives the shared [`crate::mixer::MixerVoice`] core over STM's
//! 4-channel pattern engine. STM uses per-instrument C3 frequencies
//! rather than Amiga periods (see [`crate::mixer::StmC3Pitch`]) and
//! ProTracker-like effect columns.
//!
//! Effects implemented (round 9 onward):
//!
//!  - 0xy: arpeggio — cycle the pitch through note / note+x / note+y
//!    half-steps across the row's ticks (`counter mod 3` per
//!    `Protracker-effects-MODFIL12.txt` 0:Arpeggio), then back to the
//!    note. Pure additive offset so porta / vibrato continue underneath.
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
//!  - 7xy: tremolo — sine LFO on output volume (clamped 0..=64); shares
//!    no memory with vibrato (separate `trem_speed` / `trem_depth`
//!    nibble registers per the canonical "if either xxxx or yyyy are 0,
//!    then values from the most recent prior tremolo will be used"
//!    rule in `Protracker-effects-MODFIL12.txt` 7:Tremolo).
//!  - Exy subcommands: E1x / E2x fine porta, EA x / EB x fine volume
//!    slide, EC x note cut, ED x note delay, E9x retrig note (replay
//!    the active sample every `y` ticks within the row).
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
    /// 9xx sample-offset parameter memory. Per the canonical PT reading
    /// in `Protracker-effects-MODFIL12.txt` 9:Set-sample-offset
    /// ("9xx has its own memory. 900 plays the sample at
    /// 9xx_memory*0x100"), a `900` row reuses the last non-zero `xx`
    /// instead of being inert. The memory persists until a fresh
    /// non-zero `9xx` overwrites it; a fresh non-zero `9xx` also
    /// latches the value here so a later `900` continuation reuses it.
    pub mem_sample_offset: u8,

    // -------- tremolo (7xy) state --------
    /// Tremolo sine-table position 0..=63. Walks like `vib_pos` but on
    /// a separate register so 4xy vibrato and 7xy tremolo on the same
    /// channel don't share phase.
    pub trem_pos: u8,
    /// Tremolo speed memory (last non-zero 7xy `x` nibble). PT semantics
    /// per `Protracker-effects-MODFIL12.txt` 7:Tremolo say the per-nibble
    /// memory operates independently from vibrato's.
    pub trem_speed: u8,
    /// Tremolo depth memory (last non-zero 7xy `y` nibble).
    pub trem_depth: u8,

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

    // -------- pattern loop (E6x) --------
    /// Loop-start row recorded by the most recent `E60` on this
    /// channel. Defaults to row 0 per
    /// `Protracker-effects-MODFIL12.txt` E6 ("If no start point was
    /// specified in the current pattern being played, the loop start
    /// defaults to the first line in the pattern").
    pub pat_loop_row: u8,
    /// Remaining loop iterations after the first `E6y` is encountered.
    /// Tracked per channel so two voices can drive independent loops
    /// without trampling each other's counters.
    pub pat_loop_count: u8,

    // -------- retrigger (E9x) --------
    /// `E9x` retrig period in ticks. Captured on row entry from the
    /// `y` nibble of an `E9y` cell; consumed by the per-tick handler
    /// which replays the sample whenever `tick % retrig_ticks == 0`.
    /// Zero (the default and the explicit `E90` nibble) disables the
    /// effect — per `Protracker-effects-MODFIL12.txt` E9, a zero
    /// period is documented as "every yyyy ticks" with `yyyy = 0`
    /// being the inert / no-op selection.
    pub retrig_ticks: u8,
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
    /// Pending pattern-loop row (`E6x`): when set, `next_row` rewinds
    /// to this row within the *current* pattern without re-entering
    /// `enter_row` past the rewind. Per the PT spec the loop runs
    /// inside one pattern, so the order index is preserved.
    pub pending_pat_loop_row: Option<u8>,

    /// `EEx` pattern-delay counter: number of additional times the
    /// current row should be repeated before genuinely advancing.
    /// Per `Protracker-effects-MODFIL12.txt` EE the row's notes and
    /// effects must continue undisturbed across the delay slices, so
    /// the repeat must NOT re-fire `enter_row`. Per-tick effects keep
    /// running so vibrato / volume slides / arpeggio still animate.
    pub pattern_delay: u8,
    /// True while a row is in its EE-induced repeat phase. Suppresses
    /// `enter_row` re-execution on tick 0 of the repeated pass — see
    /// the MOD player's identically-named flag for the same reason
    /// (re-entering would reset `vib_pos`, retrigger held notes, and
    /// compound fine-volume slides once per delay slice).
    pub in_pattern_delay_repeat: bool,
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
            pending_pat_loop_row: None,
            pattern_delay: 0,
            in_pattern_delay_repeat: false,
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
            // E9x retrig period is row-scoped — clear here so a row
            // without `E9y` cannot inherit the previous row's period.
            // The tick-0 effect handler below repopulates it from the
            // `y` nibble when `E9y` is present.
            ch.retrig_ticks = 0;

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
                0x7 => {
                    // 7xy: tremolo — per-nibble memory (`x` speed,
                    // `y` depth) independent of vibrato's. PT spec
                    // §7:Tremolo: "If either xxxx or yyyy are 0, then
                    // values from the most recent prior tremolo will
                    // be used."
                    let tx = ep >> 4;
                    let ty = ep & 0x0F;
                    if tx != 0 {
                        ch.trem_speed = tx;
                    }
                    if ty != 0 {
                        ch.trem_depth = ty;
                    }
                }
                0x5 | 0x6 | 0xA if ep != 0 => {
                    ch.vol_slide_mem = ep;
                }
                0x9 if ep != 0 => {
                    // 9xx: latch a fresh non-zero offset parameter so a
                    // subsequent `900` row can reuse it. The actual
                    // cursor placement runs in the note-trigger arm
                    // below, mirroring the MOD player's "offset only
                    // applies on a note-on" semantics from
                    // `Protracker-effects-MODFIL12.txt`
                    // 9:Set-sample-offset.
                    ch.mem_sample_offset = ep;
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
                                // 9xx: resolve the cursor offset for this
                                // trigger. A non-zero `xx` was already
                                // latched into `mem_sample_offset` in the
                                // memory pass above; a `900` row reuses
                                // the latched value. The offset is in
                                // units of 0x100 = 256 bytes per the
                                // corrected reading in
                                // `Protracker-effects-MODFIL12.txt`
                                // 9:Set-sample-offset ("Don't multiply
                                // by 512. You should multiply by 256").
                                let offset_frames: u32 = if cell.command == 0x9 {
                                    (ch.mem_sample_offset as u32) * 0x100
                                } else {
                                    0
                                };

                                // Out-of-range quirk: a 9xx that lands
                                // at or past the end of the sample body
                                // plays NO NOTE at all on this channel
                                // (same line in the spec — "if the
                                // effect is out of range (e.g. if it
                                // tries to jump beyond the end of the
                                // sample) NO NOTE WILL BE PLAYED!").
                                // The note metadata is still latched so
                                // subsequent porta / arpeggio anchor to
                                // the intended pitch.
                                let sample_len = body.pcm.len();
                                if cell.command == 0x9 && offset_frames as usize >= sample_len {
                                    ch.voice.active = false;
                                    ch.voice.pos = 0.0;
                                    ch.cur_semis = target;
                                    ch.porta_target_semis = target;
                                    continue;
                                }

                                let pitch = StmC3Pitch {
                                    c3_hz: body.c3_hz as f32,
                                };
                                let freq = pitch.note_to_freq(ch.note);
                                let vol =
                                    (ch.volume as f32 / 64.0) * (self.global_volume as f32 / 64.0);
                                ch.voice.trigger(freq, vol);
                                // Apply the 9xx offset after `trigger`
                                // (which would otherwise reset the
                                // cursor to 0).
                                if cell.command == 0x9 {
                                    ch.voice.pos = offset_frames as f32;
                                }
                                ch.cur_semis = target;
                                ch.porta_target_semis = target;
                                // Fresh note resets vibrato + tremolo
                                // phases (PT canonical retrigger:
                                // `Protracker-effects-MODFIL12.txt`
                                // E7:Set-Tremolo-Waveform implies the
                                // default waveform retriggers on a new
                                // note unless E7x sets the no-retrig
                                // bit, which STM's pre-FT2 effect set
                                // doesn't expose).
                                ch.vib_pos = 0;
                                ch.trem_pos = 0;
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

        // Row-level song-state effects: Bxy / Dxy / Fxx / E6x / EEx.
        //
        // E6x (pattern loop) is technically per-channel (it lives in a
        // channel's row cell) but the *jump target* it schedules is
        // song-level, so we resolve it in the same pass as Bxy / Dxy.
        // Per `Protracker-effects-MODFIL12.txt` E6: a non-zero param
        // schedules a rewind to the channel's previously-recorded
        // start row, decrementing its iteration counter on each visit.
        // EEx (pattern delay) writes the player-level `pattern_delay`
        // counter; the repeat suppression then prevents `enter_row`
        // from re-firing on the looped pass (see `advance_tick`).
        let cur_row = self.row;
        for ch_idx in 0..STM_CHANNELS {
            let ch = &mut self.channels[ch_idx];
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
                0xE => {
                    let x = ch.effect_param >> 4;
                    let y = ch.effect_param & 0x0F;
                    match x {
                        0x6 => {
                            // E6x — Pattern loop (per-channel).
                            //
                            // y == 0: record the current row as this
                            //   channel's loop-start.
                            // y > 0 (first visit): seed the iteration
                            //   counter with `y` and schedule a rewind
                            //   to the channel's loop-start.
                            // y > 0 (subsequent visits): decrement the
                            //   counter; if still > 0, schedule another
                            //   rewind; if exhausted, fall through.
                            //
                            // Spec source:
                            // `Protracker-effects-MODFIL12.txt` E6
                            // ("If yyyy=0 … specifies the loop's start
                            // point. Otherwise, it specifies the number
                            // of times to play this line and the
                            // preceding lines from the start point").
                            if y == 0 {
                                ch.pat_loop_row = cur_row;
                            } else if ch.pat_loop_count == 0 {
                                ch.pat_loop_count = y;
                                self.pending_pat_loop_row = Some(ch.pat_loop_row);
                            } else {
                                ch.pat_loop_count -= 1;
                                if ch.pat_loop_count > 0 {
                                    self.pending_pat_loop_row = Some(ch.pat_loop_row);
                                }
                            }
                        }
                        0xE => {
                            // EEx — Pattern delay. Writes the song-level
                            // `pattern_delay` counter; the row repeats
                            // that many additional times in `next_row`.
                            // FT2 / PT semantics honour the last EEx
                            // encountered on the row (channel order
                            // does the resolution), so we just
                            // overwrite — matching the MOD player's
                            // last-channel-wins behaviour.
                            self.pattern_delay = y;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    fn advance_tick(&mut self) {
        if self.tick == 0 {
            // EE-induced repeat passes must NOT re-enter the row, or
            // we would retrigger held notes (resetting `voice.pos`) and
            // re-fire tick-0 effects (compounding fine-volume slides).
            // Per `Protracker-effects-MODFIL12.txt` EE the original
            // row's notes and effects continue across the delay, so the
            // tick-0 channel state is preserved from the first pass.
            // Per-tick effects still run via the `else` branch on
            // subsequent ticks of the repeated pass.
            if !self.in_pattern_delay_repeat {
                self.enter_row();
            }
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

            // E9x retrig: replay the active voice every `retrig_ticks`
            // ticks within the current row. Tick 0 is the row's own
            // initial trigger, so the modulo schedule kicks in from
            // tick 1 onward. Per `Protracker-effects-MODFIL12.txt` E9
            // ("re-trigger a specified sample … after yyyy ticks
            // during the line") the voice's sample cursor and phase
            // reset on each retrigger; the channel volume baseline,
            // pitch state, and instrument selection are untouched (the
            // effect is a cursor-rewind, not a fresh note-on with the
            // accompanying envelope / sample-swap path).
            if ch.retrig_ticks > 0
                && cur_tick > 0
                && cur_tick % ch.retrig_ticks == 0
                && ch.voice.active
            {
                ch.voice.pos = 0.0;
                // Fresh phase on each retrigger so the vibrato / tremolo
                // LFOs realign with the new attack — matches the
                // canonical PT retrigger behaviour (no-retrig waveform
                // flags don't exist in STM, so this is unconditional).
                ch.vib_pos = 0;
                ch.trem_pos = 0;
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
                    ch.trem_pos = 0;
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

            // Arpeggio (0xy): with at least one non-zero nibble, cycle the
            // pitch through note / note+x / note+y half-steps across the
            // ticks of the row, evenly spaced, then back to the note.
            // STM uses ProTracker-format effects, so we follow the
            // `Protracker-effects-MODFIL12.txt` 0:Arpeggio algorithm
            // ("if (counter mod 3) = 0/1/2 then play note / note+x /
            // note+y"). The offset is a pure addition to the live
            // semitone position so porta / vibrato continue underneath
            // and `cur_semis` is left unmodified (no permanent drift).
            if ch.effect == 0x0 && ch.effect_param != 0 {
                let arp_x = (ch.effect_param >> 4) as f32;
                let arp_y = (ch.effect_param & 0x0F) as f32;
                semis += match cur_tick % 3 {
                    0 => 0.0,
                    1 => arp_x,
                    _ => arp_y,
                };
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

            // Tremolo (7xy): oscillate the *output* volume with a sine
            // LFO. Per `Protracker-effects-MODFIL12.txt` 7:Tremolo, the
            // peak amplitude is `depth * (speed - 1)` volume units, and
            // the effect is "Like vibrato, except we modify the output
            // volume" per `multimedia-cx-protracker.html` 7xy with the
            // result clamped to `0 <= vol <= 64`. We mirror the STM
            // vibrato divisor (`/ 32`) so depth 15 yields a peak swing
            // of ~60 volume units (full-range at the strongest setting)
            // — the same ratio MOD's `tremolo_offset` produces via its
            // `>> 6` shift on the 32-entry table. The offset is computed
            // first (before the volume scalar update) and added to the
            // base `ch.volume` so Axy / Cxx etc. set the baseline that
            // tremolo modulates around (per the "stored volume isn't
            // modified by this effect" reading shared by the S3M Ixy
            // doc, which is the same family of effects).
            let trem_active = ch.effect == 0x7 && ch.trem_depth > 0;
            let trem_off_units: f32 = if trem_active {
                let lfo = SINE_TABLE[(ch.trem_pos & 0x3F) as usize] as f32;
                // Same shape as STM vibrato: (lfo * depth) / 32 → peak
                // ≈ depth * 4 volume units. At depth 15 → ~±60 units.
                let units = (lfo * ch.trem_depth as f32) / 32.0;
                if cur_tick > 0 {
                    // Walk the sine table at `speed * 4` (matches the
                    // STM vibrato stepping and gives the same per-line
                    // oscillation rate the 4xy / 7xy parameters share
                    // by spec construction).
                    ch.trem_pos = ch.trem_pos.wrapping_add(ch.trem_speed.wrapping_mul(4)) & 0x3F;
                }
                units
            } else {
                0.0
            };

            // Update the voice volume scalar (in case Axy / Exx modified
            // `ch.volume` this tick). Tremolo adds an offset, then the
            // result is clamped to [0, 64] per the PT spec before the
            // global-volume scale is folded in.
            let modulated = (ch.volume as f32 + trem_off_units).clamp(0.0, 64.0);
            ch.voice.volume = (modulated / 64.0) * (global_vol as f32 / 64.0);
        }
    }

    fn next_row(&mut self) {
        // EEx pattern-delay repeat. If `pattern_delay > 0`, hold the
        // current row in place and decrement. The `in_pattern_delay_repeat`
        // flag is what `advance_tick` reads on the next tick-0 to skip
        // `enter_row`. Per PT semantics (see the MOD player's identical
        // handling): EE counts *additional* row repeats, so the first
        // repeat returns here with delay = original value, and we
        // exhaust it before any other row-advance logic.
        if self.pattern_delay > 0 {
            self.pattern_delay -= 1;
            self.in_pattern_delay_repeat = true;
            return;
        }
        // Real row advance — clear the repeat flag so the next row's
        // tick-0 processing runs normally.
        self.in_pattern_delay_repeat = false;

        // Consume pending Bxy / Dxy first (channel jumps trump E6x in
        // the rare case of both on the same row — Bxy/Dxy land us in
        // a different pattern, so the loop start row from the previous
        // pattern is meaningless after the jump).
        if let Some(order) = self.pending_order_jump.take() {
            // Clear the pat-loop pending state — a song jump invalidates
            // the channel-local loop-start (which is per-pattern).
            self.pending_pat_loop_row = None;
            self.order_index = order as usize;
            self.row = self.pending_break_row.take().unwrap_or(0);
            if self.order_index >= self.order.len() {
                self.ended = true;
            }
            return;
        }
        if let Some(row) = self.pending_break_row.take() {
            self.pending_pat_loop_row = None;
            self.row = row;
            self.order_index += 1;
            if self.order_index >= self.order.len() {
                self.ended = true;
            }
            return;
        }

        // E6x pattern-loop rewind. Stays in the current pattern; just
        // rolls the row index back to the recorded start. Per spec the
        // loop body runs within a single pattern, so the order index
        // is preserved.
        if let Some(row) = self.pending_pat_loop_row.take() {
            self.row = row;
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
                0x9 => {
                    // E9x: retrig note — record the period; the
                    // per-tick handler in `advance_tick` replays the
                    // active sample whenever `tick % retrig_ticks == 0`
                    // (excluding tick 0, which is the row's initial
                    // trigger). Per `Protracker-effects-MODFIL12.txt`
                    // E9 ("re-trigger a specified sample at a
                    // particular note after yyyy ticks during the
                    // line"), a zero `y` nibble is inert. STM declares
                    // its effect column as "in ProTracker format" per
                    // `docs/audio/trackers/stm/ScreamTracker-v1.0-stm.txt`,
                    // so the PT semantics carry across verbatim.
                    ch.retrig_ticks = y;
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
pub mod tests {
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

    /// Build an STM with a C-4 note on row 0 plus arpeggio `037`
    /// (effect 0, x=3, y=7) on channel 0.
    fn build_arpeggio_stm() -> Vec<u8> {
        let mut out = build_ping_stm();
        const PATTERN_OFF: usize = 0x410;
        // Cell byte 2 = (vol_hi << 4) | command; command 0 = arpeggio.
        out[PATTERN_OFF + 2] = 0x00;
        // Cell byte 3 = command param 0x37 (x=3 half-steps, y=7).
        out[PATTERN_OFF + 3] = 0x37;
        out
    }

    #[test]
    fn arpeggio_cycles_note_x_y_half_steps() {
        let bytes = build_arpeggio_stm();
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        // Base note C-4 (48 semis from C0) at c3_hz=8363 →
        //   8363 * 2^((48-36)/12) = 8363 * 2 = 16726 Hz.
        let base = semis_to_freq(8363.0, 48.0);
        assert!((base - 16726.0).abs() < 1.0, "base freq = {base}");

        // Step the engine tick by tick (advance_tick recomputes
        // ch.voice.freq each tick). On the row's ticks the arpeggio
        // walks 0 / +3 / +7 / 0 / +3 / +7 half-steps via `tick % 3`.
        let expected = |semis_off: f32| base * 2.0f32.powf(semis_off / 12.0);
        let cases = [0.0f32, 3.0, 7.0, 0.0, 3.0, 7.0];
        for (tick, &off) in cases.iter().enumerate() {
            p.tick = tick as u8;
            p.advance_tick();
            let got = p.channels[0].voice.freq;
            let want = expected(off);
            assert!(
                (got - want).abs() < 1.0,
                "tick {tick}: arpeggio +{off} semis: got {got}, want {want}"
            );
        }
    }

    #[test]
    fn arpeggio_zero_param_is_inert() {
        // Effect 0 with a zero parameter is NOT an arpeggio (per
        // `Protracker-effects-MODFIL12.txt`: "only an arpeggio if there
        // is at least one non-zero argument"). The pitch must stay on
        // the base note across all ticks.
        let mut bytes = build_ping_stm();
        const PATTERN_OFF: usize = 0x410;
        bytes[PATTERN_OFF + 2] = 0x00; // command 0
        bytes[PATTERN_OFF + 3] = 0x00; // zero param → no arpeggio
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        let base = semis_to_freq(8363.0, 48.0);
        for tick in 0u8..4 {
            p.tick = tick;
            p.advance_tick();
            let got = p.channels[0].voice.freq;
            assert!(
                (got - base).abs() < 1.0,
                "tick {tick}: zero-param effect-0 must hold base {base}, got {got}"
            );
        }
    }

    /// Build an STM with a C-4 note on row 0 plus tremolo `7xy`
    /// (effect 7, x=speed nibble, y=depth nibble) on channel 0.
    /// `vol` (1..=64) sets the cell's starting volume so we have
    /// headroom for tremolo to swing both above and below.
    fn build_tremolo_stm(speed: u8, depth: u8, vol: u8) -> Vec<u8> {
        let mut out = build_ping_stm();
        const PATTERN_OFF: usize = 0x410;
        // Cell volume override (STM encodes vol bits as bit 0..2 of
        // byte 1 (low bit) + bits 4..6 of byte 2 (upper bits); vol-1 is
        // stored — see `StmCell::volume` decoding). For simplicity,
        // patch the existing ping STM's volume to the requested value:
        // bit 0..2 of byte 1 (instrument is in bits 3..7, value 1).
        let v = vol & 0x3F;
        out[PATTERN_OFF + 1] = (1 << 3) | (v & 0x07);
        out[PATTERN_OFF + 2] = ((v >> 3) << 4) | 0x07; // upper-vol nibble + effect 7
        out[PATTERN_OFF + 3] = (speed << 4) | (depth & 0x0F);
        out
    }

    #[test]
    fn tremolo_modulates_volume_symmetrically() {
        // Tremolo with mid-range volume (32/64) has +32 headroom up and
        // -32 down so we can see swings both directions. Use a moderate
        // speed (4) and large depth (15) → peak swing ≈ ±60 vol units,
        // clamped to [0, 64].
        let bytes = build_tremolo_stm(4, 15, 32);
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        // Walk many ticks to cover at least one full sine cycle.
        let mut min_vol = f32::INFINITY;
        let mut max_vol = f32::NEG_INFINITY;
        for tick in 0u8..32 {
            p.tick = tick;
            p.advance_tick();
            let v = p.channels[0].voice.volume;
            if v < min_vol {
                min_vol = v;
            }
            if v > max_vol {
                max_vol = v;
            }
        }

        // Baseline (no tremolo) would be (32 / 64) * (global / 64).
        // Global is 64 from build_ping_stm. So baseline = 0.5.
        // Depth 15 + STM scaler 4 gives peak ≈ ±60 vol units, clamped
        // into [0, 64]. The sine LFO with `speed * 4 = 16` steps per
        // tick covers a full cycle in 4 ticks, so within 32 ticks we
        // should see both the lower clamp (0.0) and a high value near
        // (64 / 64) = 1.0 scaled by global = 1.0.
        assert!(
            min_vol < 0.4,
            "tremolo should swing volume below baseline 0.5: min = {min_vol}"
        );
        assert!(
            max_vol > 0.6,
            "tremolo should swing volume above baseline 0.5: max = {max_vol}"
        );
    }

    #[test]
    fn tremolo_zero_param_uses_memory() {
        // First a 7xy with non-zero nibbles to seed memory, then a 700
        // continuation must keep modulating using the stored values.
        let mut bytes = build_tremolo_stm(4, 8, 32);
        // Patch row 1 channel 0 to be effect 7 with param 0 (memory).
        const PATTERN_OFF: usize = 0x410;
        const BYTES_PER_ROW: usize = 4 * 4;
        let row1 = PATTERN_OFF + BYTES_PER_ROW;
        // Note: 253 (Dots) so no retrigger / no fresh phase reset.
        bytes[row1] = 253;
        bytes[row1 + 1] = 0;
        bytes[row1 + 2] = 0x07;
        bytes[row1 + 3] = 0x00; // 700 → reuse memory
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        // Walk row 0 to seed memory (speed=4, depth=8).
        for tick in 0u8..6 {
            p.tick = tick;
            p.advance_tick();
        }
        // Now move to row 1 (which uses 700 — memory) and confirm
        // the tremolo offset is non-zero on at least one tick.
        p.next_row();
        let mut saw_swing = false;
        for tick in 0u8..6 {
            p.tick = tick;
            p.advance_tick();
            let v = p.channels[0].voice.volume;
            // Baseline would be 32/64 = 0.5. Tremolo with depth 8 must
            // move it off that value on at least one tick.
            if (v - 0.5).abs() > 0.05 {
                saw_swing = true;
            }
        }
        assert!(
            saw_swing,
            "700 must reuse last non-zero 7xy params (depth/speed memory)"
        );
    }

    #[test]
    fn tremolo_inert_at_zero_depth() {
        // 700 on a row with no prior 7xy seed → memory still zero →
        // volume must hold the baseline across all ticks.
        let bytes = build_tremolo_stm(0, 0, 32);
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        for tick in 0u8..6 {
            p.tick = tick;
            p.advance_tick();
            let v = p.channels[0].voice.volume;
            // Baseline = 32/64 * 64/64 = 0.5 exactly.
            assert!(
                (v - 0.5).abs() < 1e-4,
                "tick {tick}: 700 with empty memory must leave volume unmodulated, got {v}"
            );
        }
    }

    /// Place a no-note effect cell at (row, ch) of pattern 0 by
    /// patching the existing `build_ping_stm` buffer. Uses note byte
    /// 253 ("Dots") so no retrigger happens. `cmd` is the 4-bit
    /// effect command, `param` is the byte parameter.
    fn poke_effect(bytes: &mut [u8], row: u8, ch: u8, cmd: u8, param: u8) {
        const PATTERN_OFF: usize = 0x410;
        const BYTES_PER_ROW: usize = 4 * 4;
        let off = PATTERN_OFF + (row as usize) * BYTES_PER_ROW + (ch as usize) * 4;
        bytes[off] = 253;
        bytes[off + 1] = 0;
        bytes[off + 2] = cmd & 0x0F;
        bytes[off + 3] = param;
    }

    /// Drive the player one full row's worth of ticks at the current
    /// speed (mirrors the natural `render`-loop tick cadence).
    fn step_one_row(p: &mut StmPlayerState) {
        for t in 0..p.speed {
            p.tick = t;
            p.advance_tick();
        }
        p.tick = 0;
        p.next_row();
    }

    #[test]
    fn e6x_pattern_loop_rewinds_to_recorded_start_row() {
        // Row 0: note + E60 (record loop start = row 0).
        // Row 1: empty.
        // Row 2: E62 (loop 2 extra times back to row 0).
        // Row 3: empty.
        //
        // After visiting row 2 we expect: row 0 → 1 → 2 (rewind) →
        //  0 → 1 → 2 (rewind) → 0 → 1 → 2 → 3 (exhausted).
        let mut bytes = build_ping_stm();
        // E60 on row 0 (sets loop start). Channel 0 already carries
        // the C-4 trigger; piggy-back the effect bytes there.
        const PATTERN_OFF: usize = 0x410;
        bytes[PATTERN_OFF + 2] = 0x0E; // command 0xE
        bytes[PATTERN_OFF + 3] = 0x60; // sub-effect 6, value 0
        poke_effect(&mut bytes, 2, 0, 0xE, 0x62);
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        let mut visited = Vec::new();
        for _ in 0..16 {
            if p.ended {
                break;
            }
            visited.push(p.row);
            step_one_row(&mut p);
        }
        // Expected first 10 rows visited: 0 1 2 0 1 2 0 1 2 3.
        let want: Vec<u8> = vec![0, 1, 2, 0, 1, 2, 0, 1, 2, 3];
        assert_eq!(
            &visited[..want.len()],
            &want[..],
            "E6x loop didn't rewind correctly: visited = {visited:?}"
        );
    }

    #[test]
    fn e60_without_followup_does_not_loop() {
        // E60 alone is just a start-marker; without an E6y (y>0) on
        // the same channel later in the pattern there is no rewind,
        // so playback should march straight through.
        let mut bytes = build_ping_stm();
        const PATTERN_OFF: usize = 0x410;
        bytes[PATTERN_OFF + 2] = 0x0E;
        bytes[PATTERN_OFF + 3] = 0x60;
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        let mut visited = Vec::new();
        for _ in 0..6 {
            if p.ended {
                break;
            }
            visited.push(p.row);
            step_one_row(&mut p);
        }
        assert_eq!(
            visited,
            vec![0u8, 1, 2, 3, 4, 5],
            "E60 alone must not rewind"
        );
    }

    #[test]
    fn e6x_loop_state_is_per_channel() {
        // Two channels declare *different* loop starts on the same
        // pattern. The PT spec attaches the start row + counter to a
        // channel, so two voices can drive independent loops. We test
        // the recorded start by triggering channel 1's `E6y` (with
        // y>0) while channel 0 has no loop-start recorded — the
        // rewind must land on the row channel 1 set, not row 0.
        let mut bytes = build_ping_stm();
        // Channel 1 row 2: E60 (loop start = row 2).
        poke_effect(&mut bytes, 2, 1, 0xE, 0x60);
        // Channel 1 row 4: E61 (loop once back to row 2).
        poke_effect(&mut bytes, 4, 1, 0xE, 0x61);
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        let mut visited = Vec::new();
        for _ in 0..12 {
            if p.ended {
                break;
            }
            visited.push(p.row);
            step_one_row(&mut p);
        }
        // Rows: 0 1 2 3 4 (rewind to 2) 2 3 4 5 ...
        assert_eq!(&visited[..9], &[0u8, 1, 2, 3, 4, 2, 3, 4, 5]);
        // Channel 0 must NOT see a counter — only channel 1's loop ran.
        assert_eq!(p.channels[0].pat_loop_count, 0);
    }

    #[test]
    fn eex_pattern_delay_repeats_row_without_retriggering() {
        // Row 0: note + EE2 (repeat row 0 two more times).
        // Row 1: another note.
        //
        // The voice triggers exactly once across the three passes of
        // row 0 — `voice.pos` advances monotonically and never resets.
        let mut bytes = build_ping_stm();
        const PATTERN_OFF: usize = 0x410;
        bytes[PATTERN_OFF + 2] = 0x0E;
        bytes[PATTERN_OFF + 3] = 0xE2; // EE2 = pattern delay 2
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        // Drive row 0 once (normal pass + 2 EE repeats = 3 row-equivalents).
        let mut row0_count = 0;
        for _ in 0..6 {
            if p.row == 0 {
                row0_count += 1;
            } else {
                break;
            }
            let pos_before = p.channels[0].voice.pos;
            step_one_row(&mut p);
            let pos_after = p.channels[0].voice.pos;
            // pos must move forward (no retrigger reset to 0.0).
            // On the very first row a fresh trigger sets pos = 0.0,
            // so the *first* pass we accept pos_after > 0. On the
            // EE-repeat passes pos must be strictly > the prior
            // pos_after at the start of that pass — no retrigger
            // would have moved it back.
            assert!(
                pos_after >= pos_before,
                "EE-delay repeat must not retrigger the voice (pos moved {pos_before} → {pos_after})"
            );
        }
        // Original row plus 2 EE-induced repeats = 3 visits.
        assert_eq!(
            row0_count, 3,
            "EE2 must hold row 0 for 3 visits total, got {row0_count}"
        );
    }

    #[test]
    fn eex_zero_param_is_inert() {
        // EE0 must not stall the row.
        let mut bytes = build_ping_stm();
        const PATTERN_OFF: usize = 0x410;
        bytes[PATTERN_OFF + 2] = 0x0E;
        bytes[PATTERN_OFF + 3] = 0xE0;
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        let mut visited = Vec::new();
        for _ in 0..4 {
            if p.ended {
                break;
            }
            visited.push(p.row);
            step_one_row(&mut p);
        }
        assert_eq!(visited, vec![0u8, 1, 2, 3], "EE0 must not delay");
    }

    #[test]
    fn eex_and_e6x_compose_predictably() {
        // Row 0: note + E60 (loop-start).
        // Row 1: EE1 (delay this row by 1 extra pass).
        // Row 2: E61 (loop once back to row 0).
        //
        // Expected visits: 0 1 1 (EE repeat) 2 (rewind) 0 1 1 2 3 ...
        let mut bytes = build_ping_stm();
        const PATTERN_OFF: usize = 0x410;
        bytes[PATTERN_OFF + 2] = 0x0E;
        bytes[PATTERN_OFF + 3] = 0x60; // E60
        poke_effect(&mut bytes, 1, 0, 0xE, 0xE1); // EE1 row 1
        poke_effect(&mut bytes, 2, 0, 0xE, 0x61); // E61 row 2
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        let mut visited = Vec::new();
        for _ in 0..14 {
            if p.ended {
                break;
            }
            visited.push(p.row);
            step_one_row(&mut p);
        }
        // EE repeats hold *this* row in place, so row 1 is visited
        // twice in a row; then E61 on row 2 rewinds to row 0; after
        // exhausting the counter we advance past row 2.
        // Sequence: 0 1 1 2 0 1 1 2 3 ...
        assert_eq!(
            &visited[..9],
            &[0u8, 1, 1, 2, 0, 1, 1, 2, 3],
            "EE + E6 interaction wrong: {visited:?}"
        );
    }

    /// Build a fresh STM with a longer sample body so we can test
    /// 9xx sample-offset landings both inside and past the sample.
    /// `pcm_len` chooses the sample size (signed-8-bit values, even
    /// indices at +100, odd at -100 so any forward-cursor jump lands
    /// on audible data). `offset_param` is the row-0 9xx parameter.
    fn build_offset_stm(pcm_len: usize, offset_param: u8) -> Vec<u8> {
        const HEADER_PREFIX: usize = 0x30;
        const ORDER_OFF: usize = 0x3D0;
        const ORDER_SIZE: usize = 64;
        const PATTERN_OFF: usize = 0x410;
        const BYTES_PER_PATTERN: usize = 64 * 4 * 4;
        let n_patterns = 1u8;
        let mut out = vec![0u8; PATTERN_OFF];
        out[0..4].copy_from_slice(b"poff");
        out[0x14..0x1C].copy_from_slice(b"!Scream!");
        out[0x1C] = 0x1A;
        out[0x1D] = 2;
        out[0x1E] = 2;
        out[0x20] = 0x60;
        out[0x21] = n_patterns;
        out[0x22] = 64;
        let inst_off = HEADER_PREFIX;
        out[inst_off..inst_off + 3].copy_from_slice(b"snd");
        out[inst_off + 16..inst_off + 18].copy_from_slice(&(pcm_len as u16).to_le_bytes());
        out[inst_off + 22] = 64;
        out[inst_off + 24..inst_off + 26].copy_from_slice(&8363u16.to_le_bytes());
        for i in 0..ORDER_SIZE {
            out[ORDER_OFF + i] = if i == 0 { 0 } else { 255 };
        }
        // Pattern 0, row 0, ch 0: note C-4, instrument 1, effect 0x9,
        // param `offset_param`.
        let mut pattern = vec![0u8; BYTES_PER_PATTERN];
        pattern[0] = 0x40; // note byte: octave 4, semitone 0
        pattern[1] = 1 << 3; // instrument 1, vol_lo = 0
        pattern[2] = 0x09; // vol_hi 0, command 9
        pattern[3] = offset_param;
        out.extend(pattern);
        for i in 0..pcm_len {
            let v: i8 = if i % 2 == 0 { 100 } else { -100 };
            out.push(v as u8);
        }
        out
    }

    #[test]
    fn nine_xx_starts_sample_at_param_times_0x100() {
        // 9xx places the playback cursor at `xx * 0x100` bytes (per
        // `Protracker-effects-MODFIL12.txt` 9:Set-sample-offset, with
        // the corrected "multiply by 256" note). A 1024-byte sample +
        // a 9x02 row should land at byte 0x200 = 512.
        let bytes = build_offset_stm(1024, 0x02);
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        // enter_row runs at tick 0 of advance_tick.
        p.tick = 0;
        p.advance_tick();
        let pos = p.channels[0].voice.pos;
        assert!(
            p.channels[0].voice.active,
            "9x02 inside-range must still trigger the voice"
        );
        // pos is set before the first render_one advances it, so on
        // the row's tick 0 the cursor sits exactly on the offset.
        // (subsequent ticks advance by freq/sample_rate per `render`.)
        assert!(
            (pos - 512.0).abs() < 1.0,
            "9x02 must place cursor at 512.0 frames, got {pos}"
        );
    }

    #[test]
    fn nine_xx_param_zero_reuses_memory() {
        // 9xx with a non-zero param latches the value; a later `900`
        // continuation must reuse the latched value (per the canonical
        // "9xx has its own memory. 900 plays the sample at
        // 9xx_memory*0x100" reading).
        // Row 0: note + 9x04 → seed memory to 4. (offset 1024)
        // Row 1: note + 900  → reuse 4. (also 1024)
        // Use a 1536-byte sample so the 1024-byte landing is in-range.
        let mut bytes = build_offset_stm(1536, 0x04);
        // Patch row 1 ch 0 to be note C-4 + instrument 1 + 900.
        const PATTERN_OFF: usize = 0x410;
        const BYTES_PER_ROW: usize = 4 * 4;
        let row1 = PATTERN_OFF + BYTES_PER_ROW;
        bytes[row1] = 0x40; // C-4
        bytes[row1 + 1] = 1 << 3; // instrument 1
        bytes[row1 + 2] = 0x09; // command 9
        bytes[row1 + 3] = 0x00; // 900 → reuse memory
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        // Row 0 / tick 0: 9x04 seeds memory to 4 and places cursor at
        // 0x400 = 1024.
        p.tick = 0;
        p.advance_tick();
        assert_eq!(
            p.channels[0].mem_sample_offset, 0x04,
            "9x04 must latch 0x04 into mem_sample_offset"
        );
        assert!((p.channels[0].voice.pos - 1024.0).abs() < 1.0);

        // Step through row 0's remaining ticks and the row break.
        for t in 1..p.speed {
            p.tick = t;
            p.advance_tick();
        }
        p.tick = 0;
        p.next_row();
        // Row 1 / tick 0: 900 should reuse memory (0x04) and place
        // the cursor at 1024 again.
        p.tick = 0;
        p.advance_tick();
        assert_eq!(p.row, 1);
        assert_eq!(
            p.channels[0].mem_sample_offset, 0x04,
            "900 must NOT overwrite memory with zero"
        );
        assert!(
            (p.channels[0].voice.pos - 1024.0).abs() < 1.0,
            "900 must reuse the latched 0x04 → 1024 cursor, got {}",
            p.channels[0].voice.pos
        );
    }

    #[test]
    fn nine_xx_out_of_range_plays_no_note() {
        // Per the PT spec ("Note that if the effect is out of range
        // (e.g. if it tries to jump beyond the end of the sample) NO
        // NOTE WILL BE PLAYED!"), a 9xx whose target lands at or past
        // the sample end silences the channel rather than letting the
        // mixer wrap. With a 256-byte sample, a 9x02 → 512-byte offset
        // is well past the end.
        let bytes = build_offset_stm(256, 0x02);
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        p.tick = 0;
        p.advance_tick();
        assert!(
            !p.channels[0].voice.active,
            "9xx past sample end must silence the channel"
        );
        // The note metadata is still latched (cur_semis at C-4) so a
        // subsequent porta / arpeggio anchors to the right pitch.
        assert!(
            (p.channels[0].cur_semis - 48.0).abs() < 1e-4,
            "out-of-range 9xx must still latch note pitch, got cur_semis = {}",
            p.channels[0].cur_semis
        );
    }

    #[test]
    fn nine_xx_at_exact_end_plays_no_note() {
        // Boundary: offset == sample length must also silence
        // (the spec says "beyond the end" but the canonical PT
        // behaviour is to treat == end as the same case — the cursor
        // at byte N of an N-byte sample reads past the buffer).
        // 256-byte sample + 9x01 → 256-byte offset.
        let bytes = build_offset_stm(256, 0x01);
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        p.tick = 0;
        p.advance_tick();
        assert!(
            !p.channels[0].voice.active,
            "9xx at exact sample end must silence the channel"
        );
    }

    #[test]
    fn nine_xx_just_inside_end_plays() {
        // Symmetry check: a 9xx that lands at sample_len - 1 (still
        // in-range) must NOT silence. 256-byte sample + 9x00 with
        // memory zeroed = offset 0, which is the "no offset" case but
        // since param 0 means "reuse memory" and memory starts at 0,
        // the effective offset is 0. Use a setup where memory has
        // been pre-loaded: a 512-byte sample + 9x01 lands at offset
        // 256 with 256 bytes of headroom.
        let bytes = build_offset_stm(512, 0x01);
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        p.tick = 0;
        p.advance_tick();
        assert!(
            p.channels[0].voice.active,
            "9xx with offset < sample_len must trigger the voice"
        );
        assert!(
            (p.channels[0].voice.pos - 256.0).abs() < 1.0,
            "9x01 in a 512-byte sample must place cursor at 256, got {}",
            p.channels[0].voice.pos
        );
    }

    /// Build an STM file with an `E9y` cell co-located on the row 0
    /// note-on cell so we can drive a retrig schedule from tick 1 onward
    /// without disturbing the existing `build_ping_stm` framing.
    fn build_retrig_stm(period: u8) -> Vec<u8> {
        let mut bytes = build_ping_stm();
        const PATTERN_OFF: usize = 0x410;
        // Row 0, channel 0 already carries the C-4 note + instrument 1.
        // The effect bytes are command nibble 0xE + param `9y`.
        bytes[PATTERN_OFF + 2] = 0x0E;
        bytes[PATTERN_OFF + 3] = 0x90 | (period & 0x0F);
        bytes
    }

    #[test]
    fn e9x_retrigger_resets_sample_cursor_every_y_ticks() {
        // E91 = retrig every 1 tick. The row's note triggers on tick 0
        // (cursor = 0), then ticks 1, 2, … re-zero the cursor before
        // the mixer would advance it further. We simulate the mixer's
        // cursor walk between ticks by bumping `voice.pos` manually so
        // we can observe whether the per-tick handler rewinds it.
        let bytes = build_retrig_stm(1);
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        // Tick 0 — row entry triggers the note.
        p.tick = 0;
        p.advance_tick();
        assert!(p.channels[0].voice.active, "tick 0 must trigger the voice");
        assert_eq!(
            p.channels[0].retrig_ticks, 1,
            "E91 must latch retrig period = 1; got {}",
            p.channels[0].retrig_ticks
        );

        // Simulate mixer advancing the cursor between ticks.
        p.channels[0].voice.pos = 50.0;
        p.tick = 1;
        p.advance_tick();
        assert!(
            p.channels[0].voice.pos.abs() < 1e-3,
            "E91 must reset the cursor on tick 1; pos = {}",
            p.channels[0].voice.pos
        );

        // Same again on tick 2.
        p.channels[0].voice.pos = 75.0;
        p.tick = 2;
        p.advance_tick();
        assert!(
            p.channels[0].voice.pos.abs() < 1e-3,
            "E91 must reset the cursor on tick 2; pos = {}",
            p.channels[0].voice.pos
        );
    }

    #[test]
    fn e90_does_not_retrigger() {
        // E90 carries an explicit zero period. Per the canonical PT
        // reading, that's the inert / no-op selection — the cursor
        // must keep advancing monotonically across ticks (no rewind).
        let bytes = build_retrig_stm(0);
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        p.tick = 0;
        p.advance_tick();
        assert_eq!(
            p.channels[0].retrig_ticks, 0,
            "E90 must capture period = 0; got {}",
            p.channels[0].retrig_ticks
        );
        // Simulate mixer advance.
        p.channels[0].voice.pos = 100.0;
        p.tick = 1;
        p.advance_tick();
        assert!(
            (p.channels[0].voice.pos - 100.0).abs() < 1e-3,
            "E90 must NOT rewind on tick 1; pos = {}",
            p.channels[0].voice.pos
        );
    }

    #[test]
    fn e9x_only_fires_on_tick_y_multiples() {
        // E93 = retrig every 3 ticks. Within one row (speed = 6) the
        // retrig fires at ticks 3 and 6, but ticks 1, 2, 4, 5 must
        // leave the cursor alone.
        let bytes = build_retrig_stm(3);
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        p.tick = 0;
        p.advance_tick();
        assert_eq!(p.channels[0].retrig_ticks, 3);

        // Tick 1 — no rewind.
        p.channels[0].voice.pos = 11.0;
        p.tick = 1;
        p.advance_tick();
        assert!(
            (p.channels[0].voice.pos - 11.0).abs() < 1e-3,
            "E93 must not retrigger on tick 1; pos = {}",
            p.channels[0].voice.pos
        );

        // Tick 2 — no rewind.
        p.channels[0].voice.pos = 22.0;
        p.tick = 2;
        p.advance_tick();
        assert!(
            (p.channels[0].voice.pos - 22.0).abs() < 1e-3,
            "E93 must not retrigger on tick 2; pos = {}",
            p.channels[0].voice.pos
        );

        // Tick 3 — rewind to 0.
        p.channels[0].voice.pos = 33.0;
        p.tick = 3;
        p.advance_tick();
        assert!(
            p.channels[0].voice.pos.abs() < 1e-3,
            "E93 must retrigger on tick 3 (3 % 3 == 0); pos = {}",
            p.channels[0].voice.pos
        );

        // Tick 4 — no rewind again (4 % 3 != 0).
        p.channels[0].voice.pos = 44.0;
        p.tick = 4;
        p.advance_tick();
        assert!(
            (p.channels[0].voice.pos - 44.0).abs() < 1e-3,
            "E93 must not retrigger on tick 4; pos = {}",
            p.channels[0].voice.pos
        );
    }

    #[test]
    fn e9x_does_not_retrigger_on_inactive_voice() {
        // If the channel's voice is silenced before the retrigger
        // window (e.g. by a fresh row with no note or a Dxy break),
        // the E9x schedule must not resurrect it. Mirrors the PT
        // contract: retrig replays a *playing* sample; it doesn't
        // ignite an idle channel.
        let bytes = build_retrig_stm(1);
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        p.tick = 0;
        p.advance_tick();
        // Manually deactivate the voice to simulate a kill from another
        // path (e.g. an upstream Dash-note or cut).
        p.channels[0].voice.active = false;
        p.channels[0].voice.pos = 99.0;
        p.tick = 1;
        p.advance_tick();
        // pos must NOT be reset to 0 because the voice is inactive
        // and the retrigger gate is conditional on `voice.active`.
        assert!(
            (p.channels[0].voice.pos - 99.0).abs() < 1e-3,
            "E91 must not touch an inactive voice; pos = {}",
            p.channels[0].voice.pos
        );
    }

    #[test]
    fn e9x_period_does_not_leak_into_next_row() {
        // E9 is row-scoped: a row without an `E9y` must NOT inherit
        // the previous row's period. Place `E91` on row 0, a plain
        // dot cell on row 1, then advance to row 1 and confirm the
        // channel's `retrig_ticks` register is cleared.
        let mut bytes = build_retrig_stm(1);
        // Row 1 channel 0: plain dot cell (no effect, no note).
        poke_effect(&mut bytes, 1, 0, 0x0, 0x00);
        let h = parse_header(&bytes).unwrap();
        let pats = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, pats, 44_100);

        // Walk through row 0's full tick range, then advance the row.
        for t in 0..p.speed {
            p.tick = t;
            p.advance_tick();
        }
        p.tick = 0;
        p.next_row();
        assert_eq!(p.row, 1, "row advance should land us on row 1");
        assert_eq!(
            p.channels[0].retrig_ticks, 1,
            "retrig_ticks should still be 1 before row 1's enter_row clears it"
        );

        // Enter row 1 and confirm retrig_ticks is cleared.
        p.tick = 0;
        p.advance_tick();
        assert_eq!(
            p.channels[0].retrig_ticks, 0,
            "row 1 (no E9y) must clear retrig_ticks; got {}",
            p.channels[0].retrig_ticks
        );

        // And confirm no rewind happens on the next tick of row 1.
        p.channels[0].voice.pos = 77.0;
        p.tick = 1;
        p.advance_tick();
        assert!(
            (p.channels[0].voice.pos - 77.0).abs() < 1e-3,
            "row 1 has no E9 — cursor must not be rewound; pos = {}",
            p.channels[0].voice.pos
        );
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
