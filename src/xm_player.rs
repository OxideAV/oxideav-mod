//! FastTracker 2 Extended Module (`.xm`) playback engine — basic notes.
//!
//! XM is substantially richer than MOD / STM: variable channel count,
//! per-instrument sample map (96 note → sample slots), 8 or 16-bit
//! delta-decoded samples, volume + panning envelopes, fadeout, vibrato,
//! two frequency tables. This module implements the minimum needed to
//! produce audible PCM:
//!
//!  - Row / tick scheduling with per-file default tempo + BPM.
//!  - Note triggering via the sample-map lookup.
//!  - Amiga / Linear pitch table selection via [`crate::mixer::XmPitch`].
//!  - Basic volume handling (sample default, Set-Volume in the volume
//!    column, effect Cxx).
//!
//! Not yet implemented (explicit TODO):
//!  - Volume + panning envelopes (the `XmEnvelope` data is parsed but
//!    unused).
//!  - Vibrato / fadeout.
//!  - Any per-tick effect besides volume slide (Axx) and set-volume.
//!  - Tone portamento.
//!  - Pattern jumps (Bxx / Dxx).
//!
//! These are expected follow-on work; the mixer core and pitch trait are
//! already in place, so adding per-format features becomes local.

use crate::mixer::{MixerVoice, PitchModel, XmPitch, XmPitchTable};
use crate::xm::{
    XmCell, XmFrequencyTable, XmHeader, XmInstrument, XmPattern, XmSampleLoopMode, XmVolume,
};

/// Per-channel playback state for XM.
#[derive(Clone, Debug, Default)]
pub struct XmChannel {
    /// 1-based instrument index (0 = none).
    pub instrument: u8,
    /// The sample index within that instrument (from sample_map).
    pub sample_in_instr: u8,
    /// Current pattern note (1..=96); 0 = none.
    pub pattern_note: u8,
    /// Cached finetune for the playing sample.
    pub finetune: i8,
    /// Cached relative-note for the playing sample.
    pub relative_note: i8,
    /// Volume 0..=64.
    pub volume: u8,
    /// Shared mixer voice.
    pub voice: MixerVoice,
    /// Last effect command / param.
    pub effect: u8,
    pub effect_param: u8,
}

/// Top-level XM player state.
pub struct XmPlayerState {
    pub instruments: Vec<XmInstrument>,
    pub patterns: Vec<XmPattern>,
    pub order: Vec<u8>,
    pub song_length: u16,
    pub pitch: XmPitch,
    pub channels: Vec<XmChannel>,
    pub speed: u8,
    pub bpm: u8,
    pub sample_rate: u32,

    pub order_index: usize,
    pub row: u16,
    pub tick: u8,
    pub tick_sample_cursor: u32,
    pub ended: bool,
}

impl XmPlayerState {
    pub fn new(
        header: &XmHeader,
        instruments: Vec<XmInstrument>,
        patterns: Vec<XmPattern>,
        sample_rate: u32,
    ) -> Self {
        let pitch = XmPitch {
            table: match header.frequency_table {
                XmFrequencyTable::Amiga => XmPitchTable::Amiga,
                XmFrequencyTable::Linear => XmPitchTable::Linear,
            },
        };
        let n_ch = header.num_channels as usize;
        let channels = (0..n_ch).map(|_| XmChannel::default()).collect();
        let speed = header.default_tempo.max(1) as u8;
        let bpm = header.default_bpm.max(1) as u8;
        let order = header.order.clone();
        XmPlayerState {
            instruments,
            patterns,
            order,
            song_length: header.song_length,
            pitch,
            channels,
            speed,
            bpm,
            sample_rate,
            order_index: 0,
            row: 0,
            tick: 0,
            tick_sample_cursor: 0,
            ended: false,
        }
    }

    pub fn samples_per_tick(&self) -> u32 {
        ((self.sample_rate as f32) * 2.5 / self.bpm as f32).max(1.0) as u32
    }

    fn cell_at(&self, row: u16, ch: usize) -> Option<XmCell> {
        let pat_idx = *self.order.get(self.order_index)? as usize;
        let p = self.patterns.get(pat_idx)?;
        p.rows.get(row as usize)?.get(ch).copied()
    }

    /// Resolve (pattern_note, instrument) into the concrete sample index
    /// within the instrument via its sample_map (per-note routing).
    fn resolve_sample(&self, pattern_note: u8, instrument: u8) -> Option<(usize, usize)> {
        // pattern_note 1..=96 indexes sample_map[note-1].
        if instrument == 0 || pattern_note == 0 || pattern_note > 96 {
            return None;
        }
        let inst_idx = (instrument as usize).checked_sub(1)?;
        let inst = self.instruments.get(inst_idx)?;
        if inst.samples.is_empty() {
            return None;
        }
        let map_idx = (pattern_note - 1) as usize;
        let sample_idx = if map_idx < inst.sample_map.len() {
            inst.sample_map[map_idx] as usize
        } else {
            0
        };
        let sample_idx = sample_idx.min(inst.samples.len().saturating_sub(1));
        Some((inst_idx, sample_idx))
    }

