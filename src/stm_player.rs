//! Scream Tracker 2 (`.stm`) playback engine.
//!
//! Drives the shared [`crate::mixer::MixerVoice`] core over STM's
//! 4-channel pattern engine. The command set is Scream Tracker's own
//! lettered one — the cell's command number 1..=15 is the letter
//! `A`..`O` — as described by the tracker author in
//! `docs/audio/trackers/stm/ScreamTracker-v2.3-ST.DOC.txt` §6.4, with
//! the order-flow rules the staged black-box Scream Tracker 2.21 trace
//! settles (`stm-effect-semantics-gap.md`,
//! `fixtures/st221-jump-probes/`):
//!
//!  - `A` — tempo. The high nibble is the number of ticks per row
//!    (§6.4.1: slide commands run "TEMPO/10-1 times" per row, the
//!    tempo being shown in hex; the default `60` gives 5 slide steps);
//!    one tick lasts 20 ms (the trace measures 0.12 s per row at `60`).
//!  - `B` — jump. Per the ST 2.21 trace this does **not** break the
//!    pattern: it only replaces the order that follows the current
//!    pattern. A target that is unused / past the end restarts the song
//!    at order 0.
//!  - `C` — break: the next order (or a same-row `B`'s, in either
//!    channel order) starts at row 0 right after this row.
//!  - `D` — volume slide, per slide step: high nibble up, else low
//!    nibble down.
//!  - `E` / `F` — portamento up / down, per slide step.
//!  - `G` — note portamento toward the cell's note without
//!    retriggering; info 0 reuses the previous info byte.
//!  - `H` — vibrato (high nibble speed, low nibble size); info 0 reuses
//!    the previous info byte.
//!  - `I` — tremor: the sound is switched on / off for the times the
//!    two nibbles give, for as long as consecutive rows carry it.
//!  - `J` — arpeggio: note, note + low nibble, note + high nibble.
//!  - `K` — vibrato (remembered) + volume slide; `O` — note portamento
//!    (remembered) + volume slide.
//!  - `L`, `M`, `N` — "converted, but not implemented": no-ops.
//!
//! What the staged documents do not give (and this engine therefore
//! fixes by its own convention, see the crate README's STM section):
//! the unit of the `E` / `F` / `G` slide amounts and of the vibrato
//! size, the vibrato waveform, the effect of the tempo's low nibble,
//! and the exact tremor tick counts.
//!
//! Pitch: an instrument's C2SPD is "the speed (in Hz) at which the
//! sample should be played so that it would sound like C2" (§2.3), so
//! `freq = c2spd * 2 ^ ((octave * 12 + note - 24) / 12)`. Slides work
//! on a fractional half-note position.
//!
//! Scream Tracker 2 mixes its four channels to one output (PC speaker,
//! Covox or Sound Blaster DAC), so every channel plays centred.

use crate::mixer::{MixerVoice, VoiceBlock};
use crate::stm::{
    ORDER_UNUSED, PATTERN_ROWS, STM_CHANNELS, StmCell, StmHeader, StmNoteKind, StmPattern,
    StmSampleBody,
};
use crate::xm_player::waveform_lfo;

/// Tick rate: the staged ST 2.21 trace measures 64 rows at tempo `60`
/// (six ticks per row) as 7.68 s, i.e. 20 ms per tick.
pub const TICK_HZ: u32 = 50;

/// Ticks per row at the manual's default tempo `60`.
pub const DEFAULT_SPEED_TICKS: u8 = 6;

/// Command numbers (the cell's command nibble; 1 = `A`).
pub mod command {
    pub const TEMPO: u8 = 1; // A
    pub const JUMP: u8 = 2; // B
    pub const BREAK: u8 = 3; // C
    pub const VOLUME_SLIDE: u8 = 4; // D
    pub const PORTA_UP: u8 = 5; // E
    pub const PORTA_DOWN: u8 = 6; // F
    pub const NOTE_PORTA: u8 = 7; // G
    pub const VIBRATO: u8 = 8; // H
    pub const TREMOR: u8 = 9; // I
    pub const ARPEGGIO: u8 = 10; // J
    pub const VIBRATO_VOLUME_SLIDE: u8 = 11; // K
    pub const NOTE_PORTA_VOLUME_SLIDE: u8 = 15; // O
}

/// Slide-amount units per half note for `E` / `F` / `G` (this engine's
/// convention — the manual gives no unit): 16 units move one half note
/// per slide step.
const SLIDE_UNITS_PER_HALF_NOTE: f32 = 16.0;

/// Highest half-note position slides may reach (octave 8).
const MAX_SEMIS: f32 = 96.0;

/// Half-note index of the C-2 reference the C2SPD field describes.
const C2_SEMIS: f32 = 24.0;

fn note_to_semis(octave: u8, semitone: u8) -> f32 {
    (octave as f32) * 12.0 + (semitone as f32)
}

fn semis_to_freq(c2spd: f32, semis: f32) -> f32 {
    if c2spd <= 0.0 {
        return 0.0;
    }
    c2spd * 2.0f32.powf((semis - C2_SEMIS) / 12.0)
}

/// Per-channel playback state for STM.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct StmChannel {
    /// Currently-loaded instrument number (1..=31, 0 = none yet).
    pub instrument: u8,
    /// Last triggered note (octave, note in octave).
    pub note: (u8, u8),
    /// Volume 0..=64.
    pub volume: u8,
    /// Shared mixer voice.
    pub voice: MixerVoice,
    /// Current row's command number (0 = none, 1 = `A` …).
    pub effect: u8,
    /// Current row's command info byte.
    pub effect_param: u8,
    /// Live pitch as a fractional half-note index from C-0.
    pub cur_semis: f32,
    /// Target pitch of the note portamento (`G` / `O`).
    pub porta_target_semis: f32,
    /// `G` info-byte memory.
    pub porta_speed: u8,
    /// `H` info-byte memory (speed nibble, size nibble).
    pub vib_param: u8,
    /// Vibrato position in the 64-step cycle.
    pub vib_pos: u8,
    /// Tremor position inside the on + off cycle.
    pub tremor_pos: u8,
    /// True while the tremor has the sound switched off.
    pub tremor_muted: bool,
}

