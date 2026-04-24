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
//!  - Volume + panning envelopes — per-tick linear interpolation between
//!    envelope points, with sustain-point hold and loop-start/loop-end
//!    looping per §"Volume envelope" of FT2-v2.04-xm.txt. The envelope's
//!    `y` value for volume feeds the voice scalar as `env/64`, matching
//!    `FinalVol = FadeOutVol * EnvelopeVol * GlobalVol * Vol` shape.
//!  - Fadeout — on key-off (note 97, or effect Kxx) a 16-bit fadeout
//!    register starting at 65536 is decremented by the instrument's
//!    `volume_fadeout` each tick; it multiplies the voice volume until
//!    it reaches zero, which kills the voice.
//!  - Key-off semantics (note value 97 in the pattern) — the envelope
//!    sustain stall is released and fadeout begins.
//!
//! Not yet implemented (explicit TODO):
//!  - Vibrato (instrument autovibrato and 4xx / Txy).
//!  - Any per-tick effect besides volume slide (Axx) and set-volume.
//!  - Tone portamento.
//!  - Pattern jumps (Bxx / Dxx).
//!  - Exy subcommands, Kxx / Lxx.
//!  - Volume-column slides / tone-porta / vibrato.
//!
//! These are expected follow-on work; the mixer core and pitch trait are
//! already in place, so adding per-format features becomes local.

use crate::mixer::{MixerVoice, PitchModel, XmPitch, XmPitchTable};
use crate::xm::{
    XmCell, XmEnvelope, XmFrequencyTable, XmHeader, XmInstrument, XmPattern, XmSampleLoopMode,
    XmVolume,
};

