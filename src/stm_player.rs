//! Scream Tracker v1 (`.stm`) playback engine.
//!
//! Drives the shared [`crate::mixer::MixerVoice`] core over STM's
//! 4-channel pattern engine. STM uses per-instrument C3 frequencies
//! rather than Amiga periods (see [`crate::mixer::StmC3Pitch`]) and
//! ProTracker-like effect columns.
//!
//! This player covers the minimum needed to emit audible PCM:
//!  - Row / tick scheduling with a ProTracker-style `speed` (ticks/row).
//!  - Note triggering with sample change + volume override.
//!  - Volume as encoded in the STM cell byte (0..=64).
//!  - A handful of effects (Cxx set-volume, Axx volume-slide).
//!
//! Effects that mutate pitch (Exx, Fxx-ish tone porta, arpeggio) are
//! not implemented here; notes play at the sample's C3 frequency for
//! their full row. This is explicitly the first audible milestone; the
//! shared core means adding effects later is local work.

use crate::mixer::{MixerVoice, PitchModel, StmC3Pitch};
use crate::stm::{
    StmCell, StmHeader, StmNoteKind, StmPattern, StmSampleBody, PATTERN_ROWS, STM_CHANNELS,
};

/// Classic tracker pacing constants. STM's tempo field is related to the
/// ticks-per-row and BPM-equivalent values; we keep the MOD defaults
/// unless the file overrides them.
pub const DEFAULT_SPEED_TICKS: u8 = 6;

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

            // Sample change — update the current instrument and pull
            // volume from the sample body.
            if cell.instrument != 0 {
                ch.instrument = cell.instrument;
                if let Some(body) = self.samples.get(cell.instrument as usize - 1) {
                    ch.volume = body.volume.min(64);
                }
            }

            // Cell volume overrides sample default (STM encodes volume
            // 0..=64 in the combined vol_lo|vol_hi field; the parser
            // already clamped it to 0..=64).
            if cell.volume > 0 && cell.volume <= 64 {
                ch.volume = cell.volume;
            }

            // Note trigger.
            match cell.kind() {
                StmNoteKind::Note { octave, semitone } if semitone <= 11 => {
                    ch.note = (octave, semitone);
                    let inst_idx = match (ch.instrument as usize).checked_sub(1) {
                        Some(i) => i,
                        None => continue,
                    };
                    if let Some(body) = self.samples.get(inst_idx) {
                        let pitch = StmC3Pitch {
                            c3_hz: body.c3_hz as f32,
                        };
                        let freq = pitch.note_to_freq(ch.note);
                        let vol = (ch.volume as f32 / 64.0) * (self.global_volume as f32 / 64.0);
                        ch.voice.trigger(freq, vol);
                    }
                }
                StmNoteKind::DashNote | StmNoteKind::Dots => {
                    // Per spec: "note off" style markers. Silence the voice.
                    ch.voice.active = false;
                }
                _ => {}
            }

            // Tick-0 effects.
            apply_tick0_effect(ch, cell.command, cell.command_param);
        }

        // Speed change Axx at tick 0? STM uses ProTracker-like Axx for
        // volume slide, Fxx for speed. Apply Fxx globally (like MOD).
        for ch in self.channels.iter() {
            if ch.effect == 0xF && ch.effect_param != 0 {
                if ch.effect_param < 0x20 {
                    self.speed = ch.effect_param;
                } else {
                    self.tempo = ch.effect_param;
                }
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
    }

    fn next_row(&mut self) {
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

fn apply_tick0_effect(ch: &mut StmChannel, effect: u8, param: u8) {
    // Cxx: set volume.
    if effect == 0xC {
        ch.volume = param.min(64);
        ch.voice.volume = ch.volume as f32 / 64.0;
    }
}

fn apply_tickn_effect(ch: &mut StmChannel) {
    let effect = ch.effect;
    let param = ch.effect_param;
    let x = param >> 4;
    let y = param & 0x0F;
    // Axy: volume slide. +x or -y per tick.
    if effect == 0xA {
        if x != 0 {
            ch.volume = (ch.volume as u16 + x as u16).min(64) as u8;
        } else if y != 0 {
            ch.volume = ch.volume.saturating_sub(y);
        }
        ch.voice.volume = ch.volume as f32 / 64.0;
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
}