/// STM player — owns decoded patterns / samples and the row / tick
/// state machine.
#[non_exhaustive]
pub struct StmPlayerState {
    pub samples: Vec<StmSampleBody>,
    pub patterns: Vec<StmPattern>,
    /// The raw 128-entry order table.
    pub order: Vec<u8>,
    pub n_patterns: u8,
    pub channels: [StmChannel; STM_CHANNELS],
    /// Current tempo byte (high nibble = ticks per row).
    pub tempo: u8,
    pub sample_rate: u32,

    pub order_index: usize,
    pub row: u8,
    pub tick: u8,
    pub tick_sample_cursor: u32,
    /// Song over: the order list ran out, a jump named an unused order,
    /// or (with [`Self::stop_on_repeat`]) an order came round again.
    pub ended: bool,
    pub global_volume: u8,

    /// Order a `B` on the current pattern asked for; taken when the
    /// pattern ends (naturally or through `C`).
    pub pending_order_jump: Option<u8>,
    /// A `C` on the current row: leave the pattern after this row.
    pub pending_break: bool,
    /// End the song when the flow re-enters an order it already played
    /// (one finite pass). Scream Tracker itself keeps looping; clear
    /// this to follow it.
    pub stop_on_repeat: bool,
    /// How many times the song wrapped back to order 0 (the tracker's
    /// `Loop` counter).
    pub loops: u32,
    visited: Vec<bool>,
    /// Run the row / tick engine without mixing: `render` then only
    /// counts frames (the destination buffer is left untouched). Used
    /// to measure a pass.
    #[doc(hidden)]
    pub dry_run: bool,
}

impl StmPlayerState {
    pub fn new(
        header: &StmHeader,
        samples: Vec<StmSampleBody>,
        patterns: Vec<StmPattern>,
        sample_rate: u32,
    ) -> Self {
        let mut visited = vec![false; header.order.len()];
        if let Some(first) = visited.first_mut() {
            *first = true;
        }
        let mut state = StmPlayerState {
            samples,
            patterns,
            order: header.order.clone(),
            n_patterns: header.n_patterns,
            channels: Default::default(),
            tempo: header.tempo,
            sample_rate,
            order_index: 0,
            row: 0,
            tick: 0,
            tick_sample_cursor: 0,
            ended: false,
            global_volume: header.global_volume.min(64),
            pending_order_jump: None,
            pending_break: false,
            stop_on_repeat: true,
            loops: 0,
            visited,
            dry_run: false,
        };
        if !state.order_is_playable(0) {
            state.ended = true;
        }
        state
    }

    /// Ticks per row: the tempo's high nibble (at least one).
    pub fn ticks_per_row(&self) -> u8 {
        (self.tempo >> 4).max(1)
    }

    /// Output frames per tick (one tick = 20 ms).
    pub fn samples_per_tick(&self) -> u32 {
        (self.sample_rate / TICK_HZ).max(1)
    }

    fn order_is_playable(&self, index: usize) -> bool {
        match self.order.get(index) {
            Some(&p) => p < ORDER_UNUSED && p < self.n_patterns,
            None => false,
        }
    }

    fn cell_at(&self, row: u8, ch: usize) -> Option<StmCell> {
        let pat_idx = *self.order.get(self.order_index)? as usize;
        let pattern = self.patterns.get(pat_idx)?;
        pattern.rows.get(row as usize)?.get(ch).copied()
    }

    /// Enter a row: load note / volume / command into each channel.
    fn enter_row(&mut self) {
        for ch_idx in 0..STM_CHANNELS {
            let cell = self.cell_at(self.row, ch_idx).unwrap_or_default();
            let ch = &mut self.channels[ch_idx];
            let was_tremor = ch.effect == command::TREMOR;
            ch.effect = cell.command;
            ch.effect_param = cell.command_param;
            if !(was_tremor && cell.command == command::TREMOR) {
                // "The sound is tremored as long as the tremor commands
                // are set."
                ch.tremor_pos = 0;
                ch.tremor_muted = false;
            }

            // Info-byte memories: "If the infobyte is zero, the previous
            // value of the infobyte will be used" (G and H only).
            match cell.command {
                command::NOTE_PORTA if cell.command_param != 0 => {
                    ch.porta_speed = cell.command_param;
                }
                command::VIBRATO if cell.command_param != 0 => {
                    ch.vib_param = cell.command_param;
                }
                _ => {}
            }

            match cell.kind() {
                StmNoteKind::Note { octave, semitone } if semitone < 12 => {
                    // "The instrument field … has no effect if note is
                    // not specified. It can also be omitted. If so, the
                    // last instrument for that channel is used."
                    if cell.instrument != 0 {
                        ch.instrument = cell.instrument;
                    }
                    let body = (ch.instrument as usize)
                        .checked_sub(1)
                        .and_then(|i| self.samples.get(i));
                    // "If it's omitted, the default volume for the
                    // current instrument is used."
                    if cell.has_volume() {
                        ch.volume = cell.volume;
                    } else if let Some(body) = body {
                        ch.volume = body.volume.min(64);
                    }
                    let target = note_to_semis(octave, semitone);
                    let glide = matches!(
                        cell.command,
                        command::NOTE_PORTA | command::NOTE_PORTA_VOLUME_SLIDE
                    ) && ch.voice.active;
                    ch.note = (octave, semitone);
                    if glide {
                        // "note E2 is NOT played, but the pitch is
                        // slided … towards the E2 note."
                        ch.porta_target_semis = target;
                    } else if let Some(body) = body {
                        ch.cur_semis = target;
                        ch.porta_target_semis = target;
                        ch.vib_pos = 0;
                        if body.pcm.is_empty() {
                            ch.voice.active = false;
                        } else {
                            let freq = semis_to_freq(body.c3_hz as f32, target);
                            ch.voice.trigger(freq, 0.0);
                        }
                    }
                }
                _ => {
                    // No note: a volume on its own still applies ("It
                    // can be changed while a note is being played").
                    // The `-0-` / `...` / empty forms carry no note
                    // event.
                    if cell.has_volume() {
                        ch.volume = cell.volume;
                    }
                }
            }

            // Song-level commands, in channel order.
            match cell.command {
                command::TEMPO if cell.command_param != 0 => {
                    self.tempo = cell.command_param;
                }
                command::JUMP => {
                    self.pending_order_jump = Some(cell.command_param);
                }
                command::BREAK => {
                    self.pending_break = true;
                }
                _ => {}
            }
        }
    }