/// Initial value of the fadeout register per FT2 (the v2.04 HTML
/// annotation: "The FadeOutVol is originally 65535 and is decremented by
/// instrument.fadeout each tick after note is released"). We use 65536 so
/// that dividing by 65536 gives 1.0 exactly for a pre-fadeout voice.
const FADEOUT_MAX: i32 = 65536;

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

    // -------- envelope / fadeout state --------
    /// True while the key is held. False after a key-off event (note 97
    /// or effect Kxx). Controls envelope sustain release + fadeout.
    pub key_on: bool,
    /// Volume envelope tick cursor (frame position on the envelope
    /// x-axis). Advances by one per XM tick.
    pub vol_env_tick: u16,
    /// Last segment index within the envelope's point list that we
    /// interpolated inside. Used to detect loop-end arrival.
    pub vol_env_seg: u8,
    /// Interpolated envelope value, 0..=64, in the last evaluated tick.
    /// Feeds the voice volume multiplier.
    pub vol_env_value: u8,
    /// Panning envelope tick cursor.
    pub pan_env_tick: u16,
    /// Panning envelope segment cursor.
    pub pan_env_seg: u8,
    /// Panning envelope value, 0..=64 (32 = centre).
    pub pan_env_value: u8,
    /// Fadeout multiplier register (0..=65536). Starts at `FADEOUT_MAX`
    /// on note trigger; decremented by `volume_fadeout` per tick after
    /// key-off.
    pub fadeout: i32,
    /// Base sample-header volume captured on trigger, 0..=64. Used so we
    /// can re-derive `voice.volume` each tick after envelope + fadeout
    /// scaling without losing the channel-level Cxx / vol-column value.
    pub base_volume: u8,
    /// Base sample-header panning, 0..=255. Combined with the panning
    /// envelope per XM's `FinalPan` formula.
    pub base_panning: u8,
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
                    ch.base_volume = ch.volume;
                    ch.finetune = sample.finetune;
                    ch.relative_note = sample.relative_note;
                    ch.base_panning = sample.panning;
                }
            }

            // Volume-column handling.
            match cell.volume_kind() {
                XmVolume::Empty => {}
                XmVolume::SetVolume(v) => {
                    ch.volume = v.min(64);
                    ch.base_volume = ch.volume;
                }
                XmVolume::SetPanning(p) => {
                    // Volume-column panning: 0xC0..=0xCF, displayed as
                    // 0..=15, maps to 0..=0xFF (see FT2 volume-column
                    // table).
                    ch.base_panning = (p as u16 * 17).min(255) as u8;
                }
                _ => {
                    // Slides / vibrato / tone-porta deferred.
                }
            }

            // Note trigger / key-off.
            if cell.has_note() {
                ch.pattern_note = cell.note;
                if let Some((i, s)) = note_resolved {
                    let sample = &self.instruments[i].samples[s];
                    ch.finetune = sample.finetune;
                    ch.relative_note = sample.relative_note;
                    if cell.instrument != 0 {
                        ch.volume = sample.volume.min(64);
                        ch.base_volume = ch.volume;
                    }
                    ch.base_panning = sample.panning;
                    let real_note = (cell.note as i32 - 1) + ch.relative_note as i32;
                    let freq = self.pitch.note_to_freq((real_note, ch.finetune as i32));
                    ch.sample_in_instr = s as u8;
                    let v = ch.volume as f32 / 64.0;
                    ch.voice.trigger(freq, v);

                    // Fresh note resets envelope cursors + fadeout + key.
                    ch.key_on = true;
                    ch.vol_env_tick = 0;
                    ch.vol_env_seg = 0;
                    ch.vol_env_value = 64;
                    ch.pan_env_tick = 0;
                    ch.pan_env_seg = 0;
                    ch.pan_env_value = 32;
                    ch.fadeout = FADEOUT_MAX;
                }
            } else if cell.is_note_off() {
                // XM note 97 = key-off. Don't stop the voice; release
                // the envelope sustain and let fadeout take over. If the
                // instrument has no volume envelope, FT2 silences the
                // voice immediately — mirror that so single-sample
                // instruments still behave.
                ch.key_on = false;
                let inst_idx = ch.instrument.saturating_sub(1) as usize;
                let has_vol_env = self
                    .instruments
                    .get(inst_idx)
                    .map(|i| i.volume_envelope.is_on() && !i.volume_envelope.points.is_empty())
                    .unwrap_or(false);
                if !has_vol_env {
                    ch.voice.active = false;
                }
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

        // Envelopes + fadeout run every tick (including tick 0) per the
        // FT2 "envelopes processed once per frame" rule.
        for ch_idx in 0..self.channels.len() {
            let inst_idx = self.channels[ch_idx].instrument as usize;
            if inst_idx == 0 {
                continue;
            }
            let Some(inst) = self.instruments.get(inst_idx - 1) else {
                continue;
            };

            // Collect envelope outputs under an immutable borrow of
            // `inst`, then apply them under a mutable borrow of the
            // channel.
            let vol_env = tick_envelope(
                &inst.volume_envelope,
                self.channels[ch_idx].vol_env_tick,
                self.channels[ch_idx].vol_env_seg,
                self.channels[ch_idx].key_on,
                64,
            );
            let pan_env = tick_envelope(
                &inst.panning_envelope,
                self.channels[ch_idx].pan_env_tick,
                self.channels[ch_idx].pan_env_seg,
                self.channels[ch_idx].key_on,
                32,
            );
            let fadeout_step = inst.volume_fadeout as i32;

            let ch = &mut self.channels[ch_idx];
            // Apply envelope state.
            ch.vol_env_tick = vol_env.next_tick;
            ch.vol_env_seg = vol_env.next_seg;
            ch.vol_env_value = vol_env.value;
            ch.pan_env_tick = pan_env.next_tick;
            ch.pan_env_seg = pan_env.next_seg;
            ch.pan_env_value = pan_env.value;

            // Fadeout: decrements each tick once key is released.
            if !ch.key_on {
                ch.fadeout = (ch.fadeout - fadeout_step).max(0);
                if ch.fadeout == 0 {
                    ch.voice.active = false;
                }
            }

            // Combine base volume * envelope * fadeout into the voice
            // volume scalar. Base volume is already carried by
            // `ch.volume`; we re-derive the voice multiplier each tick
            // so envelope / fadeout apply continuously.
            let env_scalar = if inst.volume_envelope.is_on() && !inst.volume_envelope.points.is_empty() {
                ch.vol_env_value as f32 / 64.0
            } else {
                1.0
            };
            let fade_scalar = ch.fadeout as f32 / FADEOUT_MAX as f32;
            ch.voice.volume =
                (ch.volume as f32 / 64.0) * env_scalar * fade_scalar;
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
                    // XM's FinalPan formula:
                    //   FinalPan = Pan + (EnvelopePan - 32) *
                    //              (128 - |Pan - 128|) / 32
                    // EnvelopePan is 0..=64 (32 = centre). Pan is 0..=255.
                    let pan_base = ch.base_panning as i32;
                    let env_pan = ch.pan_env_value as i32; // 0..=64
                    let range = 128 - (pan_base - 128).abs();
                    let final_pan = pan_base + (env_pan - 32) * range / 32;
                    let final_pan = final_pan.clamp(0, 255) as f32 / 255.0;
                    let _ = i; // file-channel index not used for panning
                    l += s * (1.0 - final_pan);
                    r += s * final_pan;
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
        ch.base_volume = ch.volume;
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
        ch.base_volume = ch.volume;
    }
}