    fn enter_row(&mut self) {
        for ch_idx in 0..self.channels.len() {
            let Some(cell) = self.cell_at(self.row, ch_idx) else {
                continue;
            };

            // Resolve sample indices first (immutable self borrow), then
            // update channel state in a separate mutable borrow.
            let row_pattern_note = self.channels[ch_idx].pattern_note;
            let instrument_change_resolved = if cell.instrument != 0 {
                self.resolve_sample(row_pattern_note.max(49), cell.instrument)
            } else {
                None
            };
            let note_resolved = if cell.has_note() {
                // Use the *new* instrument after the row's instrument
                // change, falling back to the channel's current one.
                let inst = if cell.instrument != 0 {
                    cell.instrument
                } else {
                    self.channels[ch_idx].instrument
                };
                self.resolve_sample(cell.note, inst)
            } else {
                None
            };

            let ch = &mut self.channels[ch_idx];
            ch.effect = cell.effect_type;
            ch.effect_param = cell.effect_param;

            // Instrument change.
            if cell.instrument != 0 {
                ch.instrument = cell.instrument;
                if let Some((i, s)) = instrument_change_resolved {
                    let sample = &self.instruments[i].samples[s];
                    ch.volume = sample.volume.min(64);
                    ch.finetune = sample.finetune;
                    ch.relative_note = sample.relative_note;
                }
            }

            // Volume-column handling.
            match cell.volume_kind() {
                XmVolume::Empty => {}
                XmVolume::SetVolume(v) => {
                    ch.volume = v.min(64);
                }
                _ => {
                    // Slides / vibrato / panning deferred.
                }
            }

            // Note trigger.
            if cell.has_note() {
                ch.pattern_note = cell.note;
                if let Some((i, s)) = note_resolved {
                    let sample = &self.instruments[i].samples[s];
                    ch.finetune = sample.finetune;
                    ch.relative_note = sample.relative_note;
                    if cell.instrument != 0 {
                        ch.volume = sample.volume.min(64);
                    }
                    let real_note = (cell.note as i32 - 1) + ch.relative_note as i32;
                    let freq = self.pitch.note_to_freq((real_note, ch.finetune as i32));
                    ch.sample_in_instr = s as u8;
                    let v = ch.volume as f32 / 64.0;
                    ch.voice.trigger(freq, v);
                }
            } else if cell.is_note_off() {
                ch.voice.active = false;
            }

            apply_tick0_effect(ch);
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
        // Pattern-length comes from the pattern header; use a default of
        // 64 if we can't find the active pattern.
        let pat_len = self
            .order
            .get(self.order_index)
            .and_then(|&o| self.patterns.get(o as usize))
            .map(|p| p.num_rows)
            .unwrap_or(64);
        if self.row >= pat_len {
            self.row = 0;
            self.order_index += 1;
            if self.order_index >= self.song_length as usize
                || self.order_index >= self.order.len()
            {
                self.ended = true;
            }
        }
    }

    pub fn render(&mut self, dst: &mut [i16]) -> usize {
        assert!(dst.len() % 2 == 0);
        let mut produced = 0usize;
        let total_frames = dst.len() / 2;
        let out_rate = self.sample_rate as f32;
        let n_ch = self.channels.len().max(1);
        let headroom = (n_ch as f32 / 2.0).max(1.0);

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
                    if ch.instrument == 0 {
                        continue;
                    }
                    let Some(inst) = self.instruments.get(ch.instrument as usize - 1) else {
                        continue;
                    };
                    let Some(sample) = inst.samples.get(ch.sample_in_instr as usize) else {
                        continue;
                    };
                    let s = ch.voice.render_one(sample, out_rate);
                    // Apply sample-specific panning (XM stores 0..=255).
                    let pan = sample.panning as f32 / 255.0;
                    // Hard L/R pan mirroring MOD when file channel count
                    // exceeds per-sample pan values, but respect
                    // per-sample panning: L = s*(1-pan), R = s*pan.
                    // For XM files that leave panning at 128 (centre),
                    // this gives 0.5/0.5 which mixes everything.
                    let _ = i; // file-channel index not used for panning
                    l += s * (1.0 - pan);
                    r += s * pan;
                }
                let l = (l / headroom).clamp(-1.0, 1.0);
                let r = (r / headroom).clamp(-1.0, 1.0);
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

fn apply_tick0_effect(ch: &mut XmChannel) {
    // 0x0C: Set volume.
    if ch.effect == 0x0C {
        ch.volume = ch.effect_param.min(64);
        ch.voice.volume = ch.volume as f32 / 64.0;
    }
}

fn apply_tickn_effect(ch: &mut XmChannel) {
    let x = ch.effect_param >> 4;
    let y = ch.effect_param & 0x0F;
    // 0x0A: Volume slide.
    if ch.effect == 0x0A {
        if x != 0 {
            ch.volume = (ch.volume as u16 + x as u16).min(64) as u8;
        } else if y != 0 {
            ch.volume = ch.volume.saturating_sub(y);
        }
        ch.voice.volume = ch.volume as f32 / 64.0;
    }
}

// Small helper to quiet an unused-import warning on platforms that don't
// reach the `PingPong` branch in tests.
#[allow(dead_code)]
fn _loop_mode_unused(m: XmSampleLoopMode) -> XmSampleLoopMode { m }