    /// Run the tick at the current position: row entry on tick 0, slide
    /// steps on the others, then refresh every voice's pitch / volume.
    fn begin_tick(&mut self) {
        if self.tick == 0 {
            self.enter_row();
        } else {
            for ch in self.channels.iter_mut() {
                apply_slide_step(ch);
            }
        }

        let cur_tick = self.tick;
        let global_vol = self.global_volume;
        for ch_idx in 0..STM_CHANNELS {
            let ch = &mut self.channels[ch_idx];
            let mut semis = ch.cur_semis;

            let vibrato = matches!(ch.effect, command::VIBRATO | command::VIBRATO_VOLUME_SLIDE);
            if vibrato && ch.vib_param & 0x0F != 0 {
                let speed = ch.vib_param >> 4;
                let size = (ch.vib_param & 0x0F) as f32;
                let lfo = waveform_lfo(0, ch.vib_pos) as f32;
                semis += lfo * size / 32.0 / SLIDE_UNITS_PER_HALF_NOTE;
                if cur_tick > 0 {
                    ch.vib_pos = ch.vib_pos.wrapping_add(speed.wrapping_mul(4)) & 0x3F;
                }
            }

            if ch.effect == command::ARPEGGIO {
                // "Three stages: Note, Note+lo-nibble halfnotes,
                // Note+hi nibble halfnotes".
                semis += match cur_tick % 3 {
                    0 => 0.0,
                    1 => (ch.effect_param & 0x0F) as f32,
                    _ => (ch.effect_param >> 4) as f32,
                };
            }

            if ch.voice.active
                && let Some(body) = (ch.instrument as usize)
                    .checked_sub(1)
                    .and_then(|i| self.samples.get(i))
            {
                let freq = semis_to_freq(body.c3_hz as f32, semis);
                if freq > 0.0 {
                    ch.voice.freq = freq;
                }
            }

            let volume = if ch.tremor_muted {
                0
            } else {
                ch.volume.min(64)
            };
            ch.voice.volume = (volume as f32 / 64.0) * (global_vol as f32 / 64.0);
        }
    }

    /// Close the tick: advance the tick counter and, after the row's
    /// last tick, the position.
    fn end_tick(&mut self) {
        self.tick += 1;
        if self.tick >= self.ticks_per_row() {
            self.tick = 0;
            self.next_row();
        }
    }

    /// Advance the engine by one whole tick without rendering audio —
    /// for flow tests and position traces.
    #[doc(hidden)]
    pub fn step_tick(&mut self) {
        if self.ended {
            return;
        }
        self.begin_tick();
        self.end_tick();
    }

    fn next_row(&mut self) {
        if self.pending_break {
            self.advance_pattern();
            return;
        }
        self.row += 1;
        if (self.row as usize) >= PATTERN_ROWS {
            self.advance_pattern();
        }
    }

    /// Leave the current pattern: the order a `B` asked for, else the
    /// next one, at row 0. An unused / out-of-range order restarts the
    /// song at order 0 — the tracker's `Loop` counter goes up — and
    /// that is the song-over signal.
    fn advance_pattern(&mut self) {
        self.pending_break = false;
        self.row = 0;
        let target = match self.pending_order_jump.take() {
            Some(order) => order as usize,
            None => self.order_index + 1,
        };
        if !self.order_is_playable(target) {
            self.order_index = 0;
            self.loops = self.loops.saturating_add(1);
            self.ended = true;
            self.visited.fill(false);
            if let Some(first) = self.visited.first_mut() {
                *first = true;
            }
            return;
        }
        self.order_index = target;
        if self.visited[target] && self.stop_on_repeat {
            self.ended = true;
            self.visited.fill(false);
        }
        self.visited[target] = true;
    }