/// Output of a single envelope tick.
struct EnvelopeTick {
    /// Interpolated y-value at the current tick, 0..=64.
    value: u8,
    /// Next tick position on the envelope's x-axis.
    next_tick: u16,
    /// Segment index (index into `points` such that
    /// `points[seg].0 <= next_tick < points[seg+1].0`).
    next_seg: u8,
}

/// Advance an XM envelope by one tick.
///
/// Implements the FT2 envelope rules documented in
/// `FastTracker-2-v2.04-xm.txt` §"Volume envelope" plus the annotations
/// in `FastTracker-2-v2.04-xm.html`:
///
/// - Envelope points are `(tick_x, value_y)` pairs, `y` in `0..=64`.
/// - Within a segment we linearly interpolate between successive points.
/// - If the sustain bit is set and the note is still key-on, stall at
///   `points[sustain_point]` (don't advance `tick`).
/// - If the loop bit is set and the cursor reaches
///   `points[loop_end_point]`, jump back to `points[loop_start_point]`.
/// - Past the last point, hold at that point's value.
///
/// `default_value` is what to return when the envelope is disabled or
/// has no points — 64 for volume (full-scale), 32 for panning (centre).
fn tick_envelope(
    env: &XmEnvelope,
    cur_tick: u16,
    cur_seg: u8,
    key_on: bool,
    default_value: u8,
) -> EnvelopeTick {
    if !env.is_on() || env.points.is_empty() {
        return EnvelopeTick {
            value: default_value,
            next_tick: cur_tick,
            next_seg: cur_seg,
        };
    }

    // Clamp the segment cursor to the valid index range, accounting for
    // a possibly-truncated points vector (parser already caps at 12).
    let n = env.points.len();
    let mut seg = (cur_seg as usize).min(n.saturating_sub(1));
    let mut tick = cur_tick;

    // 1. Evaluate the current (tick, seg) pair.
    let value = eval_envelope_at(&env.points, seg, tick);

    // 2. Compute the next position for the next call.

    // Sustain: if we're at or past the sustain-point tick and the note
    // is still held, stall. FT2 holds *on* the sustain point.
    if env.has_sustain() && key_on {
        let sp = (env.sustain_point as usize).min(n - 1);
        if tick >= env.points[sp].0 {
            // Re-anchor to the sustain point's tick so jitter / overshoot
            // from the initial catch-up doesn't drift.
            tick = env.points[sp].0;
            seg = sp.min(n.saturating_sub(2));
            return EnvelopeTick {
                value,
                next_tick: tick,
                next_seg: seg as u8,
            };
        }
    }

    // Advance one tick.
    tick = tick.saturating_add(1);

    // Loop: if we crossed the loop-end point, snap back to loop-start.
    if env.has_loop() {
        let le = (env.loop_end_point as usize).min(n - 1);
        let ls = (env.loop_start_point as usize).min(le);
        let loop_end_tick = env.points[le].0;
        let loop_start_tick = env.points[ls].0;
        if tick >= loop_end_tick && loop_end_tick > loop_start_tick {
            tick = loop_start_tick;
            seg = ls;
        }
    }

    // Keep `seg` aligned with `tick` — advance until we're in the
    // segment starting at `points[seg].0`.
    while seg + 1 < n && tick >= env.points[seg + 1].0 {
        seg += 1;
    }

    // Past the last point: clamp tick so we don't wrap. FT2 holds at
    // the last point's value indefinitely in this case.
    let last_x = env.points[n - 1].0;
    if tick > last_x {
        tick = last_x;
    }

    EnvelopeTick {
        value,
        next_tick: tick,
        next_seg: seg as u8,
    }
}