    /// Render interleaved stereo S16 PCM into `dst` (length must be
    /// even). Returns the number of frames produced — fewer than asked
    /// once the song is over. All four channels play centred.
    pub fn render(&mut self, dst: &mut [i16]) -> usize {
        assert!(dst.len().is_multiple_of(2));
        let mut produced = 0usize;
        let total_frames = dst.len() / 2;
        let out_rate = self.sample_rate as f32;

        while produced < total_frames {
            if self.ended {
                break;
            }
            if self.tick_sample_cursor == 0 {
                self.begin_tick();
            }
            let spt = self.samples_per_tick();
            let remaining = spt.saturating_sub(self.tick_sample_cursor);
            let want = (total_frames - produced).min(remaining as usize);

            if !self.dry_run {
                let block = &mut dst[produced * 2..(produced + want) * 2];
                let samples = &self.samples;
                // Resolve each channel's body, loop bounds and position
                // step once per tick block.
                let mut bodies: [Option<(&StmSampleBody, VoiceBlock)>; STM_CHANNELS] =
                    [None; STM_CHANNELS];
                for (slot, ch) in bodies.iter_mut().zip(self.channels.iter()) {
                    if ch.voice.active {
                        *slot = (ch.instrument as usize)
                            .checked_sub(1)
                            .and_then(|i| samples.get(i))
                            .and_then(|body| Some((body, ch.voice.resolve_block(body, out_rate)?)));
                    }
                }
                for frame in block.as_chunks_mut::<2>().0 {
                    let mut mix = 0.0f32;
                    for (ch, body) in self.channels.iter_mut().zip(bodies.iter()) {
                        if let Some((body, voice_block)) = body {
                            mix += ch.voice.render_resolved(*body, voice_block);
                        }
                    }
                    // Four channels into one output: divide by the channel
                    // count so a full-scale chord cannot clip.
                    let s = ((mix / STM_CHANNELS as f32).clamp(-1.0, 1.0) * 32767.0) as i16;
                    frame[0] = s;
                    frame[1] = s;
                }
            }
            produced += want;

            self.tick_sample_cursor += want as u32;
            if self.tick_sample_cursor >= spt {
                self.tick_sample_cursor = 0;
                self.end_tick();
            }
        }
        produced
    }
}

/// One slide step (every tick of a row but the first — "TEMPO/10-1
/// times" per row).
fn apply_slide_step(ch: &mut StmChannel) {
    let info = ch.effect_param;
    match ch.effect {
        command::VOLUME_SLIDE
        | command::VIBRATO_VOLUME_SLIDE
        | command::NOTE_PORTA_VOLUME_SLIDE => {
            if ch.effect == command::NOTE_PORTA_VOLUME_SLIDE {
                note_porta_step(ch);
            }
            // "If the hi-nibble … is specified, the volume is increased
            // by the amount the digit specifies. If the lo-nibble is
            // set, the volume will be decreased."
            let up = info >> 4;
            let down = info & 0x0F;
            if up != 0 {
                ch.volume = (ch.volume + up).min(64);
            } else {
                ch.volume = ch.volume.saturating_sub(down);
            }
        }
        command::PORTA_UP => {
            ch.cur_semis = (ch.cur_semis + info as f32 / SLIDE_UNITS_PER_HALF_NOTE).min(MAX_SEMIS);
        }
        command::PORTA_DOWN => {
            ch.cur_semis = (ch.cur_semis - info as f32 / SLIDE_UNITS_PER_HALF_NOTE).max(0.0);
        }
        command::NOTE_PORTA => note_porta_step(ch),
        command::TREMOR => {
            // On for the high nibble's time, off for the low nibble's.
            // The staged example song demonstrates the command with
            // `I10`, which only gates anything if a nibble of n lasts
            // n + 1 steps — so that is the reading used.
            let on = (info >> 4) + 1;
            let off = (info & 0x0F) + 1;
            ch.tremor_pos %= on + off;
            ch.tremor_muted = ch.tremor_pos >= on;
            ch.tremor_pos = (ch.tremor_pos + 1) % (on + off);
        }
        _ => {}
    }
}

/// One `G` / `O` step toward the target note: "When the pitch reaches
/// the note specified … the command has no effect."
fn note_porta_step(ch: &mut StmChannel) {
    let step = ch.porta_speed as f32 / SLIDE_UNITS_PER_HALF_NOTE;
    if (ch.cur_semis - ch.porta_target_semis).abs() <= step {
        ch.cur_semis = ch.porta_target_semis;
    } else if ch.cur_semis < ch.porta_target_semis {
        ch.cur_semis += step;
    } else {
        ch.cur_semis -= step;
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::stm::{extract_samples, parse_header, parse_patterns};
    use crate::stm_writer::{StmWriter, StmWriterInstrument, StmWriterPattern};

    /// A 64-byte square wave (unsigned 8-bit, period 32 bytes).
    fn square_pcm(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| if (i / 16) % 2 == 0 { 0xC0 } else { 0x40 })
            .collect()
    }

    /// Writer preset: one looped square instrument, `n` empty patterns
    /// played in order.
    pub fn base_writer(n_patterns: usize) -> StmWriter {
        let mut w = StmWriter::default();
        w.instruments.push(StmWriterInstrument {
            name: "SQUARE.SMP".into(),
            pcm: square_pcm(4096),
            loop_start: 0,
            loop_end: 4096,
            volume: 64,
            c2spd: 8363,
        });
        for _ in 0..n_patterns {
            w.patterns.push(StmWriterPattern::new());
        }
        w.orders = (0..n_patterns as u8).collect();
        w
    }

    pub fn note(octave: u8, semitone: u8, instrument: u8) -> StmCell {
        StmCell {
            note_raw: (octave << 4) | semitone,
            instrument,
            ..StmCell::default()
        }
    }

    pub fn fx(cmd: u8, info: u8) -> StmCell {
        StmCell {
            command: cmd,
            command_param: info,
            ..StmCell::default()
        }
    }

    fn with_fx(mut cell: StmCell, cmd: u8, info: u8) -> StmCell {
        cell.command = cmd;
        cell.command_param = info;
        cell
    }

    pub fn player_for(w: &StmWriter) -> StmPlayerState {
        let bytes = w.build();
        let h = parse_header(&bytes).unwrap();
        let patterns = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        StmPlayerState::new(&h, samples, patterns, 44_100)
    }

    /// Build a tiny STM module with a single note-on on row 0, channel 0.
    pub fn build_ping_stm() -> Vec<u8> {
        let mut w = base_writer(1);
        w.patterns[0].put(0, 0, note(2, 0, 1));
        w.build()
    }

    /// Step whole rows; returns the `(order, row)` reached.
    fn step_rows(p: &mut StmPlayerState, rows: usize) -> (usize, u8) {
        for _ in 0..rows {
            let ticks = p.ticks_per_row();
            for _ in 0..ticks {
                p.step_tick();
            }
        }
        (p.order_index, p.row)
    }

    /// Run tick 0 of the current row only (row entry).
    fn enter(p: &mut StmPlayerState) {
        assert_eq!(p.tick, 0);
        p.step_tick();
    }

    /// Play on until tick 0 of `row` has run.
    fn enter_at(p: &mut StmPlayerState, row: u8) {
        while !(p.row == row && p.tick == 0) {
            p.step_tick();
        }
        p.step_tick();
    }

    #[test]
    fn stm_player_emits_nonzero_audio() {
        let bytes = build_ping_stm();
        let h = parse_header(&bytes).unwrap();
        let patterns = parse_patterns(&h, &bytes);
        let samples = extract_samples(&h, &bytes);
        let mut p = StmPlayerState::new(&h, samples, patterns, 44_100);
        let mut buf = vec![0i16; 4096 * 2];
        assert_eq!(p.render(&mut buf), 4096);
        assert!(buf.iter().any(|&s| s != 0));
        // Every channel is centred: left == right.
        assert!(buf.as_chunks::<2>().0.iter().all(|f| f[0] == f[1]));
    }

    #[test]
    fn c2spd_is_the_rate_of_c_octave_two() {
        // Manual §2.3: C2SPD is "the speed (in Hz) at which the sample
        // should be played so that it would sound like C2".
        let mut w = base_writer(1);
        w.patterns[0].put(0, 0, note(2, 0, 1));
        w.patterns[0].put(0, 1, note(3, 0, 1));
        w.patterns[0].put(0, 2, note(1, 0, 1));
        w.patterns[0].put(0, 3, note(2, 7, 1));
        let mut p = player_for(&w);
        enter(&mut p);
        assert!((p.channels[0].voice.freq - 8363.0).abs() < 0.01);
        assert!((p.channels[1].voice.freq - 16726.0).abs() < 0.01);
        assert!((p.channels[2].voice.freq - 4181.5).abs() < 0.01);
        let fifth = 8363.0 * 2.0f32.powf(7.0 / 12.0);
        assert!((p.channels[3].voice.freq - fifth).abs() < 0.5);
    }

    #[test]
    fn row_time_is_the_tempo_high_nibble_in_20ms_ticks() {
        // Trace: 64 rows at tempo 0x60 = 7.68 s -> 0.12 s per row.
        let w = base_writer(1);
        let mut p = player_for(&w);
        assert_eq!(p.ticks_per_row(), DEFAULT_SPEED_TICKS);
        assert_eq!(p.samples_per_tick(), 882);
        let mut buf = vec![0i16; 2 * 44_100 * 9];
        let frames = p.render(&mut buf);
        assert!(p.ended, "one pattern, then the order list runs out");
        assert_eq!(frames, 64 * 6 * 882, "7.68 s at 44.1 kHz");
    }

    #[test]
    fn tempo_command_sets_ticks_per_row() {
        let mut w = base_writer(1);
        w.patterns[0].put(1, 3, fx(command::TEMPO, 0x30));
        w.patterns[0].put(3, 0, fx(command::TEMPO, 0x00));
        let mut p = player_for(&w);
        step_rows(&mut p, 1);
        assert_eq!(p.ticks_per_row(), 6);
        enter(&mut p);
        assert_eq!(p.tempo, 0x30);
        assert_eq!(p.ticks_per_row(), 3, "A30: three ticks per row");
        p.step_tick();
        p.step_tick();
        assert_eq!((p.row, p.tick), (2, 0), "row 1 lasted three ticks");
        enter_at(&mut p, 3);
        assert_eq!(p.tempo, 0x30, "A00 leaves the tempo alone");
    }

    // ---- order flow: the staged ST 2.21 trace rules ----

    #[test]
    fn break_leaves_the_pattern_immediately() {
        // STC: `C00` on row 4 -> next state order 1, row 0.
        let mut w = base_writer(3);
        w.patterns[0].put(4, 1, fx(command::BREAK, 0));
        let mut p = player_for(&w);
        assert_eq!(step_rows(&mut p, 4), (0, 4));
        assert_eq!(step_rows(&mut p, 1), (1, 0));
    }

    #[test]
    fn jump_alone_waits_for_the_pattern_end() {
        // STB: `B02` on row 4 does not break; pattern 0 plays all 64
        // rows, then order 2 row 0.
        let mut w = base_writer(3);
        w.patterns[0].put(4, 1, fx(command::JUMP, 2));
        let mut p = player_for(&w);
        assert_eq!(step_rows(&mut p, 5), (0, 5), "no break on the B row");
        assert_eq!(step_rows(&mut p, 58), (0, 63));
        assert_eq!(step_rows(&mut p, 1), (2, 0));
        assert!(!p.ended);
    }

    #[test]
    fn jump_plus_break_goes_to_the_jump_order_in_either_channel_order() {
        // STBC / STCB: immediate break to order 2 row 0.
        for (b_ch, c_ch) in [(1, 2), (2, 1)] {
            let mut w = base_writer(3);
            w.patterns[0].put(4, b_ch, fx(command::JUMP, 2));
            w.patterns[0].put(4, c_ch, fx(command::BREAK, 0));
            let mut p = player_for(&w);
            assert_eq!(step_rows(&mut p, 5), (2, 0));
            assert!(!p.ended);
        }
    }

    #[test]
    fn jump_and_later_break_on_another_row_still_combine() {
        // `B` only replaces the next order, so a `C` further down the
        // same pattern takes it.
        let mut w = base_writer(3);
        w.patterns[0].put(2, 0, fx(command::JUMP, 2));
        w.patterns[0].put(9, 3, fx(command::BREAK, 0));
        let mut p = player_for(&w);
        assert_eq!(step_rows(&mut p, 10), (2, 0));
    }

    #[test]
    fn jump_to_an_unused_order_restarts_the_song_after_the_pattern() {
        // STBPAST: `B05` (order 5 = 99): pattern 0 plays to its end,
        // then the song restarts at order 0 with the loop counter up.
        let mut w = base_writer(3);
        w.patterns[0].put(4, 1, fx(command::JUMP, 5));
        let mut p = player_for(&w);
        assert_eq!(step_rows(&mut p, 63), (0, 63));
        assert!(!p.ended);
        assert_eq!(step_rows(&mut p, 1), (0, 0));
        assert_eq!(p.loops, 1);
        assert!(p.ended, "the restart is the song-over signal");
    }

    #[test]
    fn a_pending_jump_does_not_outlive_its_pattern() {
        let mut w = base_writer(3);
        w.patterns[0].put(0, 0, fx(command::JUMP, 2));
        let mut p = player_for(&w);
        assert_eq!(step_rows(&mut p, 64), (2, 0));
        assert_eq!(p.pending_order_jump, None);
        // Pattern 2 ends naturally -> order 3 is unused -> song over.
        step_rows(&mut p, 64);
        assert!(p.ended);
    }

    #[test]
    fn natural_end_of_the_order_list_ends_the_song() {
        let w = base_writer(2);
        let mut p = player_for(&w);
        assert_eq!(step_rows(&mut p, 64), (1, 0));
        assert!(!p.ended);
        step_rows(&mut p, 64);
        assert!(p.ended);
        assert_eq!((p.order_index, p.row, p.loops), (0, 0, 1));
        let mut buf = vec![0i16; 64];
        assert_eq!(p.render(&mut buf), 0, "an ended player renders nothing");
    }

    #[test]
    fn revisiting_an_order_ends_the_pass() {
        // The shape of the staged example song: the last pattern jumps
        // back to order 1, skipping the intro at order 0.
        let mut w = base_writer(3);
        w.patterns[2].put(63, 1, fx(command::JUMP, 1));
        let mut p = player_for(&w);
        assert_eq!(step_rows(&mut p, 3 * 64 - 1), (2, 63));
        assert!(!p.ended);
        assert_eq!(step_rows(&mut p, 1), (1, 0));
        assert!(p.ended, "order 1 came round again: one pass is over");

        // With the stop cleared the engine follows the tracker and
        // keeps looping.
        let mut p = player_for(&w);
        p.stop_on_repeat = false;
        assert_eq!(step_rows(&mut p, 3 * 64), (1, 0));
        assert!(!p.ended);
        assert_eq!(step_rows(&mut p, 2 * 64), (1, 0));
        assert!(!p.ended);
    }

    #[test]
    fn unusable_first_order_is_an_empty_song() {
        let mut w = base_writer(1);
        w.orders = vec![ORDER_UNUSED];
        let mut p = player_for(&w);
        assert!(p.ended);
        let mut buf = vec![0i16; 64];
        assert_eq!(p.render(&mut buf), 0);
        // An order naming a pattern the file does not store ends the
        // song the same way.
        w.orders = vec![0, 7];
        let mut p = player_for(&w);
        step_rows(&mut p, 64);
        assert!(p.ended);
    }

    // ---- notes, instruments, volume ----

    #[test]
    fn omitted_volume_takes_the_instrument_default() {
        let mut w = base_writer(1);
        w.instruments[0].volume = 40;
        w.patterns[0].put(0, 0, note(2, 0, 1));
        let mut loud = note(2, 0, 1);
        loud.volume = 17;
        w.patterns[0].put(0, 1, loud);
        // Row 2: a note with neither instrument nor volume reuses the
        // channel's instrument and its default volume.
        w.patterns[0].put(2, 1, note(2, 4, 0));
        let mut p = player_for(&w);
        enter(&mut p);
        assert_eq!(p.channels[0].volume, 40);
        assert_eq!(p.channels[1].volume, 17);
        enter_at(&mut p, 2);
        assert_eq!(p.channels[1].instrument, 1);
        assert_eq!(p.channels[1].volume, 40);
        assert_eq!(p.channels[1].note, (2, 4));
    }

    #[test]
    fn volume_alone_changes_a_playing_note_and_instrument_alone_does_nothing() {
        let mut w = base_writer(1);
        w.instruments.push(StmWriterInstrument {
            name: "TWO".into(),
            pcm: square_pcm(64),
            volume: 9,
            ..StmWriterInstrument::default()
        });
        w.patterns[0].put(0, 0, note(2, 0, 1));
        let vol_only = StmCell {
            volume: 12,
            ..StmCell::default()
        };
        w.patterns[0].put(1, 0, vol_only);
        let ins_only = StmCell {
            instrument: 2,
            ..StmCell::default()
        };
        w.patterns[0].put(2, 0, ins_only);
        let mut p = player_for(&w);
        enter_at(&mut p, 1);
        assert_eq!(p.channels[0].volume, 12);
        assert!(p.channels[0].voice.active);
        enter_at(&mut p, 2);
        assert_eq!(
            (p.channels[0].instrument, p.channels[0].volume),
            (1, 12),
            "an instrument without a note has no effect"
        );
    }

    #[test]
    fn dash_and_dots_note_forms_leave_the_voice_playing() {
        // The staged example song fills the rows under its sustained,
        // volume-faded notes with packed 252 cells — they cannot be
        // note stops.
        let mut w = base_writer(1);
        w.song = true;
        w.patterns[0].put(0, 0, note(2, 0, 1));
        let bytes = w.build();
        let h = parse_header(&bytes).unwrap();
        let patterns = parse_patterns(&h, &bytes);
        assert_eq!(patterns[0].rows[1][0].kind(), StmNoteKind::DashNote);
        let mut samples = extract_samples(&h, &bytes);
        samples[0] = StmSampleBody::from_unsigned(&h.instruments[0], &square_pcm(4096));
        let mut p = StmPlayerState::new(&h, samples, patterns, 44_100);
        step_rows(&mut p, 8);
        assert!(p.channels[0].voice.active);
        assert_eq!(p.channels[0].volume, 64);
    }

    #[test]
    fn global_volume_scales_the_voice() {
        let mut w = base_writer(1);
        w.global_volume = 32;
        w.patterns[0].put(0, 0, note(2, 0, 1));
        let mut p = player_for(&w);
        enter(&mut p);
        assert!((p.channels[0].voice.volume - 0.5).abs() < 1e-6);
    }

    // ---- per-step commands ----

    #[test]
    fn volume_slide_runs_ticks_minus_one_times_per_row() {
        // Manual §6.4.1: "if tempo is 60, the volume slide will be run
        // 5 times, which means that command D02 slides the volume down
        // 5*2=10 units every row."
        let mut w = base_writer(1);
        w.patterns[0].put(0, 0, with_fx(note(2, 0, 1), command::VOLUME_SLIDE, 0x02));
        w.patterns[0].put(1, 0, fx(command::VOLUME_SLIDE, 0x30));
        w.patterns[0].put(2, 0, fx(command::VOLUME_SLIDE, 0x00));
        let mut p = player_for(&w);
        step_rows(&mut p, 1);
        assert_eq!(p.channels[0].volume, 54);
        step_rows(&mut p, 1);
        assert_eq!(p.channels[0].volume, 64, "D30: +3 x 5, capped at 64");
        step_rows(&mut p, 1);
        assert_eq!(p.channels[0].volume, 64, "D00 has no memory");
    }

    #[test]
    fn portamento_e_slides_up_and_f_slides_down() {
        let mut w = base_writer(1);
        w.patterns[0].put(0, 0, with_fx(note(2, 0, 1), command::PORTA_UP, 0x08));
        w.patterns[0].put(0, 1, with_fx(note(2, 0, 1), command::PORTA_DOWN, 0x08));
        let mut p = player_for(&w);
        enter(&mut p);
        let start = p.channels[0].voice.freq;
        for _ in 1..6 {
            p.step_tick();
        }
        // 5 steps x 8/16 half note = 2.5 half notes each way.
        assert!((p.channels[0].cur_semis - 26.5).abs() < 1e-4);
        assert!((p.channels[1].cur_semis - 21.5).abs() < 1e-4);
        assert!(p.channels[0].voice.freq > start);
        assert!(p.channels[1].voice.freq < start);
    }

    #[test]
    fn note_portamento_glides_without_retrigger_and_stops_at_the_note() {
        let mut w = base_writer(1);
        w.patterns[0].put(0, 0, note(2, 0, 1));
        // Row 1: E-2 with G05 — the note is not played.
        w.patterns[0].put(1, 0, with_fx(note(2, 4, 0), command::NOTE_PORTA, 0x10));
        // Rows 2-3: G00 reuses the previous info byte.
        w.patterns[0].put(2, 0, fx(command::NOTE_PORTA, 0x00));
        w.patterns[0].put(3, 0, fx(command::NOTE_PORTA, 0x00));
        let mut p = player_for(&w);
        step_rows(&mut p, 1);
        let pos_before = p.channels[0].voice.pos;
        enter(&mut p);
        assert_eq!(p.channels[0].voice.pos, pos_before, "no retrigger");
        assert!((p.channels[0].cur_semis - 24.0).abs() < 1e-4);
        assert!((p.channels[0].porta_target_semis - 28.0).abs() < 1e-4);
        for _ in 1..6 {
            p.step_tick();
        }
        // 5 steps of one half note would overshoot E-2: it stops there.
        assert!((p.channels[0].cur_semis - 28.0).abs() < 1e-4);
        step_rows(&mut p, 2);
        assert!((p.channels[0].cur_semis - 28.0).abs() < 1e-4);
        assert_eq!(p.channels[0].porta_speed, 0x10);
    }

    #[test]
    fn note_portamento_row_reloads_the_default_volume() {
        // Example song pattern 3: a `D0F` fade to silence, then
        // `B-2 05 .. G0F` — the glide row gives a note and no volume,
        // so the instrument default applies again.
        let mut w = base_writer(1);
        w.patterns[0].put(0, 0, with_fx(note(2, 5, 1), command::VOLUME_SLIDE, 0x0F));
        w.patterns[0].put(1, 0, with_fx(note(2, 11, 1), command::NOTE_PORTA, 0x0F));
        let mut p = player_for(&w);
        step_rows(&mut p, 1);
        assert_eq!(p.channels[0].volume, 0);
        enter(&mut p);
        assert_eq!(p.channels[0].volume, 64);
        assert!((p.channels[0].cur_semis - 29.0).abs() < 1e-4, "still F-2");
    }

    #[test]
    fn vibrato_wobbles_around_the_note_and_remembers_its_info() {
        let mut w = base_writer(1);
        w.patterns[0].put(0, 0, with_fx(note(2, 0, 1), command::VIBRATO, 0x44));
        w.patterns[0].put(1, 0, fx(command::VIBRATO, 0x00));
        w.patterns[0].put(2, 0, StmCell::default());
        let mut p = player_for(&w);
        let mut freqs = Vec::new();
        for _ in 0..12 {
            p.step_tick();
            freqs.push(p.channels[0].voice.freq);
        }
        assert_eq!(p.channels[0].vib_param, 0x44, "H00 reuses H44");
        let (lo, hi) = freqs
            .iter()
            .fold((f32::MAX, f32::MIN), |(l, h), &f| (l.min(f), h.max(f)));
        assert!(lo < 8363.0 - 50.0 && hi > 8363.0 + 50.0, "{lo}..{hi}");
        assert!((p.channels[0].cur_semis - 24.0).abs() < 1e-6, "no drift");
        // The row without H plays the plain note again.
        p.step_tick();
        assert!((p.channels[0].voice.freq - 8363.0).abs() < 0.01);
    }

    #[test]
    fn tremor_gates_the_sound_while_the_command_repeats() {
        let mut w = base_writer(1);
        w.patterns[0].put(0, 0, with_fx(note(2, 0, 1), command::TREMOR, 0x10));
        w.patterns[0].put(1, 0, fx(command::TREMOR, 0x10));
        let mut p = player_for(&w);
        let mut gate = Vec::new();
        for _ in 0..12 {
            p.step_tick();
            gate.push(p.channels[0].voice.volume > 0.0);
        }
        // I10: on for two steps, off for one, counted over slide steps
        // and carried across the two rows.
        assert!(gate.iter().any(|&g| g) && gate.iter().any(|&g| !g));
        let muted = gate.iter().filter(|&&g| !g).count();
        assert!((3..=4).contains(&muted), "about a third muted: {gate:?}");
        // Row 2 has no tremor: the sound is back on.
        p.step_tick();
        assert!(p.channels[0].voice.volume > 0.0);
        assert_eq!(p.channels[0].volume, 64, "tremor never edits the volume");
    }

    #[test]
    fn arpeggio_plays_note_then_low_nibble_then_high_nibble() {
        let mut w = base_writer(1);
        w.patterns[0].put(0, 0, with_fx(note(2, 0, 1), command::ARPEGGIO, 0x74));
        let mut p = player_for(&w);
        let mut ratios = Vec::new();
        for _ in 0..6 {
            p.step_tick();
            ratios.push((p.channels[0].voice.freq / 8363.0).log2() * 12.0);
        }
        for (got, want) in ratios.iter().zip([0.0, 4.0, 7.0, 0.0, 4.0, 7.0]) {
            assert!((got - want).abs() < 1e-3, "{ratios:?}");
        }
        assert!((p.channels[0].cur_semis - 24.0).abs() < 1e-6);
    }

    #[test]
    fn k_and_o_combine_the_remembered_effect_with_a_volume_slide() {
        let mut w = base_writer(1);
        w.patterns[0].put(0, 0, with_fx(note(2, 0, 1), command::VIBRATO, 0x42));
        w.patterns[0].put(1, 0, fx(command::VIBRATO_VOLUME_SLIDE, 0x04));
        w.patterns[0].put(0, 1, note(2, 0, 1));
        w.patterns[0].put(1, 1, with_fx(note(2, 2, 0), command::NOTE_PORTA, 0x04));
        w.patterns[0].put(2, 1, fx(command::NOTE_PORTA_VOLUME_SLIDE, 0x02));
        let mut p = player_for(&w);
        step_rows(&mut p, 2);
        assert_eq!(p.channels[0].volume, 64 - 20, "K04: -4 x 5");
        assert_eq!(p.channels[0].vib_param, 0x42, "K keeps the H info");
        // Row 1 glided 5 x 4/16 = 1.25 of the 2 half notes; row 2's O
        // finishes the glide and slides the volume.
        assert!((p.channels[1].cur_semis - 25.25).abs() < 1e-4);
        step_rows(&mut p, 1);
        assert!((p.channels[1].cur_semis - 26.0).abs() < 1e-4);
        assert_eq!(p.channels[1].volume, 64 - 10);
        assert_eq!(p.channels[1].porta_speed, 0x04, "O keeps the G info");
    }

    #[test]
    fn unimplemented_letters_are_no_ops() {
        // "Lx,M,N - ProTracker commands converted, but not implemented".
        let mut w = base_writer(2);
        for (ch, cmd) in [(0usize, 12u8), (1, 13), (2, 14), (3, 0)] {
            w.patterns[0].put(0, ch, with_fx(note(2, 0, 1), cmd, 0xFF));
        }
        let mut p = player_for(&w);
        assert_eq!(step_rows(&mut p, 2), (0, 2));
        for ch in &p.channels {
            assert_eq!(ch.volume, 64);
            assert!((ch.cur_semis - 24.0).abs() < 1e-6);
        }
        assert_eq!(p.tempo, 0x60);
    }

    #[test]
    fn hostile_cells_never_panic() {
        let mut w = base_writer(2);
        w.tempo = 0x00;
        w.orders = vec![1, 0, 1, 0];
        for row in 0..64 {
            for ch in 0..4 {
                let seed = (row * 4 + ch) as u8;
                w.patterns[row % 2].put(
                    row,
                    ch,
                    StmCell {
                        note_raw: seed.wrapping_mul(37),
                        instrument: seed % 32,
                        volume: seed.wrapping_mul(11) & 0x7F,
                        command: seed % 16,
                        command_param: seed.wrapping_mul(73),
                    },
                );
            }
        }
        let mut p = player_for(&w);
        let mut buf = vec![0i16; 4096];
        let mut guard = 0;
        while p.render(&mut buf) > 0 {
            guard += 1;
            assert!(guard < 100_000, "the pass must be finite");
        }
        assert!(p.ended);
    }
}