/// Evaluate an envelope at (seg, tick) via linear interpolation between
/// `points[seg]` and `points[seg+1]`. If `tick` is past the last point,
/// returns the last point's y-value.
fn eval_envelope_at(points: &[(u16, u16)], seg: usize, tick: u16) -> u8 {
    let n = points.len();
    if n == 0 {
        return 0;
    }
    // Past last point → hold.
    if seg >= n - 1 {
        return points[n - 1].1.min(64) as u8;
    }
    let (x0, y0) = points[seg];
    let (x1, y1) = points[seg + 1];
    if x1 <= x0 {
        return y0.min(64) as u8;
    }
    let t = tick.clamp(x0, x1);
    // Linear interp: y = y0 + (y1-y0) * (t-x0) / (x1-x0).
    let num = (y1 as i32 - y0 as i32) * (t as i32 - x0 as i32);
    let den = (x1 as i32 - x0 as i32).max(1);
    let y = y0 as i32 + num / den;
    y.clamp(0, 64) as u8
}

// Small helper to quiet an unused-import warning on platforms that don't
// reach the `PingPong` branch in tests.
#[allow(dead_code)]
fn _loop_mode_unused(m: XmSampleLoopMode) -> XmSampleLoopMode {
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xm::XmEnvelope;

    fn env_with_points(points: Vec<(u16, u16)>, type_bits: u8) -> XmEnvelope {
        XmEnvelope {
            points,
            sustain_point: 0,
            loop_start_point: 0,
            loop_end_point: 0,
            type_bits,
        }
    }

    #[test]
    fn envelope_disabled_returns_default() {
        let env = env_with_points(vec![(0, 0), (10, 64)], 0);
        let r = tick_envelope(&env, 5, 0, true, 64);
        assert_eq!(r.value, 64);
    }

    #[test]
    fn envelope_linear_interpolates() {
        // (0,0) -> (10,64): at tick 5 we expect ~32.
        let env = env_with_points(vec![(0, 0), (10, 64)], 0x01);
        let r = tick_envelope(&env, 5, 0, true, 64);
        assert_eq!(r.value, 32);
    }

    #[test]
    fn envelope_sustain_holds_while_key_on() {
        // Points: (0,0), (5,64), (10,0). Sustain at point 1 (tick=5).
        let mut env = env_with_points(vec![(0, 0), (5, 64), (10, 0)], 0x01 | 0x02);
        env.sustain_point = 1;
        // At tick 5, key on → value 64, next_tick stays 5.
        let r = tick_envelope(&env, 5, 1, true, 64);
        assert_eq!(r.value, 64);
        assert_eq!(r.next_tick, 5);
        // Released: next call advances past sustain.
        let r = tick_envelope(&env, 5, 1, false, 64);
        assert_eq!(r.next_tick, 6);
    }

    #[test]
    fn envelope_loop_wraps() {
        // Points: (0,0), (5,64), (10,32). Loop 0..2 (ticks 0..10).
        let mut env = env_with_points(vec![(0, 0), (5, 64), (10, 32)], 0x01 | 0x04);
        env.loop_start_point = 0;
        env.loop_end_point = 2;
        // At tick 8 segment 1, advancing goes to tick 9 (no loop yet).
        let r = tick_envelope(&env, 8, 1, true, 64);
        assert_eq!(r.next_tick, 9);
        // At tick 9, advancing hits tick 10 == loop_end, so we wrap to 0.
        let r = tick_envelope(&env, 9, 1, true, 64);
        assert_eq!(r.next_tick, 0);
        assert_eq!(r.next_seg, 0);
    }

    #[test]
    fn envelope_past_last_point_holds() {
        let env = env_with_points(vec![(0, 0), (5, 64)], 0x01);
        let r = tick_envelope(&env, 100, 1, true, 64);
        // Should clamp to the last point's y.
        assert_eq!(r.value, 64);
        assert_eq!(r.next_tick, 5);
    }
}
