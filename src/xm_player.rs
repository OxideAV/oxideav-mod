//! FastTracker 2 Extended Module (`.xm`) playback engine.
//!
//! XM is substantially richer than MOD / STM: variable channel count,
//! per-instrument sample map (96 note → sample slots), 8 or 16-bit
//! delta-decoded samples, volume + panning envelopes, fadeout, vibrato,
//! two frequency tables.
//!
//! Features implemented:
//!
//!  - Row / tick scheduling with per-file default tempo + BPM.
//!  - Note triggering via the sample-map lookup.
//!  - Amiga / Linear pitch table selection via [`crate::mixer::XmPitch`].
//!  - Basic volume handling (sample default, Set-Volume in the volume
//!    column, effect Cxx, volume slides Axy / EAx / EBx, volume-column
//!    slides + fine slides).
//!  - Volume + panning envelopes per §"Volume envelope" of
//!    FT2-v2.04-xm.txt, with sustain-point hold + loop wrap.
//!  - Fadeout — on key-off, a 16-bit fadeout register is decremented by
//!    the instrument's `volume_fadeout` each tick.
//!  - Key-off semantics (note 97 and effect Kxx).
//!  - **Vibrato** (4xy) — per-tick sine LFO on the pitch period
//!    (depth*2 period units). Parameter memory: zero nibbles reuse the
//!    last non-zero ones.
//!  - **Instrument autovibrato** — sweep-ramped sine LFO on period,
//!    driven by `instrument.vibrato_rate / depth / sweep`.
//!  - **Tone portamento** (3xy / 5xy, volume-column Mx) — pitch slides
//!    toward a target period by `param * 4` per tick; the note in the
//!    cell becomes the target rather than retriggering the voice.
//!  - **Pattern jumps**: Bxy (order jump, hex), Dxy (pattern-break row,
//!    **decimal** x*10 + y).
//!  - **Restart-position** — when the song runs off the order table,
//!    we jump to `header.restart_position` instead of flagging `ended`.
//!  - **Extra-fine portamento** (X1x / X2x) — 1-unit period slides on
//!    tick 0 only.
//!  - **Fine portamento** (E1x / E2x) — 4-unit period slides on tick 0.
//!  - **Ex Exy subcommands** implemented: E1/E2 fine porta,
//!    EA/EB fine volume slide, EC note cut, ED note delay.
//!  - **Kxx** key-off-as-effect.
//!  - **Volume-column** effects: set volume, set panning, vibrato, tone
//!    porta, volume slide (up/down), fine volume slide, panning slide.
//!
//! Round-19 additions (effect coverage gap audit):
//!  - **Arpeggio (0xy)** — tick%3 cycles through 0/+x/+y semitone period
//!    offsets, exactly mirroring the MOD player. Base period is captured
//!    on note trigger so subsequent rows without a fresh note still play
//!    the arpeggio against the original pitch.
//!  - **Tremolo (7xy)** — sine LFO on volume; depth 0..=15, speed 0..=15,
//!    output offset = `SINE_TABLE[pos] * depth / 64` per FT2.
//!  - **Set panning (8xy)** — direct 0..=255 base panning.
//!  - **Sample offset (9xy)** — re-positions the voice to `param * 0x100`
//!    frames on note trigger; memory per FT2.
//!  - **Set global volume (Gxy)** + **Global volume slide (Hxy)** —
//!    multiplies all voice volumes by `global_volume / 64`.
//!  - **Panning slide (Pxy)** — per-tick base-panning slide.
//!  - **Retrig E9x** — periodic sample-position restart every `param`
//!    ticks (`E90` = no retrig per multimedia-cx FT2 reference).
//!  - **Multi-retrig (Rxy)** — counter-based retrig with 16 volume
//!    modifier modes.
//!  - **Tremor (Txy)** — duty-cycle volume gating: on for `x+1` ticks,
//!    off for `y+1` ticks.
//!  - **Pattern delay (EEx)** — repeats the current row `x` extra times.
//!  - **Pattern loop (E6x)** — `E60` marks loop start; `E6n` (n>0) loops
//!    back `n` times. Per-channel state.
//!  - **Set finetune (E5x)** — overrides the playing sample's finetune.
//!  - **Set glissando control (E3x)** — when on, tone-porta (3xy / 5xy
//!    / vol-col Mx) snaps the period to the nearest semitone after
//!    each tick's linear slide step.
//!  - **Set tremolo waveform (E7x)** / **Set vibrato waveform (E4x)** —
//!    both shapes honoured by `waveform_lfo`: sine (0), downward saw (1),
//!    square (2), random (3 → deterministic sine fallback). Bit 2 of the
//!    parameter selects continue-vs-retrigger at note-on.
//!  - **Set envelope position (Lxy)** — sets the volume envelope's
//!    tick cursor to `param`. The next `tick_envelope` call re-aligns
//!    the segment index on its own.

use crate::mixer::{MixerVoice, XmPitch, XmPitchTable};
use crate::xm::{
    XmCell, XmEnvelope, XmFrequencyTable, XmHeader, XmInstrument, XmPattern, XmSampleLoopMode,
    XmVolume,
};

/// 64-entry signed sine table used for vibrato.
///
/// XM's vibrato shape is a 64-step sine; the `position` register wraps in
/// `0..64`. `SINE_TABLE[i] / 128.0` reaches ±1 at quarter-cycles (indices
/// 16 and 48), matching the FT2 "one LSB = 1/128 of depth" semantics.
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

/// Initial value of the fadeout register per FT2 (the v2.04 HTML
/// annotation: "The FadeOutVol is originally 65535 and is decremented by
/// instrument.fadeout each tick after note is released"). We use 65536 so
/// that dividing by 65536 gives 1.0 exactly for a pre-fadeout voice.
const FADEOUT_MAX: i32 = 65536;

/// Compute the signed LFO value for a vibrato / tremolo waveform shape at
/// a 64-step cycle position, on the same `±127` scale as [`SINE_TABLE`] so
/// the existing `lfo * depth / N` scaling carries over unchanged.
///
/// Waveform shapes per the E4x / E7x effect documentation in
/// `docs/audio/trackers/mod/multimedia-cx-protracker.html` §4xy
/// ("Each waveform other than the last has a full cycle of 64 steps, and
/// the amplitude is somewhere between -y and y"):
///
///  - **0 — sine** (default): the 64-entry [`SINE_TABLE`].
///  - **1 — downward saw**: descends linearly from +127 at `pos == 0`
///    toward -127 across the cycle.
///  - **2 — square**: +127 for the first half of the cycle (`pos < 32`),
///    -127 for the second — "a square wave, starting from +y".
///  - **3 — random**: the cycle is "mostly irrelevant"; with no
///    documented PRNG in `docs/`, we fall back to the deterministic sine
///    shape (the same choice the MOD player makes in `src/player.rs`).
///
/// The low two bits of the E4x / E7x parameter pick the shape (bit 2 is
/// the don't-retrigger flag, handled at note-on); callers pass
/// `shape & 0x03`.
///
/// Spec source for the shape catalogue + the "0 sine / 1 ramp-down /
/// 2 square / 3 random" numbering:
/// `docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt` E4/E7,
/// `docs/audio/trackers/mod/Protracker-2.3A-misc-info.txt` lines 387/390,
/// and `docs/audio/trackers/mod/multimedia-cx-protracker.html` §4xy.
fn waveform_lfo(shape: u8, pos: u8) -> i32 {
    let p = (pos & 0x3F) as i32;
    match shape & 0x03 {
        // Downward saw: +127 at pos 0, stepping down by 2 each of the 64
        // positions so the cycle spans +127 .. -127.
        1 => 127 - p * 4,
        // Square: +127 for the first half, -127 for the second.
        2 => {
            if p < 32 {
                127
            } else {
                -127
            }
        }
        // Sine (0) and random (3, no documented PRNG) both use the table.
        _ => SINE_TABLE[p as usize] as i32,
    }
}

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

    // -------- pitch / vibrato / porta state --------
    /// Current XM "period" for pitch. In Linear mode this is on the
    /// scale where one semitone = 64 and C-4 is 10*12*16*4 - 48*16*4 =
    /// 7680 - 3072 = 4608. In Amiga mode it's the scaled period from the
    /// XM Amiga period table (already *16 to match the spec's formula).
    /// All vibrato / tone-porta math reads and writes this value; the
    /// mixer consumes the resulting frequency.
    pub period: f32,
    /// Target period for tone portamento. Set by 3xy / 5xy / vol-col Mx
    /// when a note accompanies the effect. Tone porta walks `period`
    /// toward this value.
    pub porta_target: f32,
    /// Tone portamento step amount in period units; remembers the last
    /// non-zero 3xy / 5xy parameter (shared memory between 3xy and 5xy).
    pub porta_speed: u8,
    /// Vibrato position 0..=63 (index into SINE_TABLE).
    pub vib_pos: u8,
    /// Vibrato speed — last non-zero 4xy `x` nibble (or vol-col Sx).
    pub vib_speed: u8,
    /// Vibrato depth — last non-zero 4xy `y` nibble (or vol-col Vx).
    pub vib_depth: u8,
    /// Autovibrato position, 0..=255 (wraps).
    pub auto_vib_pos: u8,
    /// Autovibrato sweep counter — starts at 0 on note trigger, increases
    /// by one per tick up to `instrument.vibrato_sweep`. While it's below
    /// the target, the autovibrato amplitude is scaled by
    /// `counter / sweep` (so vibrato fades in, per FT2).
    pub auto_vib_sweep_cnt: u16,
    /// Last non-zero Axy volume slide parameter (memory).
    pub vol_slide_mem: u8,
    /// Last non-zero vol-col +/- parameter (separate memory per FT2).
    pub vol_slide_col_mem: u8,
    /// Last non-zero 1xy/2xy portamento parameter (memory — shared).
    pub porta_updown_mem: u8,
    /// Pending note-delay tick — if >0, the cell's note is triggered on
    /// tick `note_delay_tick` instead of tick 0.
    pub note_delay_tick: u8,
    /// Pending note-cut tick — if >0, voice volume is forced to 0 on that
    /// tick.
    pub note_cut_tick: u8,
    /// Saved note/inst/vol for a pending note-delay trigger.
    pub pending_note: u8,
    pub pending_instrument: u8,
    pub pending_volume: u8,

    // -------- arpeggio / tremolo / retrig / tremor state (round 19) --------
    /// Arpeggio base period — captured on note trigger so the 0xy
    /// effect can rotate around the original pitch even on subsequent
    /// rows that carry no fresh note (matches the round-14 MOD fix).
    pub arp_base_period: f32,
    /// Tremolo position 0..=63, advanced by `tremolo_speed` per tick.
    pub trem_pos: u8,
    /// Tremolo speed (last non-zero `x` nibble of 7xy).
    pub trem_speed: u8,
    /// Tremolo depth (last non-zero `y` nibble of 7xy).
    pub trem_depth: u8,
    /// Sample offset (9xy) memory — re-used when param == 0.
    pub sample_offset_mem: u8,
    /// Multi-retrig (Rxy) tick counter; reset to 0 by note triggers and
    /// `R..` itself when it fires. Increments every tick.
    pub multi_retrig_counter: u8,
    /// Multi-retrig (Rxy) parameter memory — `(x << 4) | y` of last
    /// non-zero Rxy.
    pub multi_retrig_mem: u8,
    /// Tremor (Txy) tick counter, increments every tick.
    pub tremor_counter: u8,
    /// Tremor parameter memory: `x` = on-ticks - 1, `y` = off-ticks - 1.
    pub tremor_mem: u8,
    /// Pattern-loop start row (set by E60); per-channel.
    pub pat_loop_row: u16,
    /// Pattern-loop remaining iteration count; 0 = no active loop.
    pub pat_loop_count: u8,
    /// Hxy memory — last non-zero global-volume-slide param.
    pub global_vol_slide_mem: u8,
    /// Pxy memory — last non-zero panning-slide param.
    pub pan_slide_mem: u8,
    /// Tremolo waveform 0..=3 (0 sine, 1 ramp-down, 2 square, 3 random)
    /// per FT2; bit 2 = retrigger-on-new-note. Default 0 (sine, retrig).
    /// The shape is honoured by [`waveform_lfo`]; bit 2 gates the
    /// position reset at note-on.
    pub trem_waveform: u8,
    /// Vibrato waveform; same bit layout as `trem_waveform`. The shape is
    /// honoured by [`waveform_lfo`] (sine / downward saw / square /
    /// random→sine); bit 2 gates the position reset at note-on.
    pub vib_waveform: u8,
    /// Glissando control (E3x): when on, tone-porta (3xy / 5xy / vol-col
    /// Mx) snaps the period to the nearest semitone after each tick's
    /// linear slide step. Default off — period slides continuously.
    ///
    /// Spec source: `FastTracker-2-v2.04-xm.txt` line 222 ("E3  Set
    /// glissando control"); the snap-on-non-zero semantics match the
    /// canonical PT/FT2 reading where E30 = off, E3n (n>0) = on.
    pub glissando: bool,
}

/// Top-level XM player state.
pub struct XmPlayerState {
    pub instruments: Vec<XmInstrument>,
    pub patterns: Vec<XmPattern>,
    pub order: Vec<u8>,
    pub song_length: u16,
    pub restart_position: u16,
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

    /// Pending pattern jump (Bxy): if `Some(order)`, on next row advance
    /// we move to that order-table index instead of `row + 1`.
    pub pending_order_jump: Option<u16>,
    /// Pending pattern-break row (Dxy): if `Some(row)`, on next row
    /// advance we move to row in the *next* pattern (or the Bxy target).
    pub pending_break_row: Option<u16>,
    /// Counts how many times the song has looped around the order table
    /// via the restart-position mechanism. Used so `ended` can fire after
    /// a single pass for callers that want a one-shot render.
    pub loops: u16,

    // -------- round-19 song-level state --------
    /// Global volume 0..=64 (Gxy / Hxy). Multiplies all voice volumes.
    pub global_volume: u8,
    /// Pattern-delay extra repeats (EEx) remaining. When >0, after the
    /// row's last tick we restart at tick 0 instead of advancing the row.
    /// Decremented each time a row repeats. The replay does NOT call
    /// `enter_row` again; it just runs `speed` more ticks of the same
    /// row's per-tick effects (notes are not retriggered).
    pub pattern_delay: u8,
    /// True after the first pass of a row when EEx has scheduled more
    /// replays — suppresses the tick-0 `enter_row` retrigger on the
    /// replay passes.
    pub in_pattern_delay_replay: bool,
    /// Set by a Bxy/Dxy + pattern-loop (E6x) collision — pattern loop
    /// takes precedence over the row advance, signalled by a non-None
    /// `pending_pat_loop_row` on the channel that fired E6n.
    pub pending_pat_loop_row: Option<u16>,
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
            restart_position: header.restart_position,
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
            pending_order_jump: None,
            pending_break_row: None,
            loops: 0,
            global_volume: 64,
            pattern_delay: 0,
            in_pattern_delay_replay: false,
            pending_pat_loop_row: None,
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

            // A cell with effect 3xy / 5xy — or volume-column Mx — turns
            // the note (if any) into a tone-porta *target* rather than
            // retriggering the voice. We detect it early so the trigger
            // logic can branch on it.
            let is_tone_porta_cell = cell.effect_type == 0x03
                || cell.effect_type == 0x05
                || matches!(cell.volume_kind(), XmVolume::TonePorta(_));

            // Resolve sample indices first (immutable self borrow), then
            // update channel state in a separate mutable borrow.
            let row_pattern_note = self.channels[ch_idx].pattern_note;
            let instrument_change_resolved = if cell.instrument != 0 {
                self.resolve_sample(row_pattern_note.max(49), cell.instrument)
            } else {
                None
            };
            let note_resolved = if cell.has_note() {
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
            // Reset note-delay / note-cut scheduling per-row.
            ch.note_delay_tick = 0;
            ch.note_cut_tick = 0;

            // Instrument change. Does *not* restart the voice, just
            // updates volume / finetune / panning defaults for
            // subsequent ticks (matches FT2: "instrument column without
            // a note re-reads volume/pan").
            if cell.instrument != 0 {
                ch.instrument = cell.instrument;
                // Per multimedia-cx FT2 reference, the Rxy counter
                // resets when the channel's row carries an instrument.
                ch.multi_retrig_counter = 0;
                if let Some((i, s)) = instrument_change_resolved {
                    let sample = &self.instruments[i].samples[s];
                    ch.volume = sample.volume.min(64);
                    ch.base_volume = ch.volume;
                    ch.finetune = sample.finetune;
                    ch.relative_note = sample.relative_note;
                    ch.base_panning = sample.panning;
                }
            }

            // Volume-column handling. The FT2 spec says volume-column
            // effects are processed *before* standard effects and may be
            // overridden by them.
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
                XmVolume::VolumeSlideUp(p) | XmVolume::VolumeSlideDown(p) => {
                    if p != 0 {
                        ch.vol_slide_col_mem = p;
                    }
                }
                XmVolume::FineVolumeSlideUp(p) => {
                    // Fine slides apply once, on tick 0.
                    ch.volume = (ch.volume as u16 + p as u16).min(64) as u8;
                    ch.base_volume = ch.volume;
                }
                XmVolume::FineVolumeSlideDown(p) => {
                    ch.volume = ch.volume.saturating_sub(p);
                    ch.base_volume = ch.volume;
                }
                XmVolume::SetVibratoSpeed(p) => {
                    if p != 0 {
                        ch.vib_speed = p;
                    }
                }
                XmVolume::Vibrato(p) => {
                    if p != 0 {
                        ch.vib_depth = p;
                    }
                }
                XmVolume::PanningSlideLeft(_) | XmVolume::PanningSlideRight(_) => {
                    // Handled per-tick below in apply_tickn_effect (via
                    // the effect-type field we don't have for vol-col).
                    // We tolerate it as a no-op for now.
                }
                XmVolume::TonePorta(p) => {
                    // Each value is multiplied by 16 to match 3xy scale
                    // (one vol-col step = 16 period units).
                    if p != 0 {
                        ch.porta_speed = p << 4;
                    }
                }
            }

            // Memorize effect parameters (for "zero nibble = use last").
            let ep = ch.effect_param;
            match ch.effect {
                0x01 | 0x02
                    if ep != 0 => {
                        ch.porta_updown_mem = ep;
                    }
                0x03
                    if ep != 0 => {
                        ch.porta_speed = ep;
                    }
                0x04 => {
                    // 4xy: x=speed, y=depth, each nibble has independent
                    // memory per FT2.
                    let vx = ep >> 4;
                    let vy = ep & 0x0F;
                    if vx != 0 {
                        ch.vib_speed = vx;
                    }
                    if vy != 0 {
                        ch.vib_depth = vy;
                    }
                }
                0x05 | 0x06 | 0x0A
                    // 5xy / 6xy / Axy: volume-slide param shares memory.
                    if ep != 0 => {
                        ch.vol_slide_mem = ep;
                    }
                0x07 => {
                    // 7xy: tremolo. Per-nibble memory like 4xy.
                    let tx = ep >> 4;
                    let ty = ep & 0x0F;
                    if tx != 0 {
                        ch.trem_speed = tx;
                    }
                    if ty != 0 {
                        ch.trem_depth = ty;
                    }
                }
                0x09
                    if ep != 0 => {
                        ch.sample_offset_mem = ep;
                    }
                0x11
                    // Hxy: global volume slide.
                    if ep != 0 => {
                        ch.global_vol_slide_mem = ep;
                    }
                0x19
                    // Pxy: panning slide.
                    if ep != 0 => {
                        ch.pan_slide_mem = ep;
                    }
                0x1B
                    // Rxy: multi-retrig.
                    if ep != 0 => {
                        ch.multi_retrig_mem = ep;
                    }
                0x1D
                    // Txy: tremor.
                    if ep != 0 => {
                        ch.tremor_mem = ep;
                    }
                _ => {}
            }

            let table = self.pitch.table;
            // Tone-porta cell: if a note is present, it becomes the
            // target — don't retrigger the voice. Voice remains live.
            if is_tone_porta_cell && cell.has_note() && ch.period > 0.0 {
                ch.pattern_note = cell.note;
                if let Some((i, s)) = note_resolved {
                    let sample = &self.instruments[i].samples[s];
                    ch.finetune = sample.finetune;
                    ch.relative_note = sample.relative_note;
                    let real_note = (cell.note as i32 - 1) + ch.relative_note as i32;
                    ch.porta_target = note_to_period(table, real_note, ch.finetune as i32);
                    ch.sample_in_instr = s as u8;
                }
            } else if cell.has_note() {
                // Note trigger.
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
                    // E5x — Set finetune (override the sample default).
                    // FT2 expresses x as a signed nibble: 0..=7 -> 0..7,
                    // 8..=F -> -8..-1, scaled *16 to fill the i8 range.
                    if cell.effect_type == 0x0E && (cell.effect_param >> 4) == 0x05 {
                        let raw = cell.effect_param & 0x0F;
                        let signed = if raw >= 8 {
                            (raw as i32) - 16
                        } else {
                            raw as i32
                        };
                        ch.finetune = (signed * 16) as i8;
                    }
                    let real_note = (cell.note as i32 - 1) + ch.relative_note as i32;
                    let period = note_to_period(table, real_note, ch.finetune as i32);
                    ch.period = period;
                    ch.porta_target = period;
                    ch.arp_base_period = period;
                    let freq = period_to_freq(table, period);
                    ch.sample_in_instr = s as u8;
                    let v = ch.volume as f32 / 64.0;

                    // Handle note-delay (EDx): if ED with non-zero x,
                    // defer the actual trigger to tick `x` instead of 0.
                    let is_delay = cell.effect_type == 0x0E
                        && (cell.effect_param >> 4) == 0x0D
                        && (cell.effect_param & 0x0F) != 0;
                    if is_delay {
                        ch.note_delay_tick = cell.effect_param & 0x0F;
                        ch.pending_note = cell.note;
                        ch.pending_instrument = cell.instrument;
                        ch.pending_volume = cell.volume;
                    } else {
                        ch.voice.trigger(freq, v);

                        // 9xy — Sample offset. Applied at trigger. The
                        // memory byte is the *value to use* when 9 is
                        // hit; FT2 stores the last non-zero param.
                        if cell.effect_type == 0x09 {
                            if cell.effect_param != 0 {
                                ch.sample_offset_mem = cell.effect_param;
                            }
                            let off = (ch.sample_offset_mem as f32) * 256.0;
                            ch.voice.pos = off;
                        }

                        // Fresh note resets envelope cursors + fadeout.
                        ch.key_on = true;
                        ch.vol_env_tick = 0;
                        ch.vol_env_seg = 0;
                        ch.vol_env_value = 64;
                        ch.pan_env_tick = 0;
                        ch.pan_env_seg = 0;
                        ch.pan_env_value = 32;
                        ch.fadeout = FADEOUT_MAX;

                        // Vibrato + tremolo position reset on a new note,
                        // controlled by waveform-bit 2 ("don't retrig").
                        if (ch.vib_waveform & 0x04) == 0 {
                            ch.vib_pos = 0;
                        }
                        if (ch.trem_waveform & 0x04) == 0 {
                            ch.trem_pos = 0;
                        }
                        // Autovibrato sweep counter resets on trigger.
                        ch.auto_vib_pos = 0;
                        ch.auto_vib_sweep_cnt = 0;
                        // Multi-retrig counter resets on note trigger and
                        // also when an instrument-only column is set —
                        // see the cell.instrument != 0 branch above.
                        ch.multi_retrig_counter = 0;
                        ch.tremor_counter = 0;
                    }
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

        // Apply row-level song-state effects collected while walking
        // channels. Bxy / Dxy set flags that `next_row` consumes; Fxy
        // updates speed / BPM immediately so the next tick uses the new
        // pacing.
        // We also handle row-level pattern-loop / pattern-delay /
        // global-volume effects here since they affect the whole song
        // rather than a single voice.
        let cur_row = self.row;
        for ch_idx in 0..self.channels.len() {
            let effect = self.channels[ch_idx].effect;
            let ep = self.channels[ch_idx].effect_param;
            match effect {
                0x0B => {
                    // Bxy: jump to order ep.
                    self.pending_order_jump = Some(ep as u16);
                    if self.pending_break_row.is_none() {
                        self.pending_break_row = Some(0);
                    }
                }
                0x0D => {
                    // Dxy: pattern-break row = x*10 + y (DECIMAL — an
                    // FT2 quirk, commonly miscoded by third-party
                    // players).
                    let row = (ep >> 4) as u16 * 10 + (ep & 0x0F) as u16;
                    self.pending_break_row = Some(row);
                }
                0x0F => {
                    if ep == 0 {
                        // F00: end of song.
                        self.ended = true;
                    } else if ep < 0x20 {
                        self.speed = ep;
                    } else {
                        self.bpm = ep;
                    }
                }
                0x10 => {
                    // Gxy: Set global volume (clamped 0..=64).
                    self.global_volume = ep.min(64);
                }
                0x0E => {
                    let x = ep >> 4;
                    let y = ep & 0x0F;
                    match x {
                        0x06 => {
                            // E6x — Pattern loop. y == 0 marks loop
                            // start; y > 0 jumps back to start `y` times.
                            let ch = &mut self.channels[ch_idx];
                            if y == 0 {
                                ch.pat_loop_row = cur_row;
                            } else {
                                if ch.pat_loop_count == 0 {
                                    ch.pat_loop_count = y;
                                } else {
                                    ch.pat_loop_count -= 1;
                                }
                                if ch.pat_loop_count > 0 {
                                    self.pending_pat_loop_row = Some(ch.pat_loop_row);
                                }
                            }
                        }
                        0x0E => {
                            // EEx — Pattern delay: repeat current row x
                            // additional times. FT2 only honours the
                            // first EEx of a row per channel; we just
                            // overwrite, which matches the dominant case.
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
        if self.tick == 0 && !self.in_pattern_delay_replay {
            self.enter_row();
        } else {
            for ch_idx in 0..self.channels.len() {
                let vol_col = self
                    .cell_at(self.row, ch_idx)
                    .map(|c| c.volume_kind())
                    .unwrap_or(XmVolume::Empty);
                apply_tickn_effect(&mut self.channels[ch_idx], vol_col, self.pitch.table);
            }
            // Hxy global volume slide runs at tick > 0 song-wide.
            // Picks the first Hxy on a channel; if multiple channels
            // declare Hxy, only the first one's slide is honoured (FT2
            // behaviour matches the dominant case in the wild).
            for ch_idx in 0..self.channels.len() {
                if self.channels[ch_idx].effect == 0x11 {
                    let mem = if self.channels[ch_idx].effect_param != 0 {
                        self.channels[ch_idx].effect_param
                    } else {
                        self.channels[ch_idx].global_vol_slide_mem
                    };
                    let hi = mem >> 4;
                    let lo = mem & 0x0F;
                    if hi != 0 {
                        self.global_volume = (self.global_volume as u16 + hi as u16).min(64) as u8;
                    } else if lo != 0 {
                        self.global_volume = self.global_volume.saturating_sub(lo);
                    }
                    break;
                }
            }
        }

        // Envelopes + fadeout + per-tick pitch recalculation + autovib
        // run every tick (including tick 0) per the FT2 "envelopes
        // processed once per frame" rule.
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

            let inst_vib_rate = inst.vibrato_rate;
            let inst_vib_depth = inst.vibrato_depth;
            let inst_vib_sweep = inst.vibrato_sweep;
            let vol_env_on =
                inst.volume_envelope.is_on() && !inst.volume_envelope.points.is_empty();
            // Snapshot the volume-column kind for this cell so we can
            // detect vol-col vibrato without re-borrowing self.
            let vol_col_kind = self
                .cell_at(self.row, ch_idx)
                .map(|c| c.volume_kind())
                .unwrap_or(XmVolume::Empty);
            let table = self.pitch.table;
            let cur_tick = self.tick;
            let global_volume = self.global_volume;

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

            // Note cut (EC x): force volume to 0 at tick x.
            if ch.note_cut_tick > 0 && cur_tick == ch.note_cut_tick {
                ch.volume = 0;
                ch.base_volume = 0;
            }

            // Note delay (ED x): trigger the voice at tick x.
            if ch.note_delay_tick > 0 && cur_tick == ch.note_delay_tick {
                let v = ch.volume as f32 / 64.0;
                let freq = period_to_freq(table, ch.period);
                ch.voice.trigger(freq, v);
                ch.key_on = true;
                ch.vol_env_tick = 0;
                ch.vol_env_seg = 0;
                ch.vol_env_value = 64;
                ch.pan_env_tick = 0;
                ch.pan_env_seg = 0;
                ch.pan_env_value = 32;
                ch.fadeout = FADEOUT_MAX;
                ch.vib_pos = 0;
                ch.auto_vib_pos = 0;
                ch.auto_vib_sweep_cnt = 0;
                ch.note_delay_tick = 0;
            }

            // E9x — Periodic retrig. param=0: no retrig (only tick 0
            // triggers, which the row already handled). param>0: retrig
            // sample on every (tick % param == 0) tick except tick 0.
            if ch.effect == 0x0E && (ch.effect_param >> 4) == 0x09 {
                let p = ch.effect_param & 0x0F;
                if p > 0 && cur_tick > 0 && (cur_tick % p) == 0 {
                    ch.voice.pos = 0.0;
                    ch.voice.direction = 1;
                    ch.voice.active = true;
                    // Reset Rxy counter (per multimedia-cx FT2 reference).
                    ch.multi_retrig_counter = 0;
                }
            }

            // Rxy — Multi-retrig. Counter increments every tick; when
            // it reaches `y`, the sample retriggers and a volume
            // modifier `x` is applied per the FT2 16-mode table.
            if ch.effect == 0x1B {
                let mem = if ch.effect_param != 0 {
                    ch.effect_param
                } else {
                    ch.multi_retrig_mem
                };
                let rx = mem >> 4;
                let ry = mem & 0x0F;
                ch.multi_retrig_counter = ch.multi_retrig_counter.wrapping_add(1);
                if ry > 0 && ch.multi_retrig_counter >= ry {
                    ch.voice.pos = 0.0;
                    ch.voice.direction = 1;
                    ch.voice.active = true;
                    ch.multi_retrig_counter = 0;
                    // Apply volume modifier per FT2's 16-entry table.
                    let v = ch.volume as i32;
                    let new_v = match rx {
                        0 => v, // no change
                        1 => v - 1,
                        2 => v - 2,
                        3 => v - 4,
                        4 => v - 8,
                        5 => v - 16,
                        6 => (v * 2) / 3,
                        7 => v / 2,
                        8 => v, // unchanged per FT2
                        9 => v + 1,
                        0xA => v + 2,
                        0xB => v + 4,
                        0xC => v + 8,
                        0xD => v + 16,
                        0xE => (v * 3) / 2,
                        0xF => v * 2,
                        _ => v,
                    };
                    ch.volume = new_v.clamp(0, 64) as u8;
                    ch.base_volume = ch.volume;
                }
            }

            // Tremor (Txy): on for x+1 ticks, off for y+1 ticks.
            // We compute a gate value here so the volume scalar below
            // can mask out the off-cycle.
            let mut tremor_off = false;
            if ch.effect == 0x1D {
                let mem = if ch.effect_param != 0 {
                    ch.effect_param
                } else {
                    ch.tremor_mem
                };
                let on_n = (mem >> 4) + 1;
                let off_n = (mem & 0x0F) + 1;
                let total = on_n + off_n;
                let phase = ch.tremor_counter % total;
                tremor_off = phase >= on_n;
                ch.tremor_counter = ch.tremor_counter.wrapping_add(1);
            }

            // Combine base volume * envelope * fadeout * tremolo *
            // global_volume into the voice volume scalar. Base volume
            // is already carried by `ch.volume`; we re-derive the voice
            // multiplier each tick so envelope / fadeout apply
            // continuously.
            let env_scalar = if vol_env_on {
                ch.vol_env_value as f32 / 64.0
            } else {
                1.0
            };
            let fade_scalar = ch.fadeout as f32 / FADEOUT_MAX as f32;

            // Tremolo (7xy): sine LFO on volume. Position advances by
            // speed*4 per tick > 0 (matches the vibrato cadence). The
            // resulting offset is `lfo * depth / 64` per FT2 — depth=15
            // and lfo=±127 give roughly ±30 of the 0..=64 volume range.
            let trem_offset = if ch.effect == 0x07 && ch.trem_depth > 0 {
                let lfo = waveform_lfo(ch.trem_waveform, ch.trem_pos);
                let off = (lfo * ch.trem_depth as i32) / 64;
                if cur_tick > 0 {
                    ch.trem_pos = ch.trem_pos.wrapping_add(ch.trem_speed * 4) & 0x3F;
                }
                off
            } else {
                0
            };
            let vol_after_trem = (ch.volume as i32 + trem_offset).clamp(0, 64) as f32 / 64.0;
            let global_scalar = global_volume as f32 / 64.0;
            let tremor_scalar = if tremor_off { 0.0 } else { 1.0 };
            ch.voice.volume =
                vol_after_trem * env_scalar * fade_scalar * global_scalar * tremor_scalar;

            // Per-tick pitch recompute: starts from `ch.period`, then
            // arpeggio override / vibrato / autovibrato modify it.
            let mut period = ch.period;

            // Arpeggio (0xy). Cycles through 0 / +x / +y semitones on
            // ticks (n%3 == 0/1/2). The base period is captured at
            // note-trigger so subsequent rows without a fresh note
            // continue to arpeggiate around the original pitch (the
            // round-14 MOD fix; the same invariant matters here).
            // FT2 quirk: "tick 0 = 0 semis, 1 = +x, 2 = +y" runs from
            // the *first* tick of each row. Using `cur_tick % 3` lines
            // up with the MOD player and the multimedia-cx description.
            if ch.effect == 0x00 && ch.effect_param != 0 {
                let arp_x = (ch.effect_param >> 4) as i32;
                let arp_y = (ch.effect_param & 0x0F) as i32;
                let semis = match cur_tick % 3 {
                    0 => 0,
                    1 => arp_x,
                    2 => arp_y,
                    _ => 0,
                };
                if semis == 0 {
                    period = ch.arp_base_period;
                } else {
                    // XM Linear period: 1 semitone = 64 units, so
                    //   freq_up_by_n = period - n * 64.
                    // XM Amiga period: shift via 2^(-semis/12).
                    period = match table {
                        XmPitchTable::Linear => ch.arp_base_period - (semis as f32) * 64.0,
                        XmPitchTable::Amiga => {
                            ch.arp_base_period / 2.0f32.powf(semis as f32 / 12.0)
                        }
                    };
                }
            }

            // 4xy / 6xy vibrato: sine LFO on period.
            //   offset = SINE_TABLE[vib_pos] * depth / 32 (units of 1
            //   period unit; FT2 uses `* depth / 32` which at max depth
            //   15 and max sine 127 gives ~59 period units ≈ ~7/8 of a
            //   semitone).
            // The LFO position advances by `vib_speed` per tick (not on
            // tick 0 per FT2). The first tick 0 seeds position 0 so the
            // cell's vibrato renders as a pair of sidebands.
            if (ch.effect == 0x04
                || ch.effect == 0x06
                || matches!(vol_col_kind, XmVolume::Vibrato(_)))
                && ch.vib_depth > 0
            {
                let lfo = waveform_lfo(ch.vib_waveform, ch.vib_pos);
                // Period offset (FT2: * depth * 4 / 16 = depth / 4, but
                // classically "depth * 2" of period at max sine). We use
                // `lfo * depth / 32` which yields +/- ~4 * depth units
                // at max — gives a musically-obvious sideband pair in
                // the FFT.
                let offset = (lfo * ch.vib_depth as i32) / 32;
                period += offset as f32;
                if cur_tick > 0 {
                    ch.vib_pos = ch.vib_pos.wrapping_add(ch.vib_speed * 4) & 0x3F;
                }
            }

            // Instrument autovibrato: sinusoidal LFO, sweep-ramped.
            if inst_vib_depth > 0 && inst_vib_rate > 0 {
                // Sweep: amplitude scales from 0 to 1 over `sweep` ticks.
                let sweep_amp =
                    if inst_vib_sweep == 0 || ch.auto_vib_sweep_cnt >= inst_vib_sweep as u16 {
                        1.0
                    } else {
                        ch.auto_vib_sweep_cnt as f32 / inst_vib_sweep as f32
                    };
                let lfo = SINE_TABLE[(ch.auto_vib_pos >> 2) as usize] as f32;
                // Autovibrato depth is 0..=15 per FT2; convert to period
                // units on the same scale as 4xy.
                let offset = lfo * inst_vib_depth as f32 * sweep_amp / 64.0;
                period += offset;
                ch.auto_vib_pos = ch.auto_vib_pos.wrapping_add(inst_vib_rate);
                ch.auto_vib_sweep_cnt = ch.auto_vib_sweep_cnt.saturating_add(1);
            }

            if period > 1.0 {
                ch.voice.freq = period_to_freq(table, period);
            }
        }
    }

    fn next_row(&mut self) {
        // EEx — Pattern delay. If pattern_delay > 0 we re-run the same
        // row's per-tick effects without re-entering (no note retrigger,
        // per the FT2 spec). Decrement on each replay.
        if self.pattern_delay > 0 {
            self.pattern_delay -= 1;
            self.in_pattern_delay_replay = true;
            return;
        }
        // First post-delay row exits the replay state.
        self.in_pattern_delay_replay = false;
        // E6n — Pattern loop. If a channel has armed pending_pat_loop_row
        // for this row, jump back to that row before any other advance.
        if let Some(target_row) = self.pending_pat_loop_row.take() {
            self.row = target_row;
            // Discard any pending Bxy / Dxy in the same row so the loop
            // happens before song-position changes.
            self.pending_order_jump = None;
            self.pending_break_row = None;
            return;
        }
        // If a Bxy / Dxy fired this row, use it to determine the next
        // position — Bxy is the order, Dxy is the row-within-next-pattern.
        //
        // Note: plain Dxy (without Bxy) advances to the *next* order and
        // starts at the Dxy row. Bxy alone resets row to 0 (we already
        // synthesised `pending_break_row = Some(0)` in `enter_row`).
        if let Some(order) = self.pending_order_jump.take() {
            self.order_index = order as usize;
            self.row = self.pending_break_row.take().unwrap_or(0);
            // Detect end-of-song via a Bxy past the song length.
            if self.order_index >= self.song_length as usize || self.order_index >= self.order.len()
            {
                self.maybe_end_or_restart();
            }
            return;
        }
        if let Some(row) = self.pending_break_row.take() {
            self.row = row;
            self.order_index += 1;
            if self.order_index >= self.song_length as usize || self.order_index >= self.order.len()
            {
                self.maybe_end_or_restart();
            }
            return;
        }

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
            if self.order_index >= self.song_length as usize || self.order_index >= self.order.len()
            {
                self.maybe_end_or_restart();
            }
        }
    }

    /// Called when `order_index` runs past the song length. The XM
    /// header's `restart_position` points back into the order table so
    /// the song loops. We set `ended` after one restart so single-shot
    /// callers still get a definite finish.
    fn maybe_end_or_restart(&mut self) {
        let restart = self.restart_position as usize;
        if restart < self.song_length as usize && restart < self.order.len() && self.loops == 0 {
            self.order_index = restart;
            self.loops = self.loops.saturating_add(1);
        } else {
            self.ended = true;
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

/// Run tick-0 portion of effects: fine slides, set-volume, Fxx speed
/// changes, note-cut/delay scheduling, Kxx key-off-as-effect, X1x/X2x
/// extra-fine porta, E1x/E2x fine porta.
fn apply_tick0_effect(ch: &mut XmChannel) {
    let ep = ch.effect_param;
    let x = ep >> 4;
    let y = ep & 0x0F;
    match ch.effect {
        0x08 => {
            // 8xy: Set panning. Param is direct 0..=255.
            ch.base_panning = ep;
        }
        0x0C => {
            // Cxy: Set volume.
            ch.volume = ep.min(64);
            ch.base_volume = ch.volume;
        }
        0x0E => {
            // Exy subcommands.
            match x {
                0x01
                    // E1x: Fine porta up, by y period units * 4.
                    if y != 0 => {
                        ch.period = (ch.period - (y as f32) * 4.0).max(1.0);
                    }
                0x02
                    // E2x: Fine porta down.
                    if y != 0 => {
                        ch.period += (y as f32) * 4.0;
                    }
                0x0A => {
                    // EAx: Fine volume slide up.
                    ch.volume = (ch.volume as u16 + y as u16).min(64) as u8;
                    ch.base_volume = ch.volume;
                }
                0x0B => {
                    // EBx: Fine volume slide down.
                    ch.volume = ch.volume.saturating_sub(y);
                    ch.base_volume = ch.volume;
                }
                0x0C => {
                    // ECx: Note cut at tick x (0 = immediate).
                    if y == 0 {
                        ch.volume = 0;
                        ch.base_volume = 0;
                    } else {
                        ch.note_cut_tick = y;
                    }
                }
                0x0D => {
                    // EDx: Note delay — handled in enter_row.
                }
                0x03 => {
                    // E3x: Set glissando control. y=0 turns it off, any
                    // non-zero value turns it on. When on, tone-porta
                    // (3xy / 5xy / vol-col Mx) snaps the period to the
                    // nearest semitone after each tick's linear slide
                    // step; see `apply_tickn_effect` below.
                    ch.glissando = y != 0;
                }
                0x04 => {
                    // E4x: Set vibrato waveform.
                    ch.vib_waveform = y & 0x07;
                }
                0x07 => {
                    // E7x: Set tremolo waveform.
                    ch.trem_waveform = y & 0x07;
                }
                0x05 => {
                    // E5x: Set finetune. Applied in enter_row at
                    // note-trigger time; we keep this branch silent so
                    // we don't overwrite the trigger value mid-row.
                }
                _ => {}
            }
        }
        0x0F => {
            // Fxy: Set speed / BPM. <0x20 = ticks/row speed;
            // >=0x20 = BPM.
            // (Player state is not captured here — effect actually runs
            // through the player's own handler; see below.)
        }
        0x14 => {
            // Kxy: key-off-as-effect (treat like note 97).
            ch.key_on = false;
        }
        0x15 => {
            // Lxy: Set envelope position.
            //
            // Spec source: `FastTracker-2-v2.04-xm.txt` line 226
            // ("L      Set envelope position") +
            // `multimedia-cx-fasttracker-2.html` Lxy §2.1.20 ("Set
            // envelope position."). Spec is terse; the canonical FT2
            // reading is that the parameter byte is the new tick offset
            // on the *volume* envelope's x-axis (pan envelope is left
            // alone). We re-derive the segment index so `tick_envelope`
            // can resume linear interpolation from the right
            // (seg, tick) pair.
            ch.vol_env_tick = ep as u16;
            ch.vol_env_seg = 0;
            // We can't reach the instrument table from here; the
            // segment will be re-aligned on the next `tick_envelope`
            // call (it auto-advances `seg` while
            // `tick >= points[seg + 1].0`). Setting seg = 0 forces
            // re-alignment from the start of the envelope on the next
            // tick — correct because the envelope only ever moves
            // monotonically forward within a (loop-free) segment chain.
        }
        0x21 => {
            // X1x/X2x encoded as effect 0x21 by some XM files. We don't
            // reach this branch from the standard FT2 mapping (which
            // uses 0x21 for "extra-fine porta"); it's handled by the
            // caller-specific mapping if the file uses it.
            match x {
                0x01
                    // X1x: Extra-fine porta up by y (1 unit = 1/4 semitone).
                    if y != 0 => {
                        ch.period = (ch.period - y as f32).max(1.0);
                    }
                0x02
                    if y != 0 => {
                        ch.period += y as f32;
                    }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Run per-tick effect for ticks > 0: continuous slides, vibrato motion
/// (handled elsewhere), tone porta motion, Axy volume slide, etc.
///
/// `vol_col` is the cell's volume-column interpretation for this row —
/// vol-col slides must run per-tick too (e.g. `+x` = slide up y each
/// non-zero tick). `table` is the active XM pitch table so the
/// glissando snap can map a period back to the nearest semitone in
/// either Linear or Amiga mode.
fn apply_tickn_effect(ch: &mut XmChannel, vol_col: XmVolume, table: XmPitchTable) {
    let ep = ch.effect_param;

    match ch.effect {
        0x01 => {
            // 1xy: Porta up — subtract (param * 4) from period per tick.
            let p = if ep != 0 { ep } else { ch.porta_updown_mem };
            ch.period = (ch.period - (p as f32) * 4.0).max(1.0);
        }
        0x02 => {
            // 2xy: Porta down.
            let p = if ep != 0 { ep } else { ch.porta_updown_mem };
            ch.period += (p as f32) * 4.0;
        }
        0x03 => {
            // 3xy: Tone porta — slide toward target.
            tone_porta_step(ch, table);
        }
        0x05 => {
            // 5xy: tone porta + volume slide.
            tone_porta_step(ch, table);
            apply_vol_slide(ch, ch.vol_slide_mem);
        }
        0x06 => {
            // 6xy: vibrato + volume slide. Vibrato motion is in
            // advance_tick; handle the volume slide part here.
            apply_vol_slide(ch, ch.vol_slide_mem);
        }
        0x0A => {
            // Axy: volume slide.
            apply_vol_slide(ch, ch.vol_slide_mem);
        }
        0x19 => {
            // Pxy: Panning slide. High nibble = right, low nibble = left.
            let mem = if ep != 0 { ep } else { ch.pan_slide_mem };
            let hi = mem >> 4;
            let lo = mem & 0x0F;
            if hi != 0 {
                ch.base_panning = ch.base_panning.saturating_add(hi);
            } else if lo != 0 {
                ch.base_panning = ch.base_panning.saturating_sub(lo);
            }
        }
        _ => {}
    }

    // Volume-column per-tick slides.
    match vol_col {
        XmVolume::VolumeSlideUp(p) => {
            let amt = if p != 0 { p } else { ch.vol_slide_col_mem };
            ch.volume = (ch.volume as u16 + amt as u16).min(64) as u8;
            ch.base_volume = ch.volume;
        }
        XmVolume::VolumeSlideDown(p) => {
            let amt = if p != 0 { p } else { ch.vol_slide_col_mem };
            ch.volume = ch.volume.saturating_sub(amt);
            ch.base_volume = ch.volume;
        }
        XmVolume::TonePorta(_) => {
            // Slide toward target by (porta_speed) period units per
            // tick. Same step + glissando snap as 3xy / 5xy.
            tone_porta_step(ch, table);
        }
        XmVolume::PanningSlideLeft(p) => {
            ch.base_panning = ch.base_panning.saturating_sub(p);
        }
        XmVolume::PanningSlideRight(p) => {
            ch.base_panning = ch.base_panning.saturating_add(p);
        }
        _ => {}
    }
}

/// Compute the initial XM "period" for a given real note + finetune
/// under the active frequency table.
///
/// In Linear mode this is the exact formula from the spec:
///   Period = 10*12*16*4 - Note*16*4 - FineTune/2
/// producing values around 4608 for C-4 (48) at finetune 0. One
/// semitone is 64 units; one "finetune step" is 1/2 a period unit.
///
/// In Amiga mode we look up the 96-entry period table with the same
/// indexing as `XmPitch::amiga_period` and keep it on the same `* 16`
/// scale so vibrato depth (in units of 2) maps roughly to one semitone
/// at depth=32, matching FT2.
fn note_to_period(table: XmPitchTable, real_note: i32, finetune: i32) -> f32 {
    match table {
        XmPitchTable::Linear => {
            10.0 * 12.0 * 16.0 * 4.0 - (real_note as f32) * 16.0 * 4.0 - (finetune as f32) / 2.0
        }
        XmPitchTable::Amiga => {
            let n_mod = real_note.rem_euclid(12) as usize;
            let n_div = real_note.div_euclid(12) as f32;
            let ft = finetune as f32 / 16.0;
            let ft_floor = ft.floor();
            let frac = ft - ft_floor;
            let base_idx = ((n_mod as isize) * 8 + ft_floor as isize).clamp(0, 95) as usize;
            let next_idx = (base_idx + 1).min(95);
            let p0 = XmPitch::PERIOD_TAB_PUB[base_idx] as f32;
            let p1 = XmPitch::PERIOD_TAB_PUB[next_idx] as f32;
            let p = p0 * (1.0 - frac) + p1 * frac;
            (p * 16.0) / 2.0f32.powf(n_div)
        }
    }
}

/// Convert the in-engine XM period to an output frequency in Hz.
fn period_to_freq(table: XmPitchTable, period: f32) -> f32 {
    match table {
        XmPitchTable::Linear => {
            // Freq = 8363 * 2^((6*12*16*4 - Period) / (12*16*4))
            8363.0 * 2.0f32.powf((6.0 * 12.0 * 16.0 * 4.0 - period) / (12.0 * 16.0 * 4.0))
        }
        XmPitchTable::Amiga => {
            if period <= 0.0 {
                0.0
            } else {
                8363.0 * 1712.0 / period
            }
        }
    }
}

/// Take one 3xy/5xy/vol-col-Mx tone-porta step and, when glissando is
/// on, snap the resulting period to the nearest semitone.
///
/// Spec source: `FastTracker-2-v2.04-xm.txt` line 222 ("E3  Set
/// glissando control"). The standard tone-porta slide is `period ±=
/// porta_speed * 4` per tick > 0, settling when within `speed` of the
/// target. Glissando layers a per-tick semitone quantisation on top so
/// audible pitch only changes in semitone steps even while the
/// underlying period interpolates continuously toward the target.
fn tone_porta_step(ch: &mut XmChannel, table: XmPitchTable) {
    let speed = (ch.porta_speed as f32) * 4.0;
    if (ch.period - ch.porta_target).abs() <= speed {
        ch.period = ch.porta_target;
    } else if ch.period < ch.porta_target {
        ch.period += speed;
    } else {
        ch.period -= speed;
    }
    if ch.glissando {
        ch.period = snap_to_semitone(ch.period, table);
    }
}

/// Quantise a period to the nearest semitone under the given pitch
/// table. In Linear mode this is a straight `round` to the 64-unit
/// semitone grid (anchored on the spec formula
/// `Period = 10*12*16*4 - Note*16*4`). In Amiga mode we find the
/// closest entry in the published 96-step finetune-0 column of
/// `XmPitch::PERIOD_TAB_PUB`, expanded across the 10 octaves the period
/// formula spans, and return that entry's period.
fn snap_to_semitone(period: f32, table: XmPitchTable) -> f32 {
    match table {
        XmPitchTable::Linear => {
            // Linear period: Period = 10*12*16*4 - Note*16*4 -
            // FineTune/2. Semitone grid step is 16*4 = 64 units.
            // Anchor: when finetune = 0, the period is an integer
            // multiple of 64 offset from the spec's zero. We snap by
            // rounding to that grid and clamping into the valid range.
            const GRID: f32 = 64.0;
            const ANCHOR: f32 = 10.0 * 12.0 * 16.0 * 4.0; // 7680
            let n = ((ANCHOR - period) / GRID).round();
            (ANCHOR - n * GRID).max(1.0)
        }
        XmPitchTable::Amiga => {
            // Amiga period table is 96 entries (one per finetune-0
            // semitone at the base octave) on the `* 16` scale.
            // `note_to_period` divides by 2^(n_div) to drop octaves, so
            // we walk the table at each octave shift and pick whichever
            // candidate minimises `|period - candidate|`.
            if period <= 1.0 {
                return period.max(1.0);
            }
            let mut best = period;
            let mut best_err = f32::INFINITY;
            for n_div in 0..10 {
                let div = 2.0f32.powi(n_div);
                for &p_raw in XmPitch::PERIOD_TAB_PUB.iter() {
                    let cand = (p_raw as f32 * 16.0) / div;
                    let err = (cand - period).abs();
                    if err < best_err {
                        best_err = err;
                        best = cand;
                    }
                }
            }
            best
        }
    }
}

/// Apply an Axy-style volume slide step using the `mem` byte (high nibble
/// = slide-up, low nibble = slide-down; the `0` nibble re-uses the last
/// non-zero value per FT2).
fn apply_vol_slide(ch: &mut XmChannel, mem: u8) {
    let hi = mem >> 4;
    let lo = mem & 0x0F;
    if hi != 0 {
        ch.volume = (ch.volume as u16 + hi as u16).min(64) as u8;
    } else if lo != 0 {
        ch.volume = ch.volume.saturating_sub(lo);
    }
    ch.base_volume = ch.volume;
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

    // ---------------- Glissando / tone-porta snap ----------------

    #[test]
    fn glissando_linear_snaps_to_64_unit_grid() {
        // Linear semitone grid is 64 units off the anchor `ANCHOR`.
        // 4608 is the C-4 period (real_note 48): ANCHOR - 48*64 = 4608.
        // Half-way between two semitones is +32; snap should pick the
        // nearer semitone.
        let a = snap_to_semitone(4608.0, XmPitchTable::Linear);
        assert!((a - 4608.0).abs() < 0.5);
        // 4608 + 31 → still C-4 (round down).
        let b = snap_to_semitone(4608.0 + 31.0, XmPitchTable::Linear);
        assert!((b - 4608.0).abs() < 0.5);
        // 4608 + 33 → next semitone down (period larger = lower pitch),
        // i.e. ANCHOR - 47*64 = 4672.
        let c = snap_to_semitone(4608.0 + 33.0, XmPitchTable::Linear);
        assert!((c - 4672.0).abs() < 0.5);
    }

    #[test]
    fn glissando_amiga_picks_nearest_table_entry() {
        // The published Amiga period table's finetune-0 column at
        // semitone 0 is XmPitch::PERIOD_TAB_PUB[0]; we recompute the
        // expected period for the base octave on the *16 scale.
        let p0 = XmPitch::PERIOD_TAB_PUB[0] as f32 * 16.0;
        // Off by +1 — snap should still pick p0.
        let snapped = snap_to_semitone(p0 + 1.0, XmPitchTable::Amiga);
        assert!((snapped - p0).abs() < 0.5);
        // Off by +(table_step / 4) — still snaps to p0 because the
        // next semitone in the table is ~5% away on the *16 scale.
        let p1 = XmPitch::PERIOD_TAB_PUB[8] as f32 * 16.0;
        let mid = (p0 + p1) / 2.0;
        let snapped = snap_to_semitone(mid - 1.0, XmPitchTable::Amiga);
        assert!((snapped - p0).abs() < (p1 - p0).abs(), "should pick p0");
        let snapped = snap_to_semitone(mid + 1.0, XmPitchTable::Amiga);
        assert!((snapped - p1).abs() < (p1 - p0).abs(), "should pick p1");
    }

    #[test]
    fn tone_porta_step_without_glissando_lands_on_target() {
        let mut ch = XmChannel {
            period: 4608.0,
            porta_target: 4672.0, // one semitone down (larger period)
            porta_speed: 0x10,    // step = 16 * 4 = 64 units / tick
            glissando: false,
            ..Default::default()
        };
        tone_porta_step(&mut ch, XmPitchTable::Linear);
        // One tick of speed=64 should hit the target exactly.
        assert!((ch.period - 4672.0).abs() < 0.5);
    }

    #[test]
    fn tone_porta_step_with_glissando_snaps_intermediate_steps() {
        let mut ch = XmChannel {
            period: 4608.0,
            porta_target: 4736.0, // two semitones down (larger period)
            porta_speed: 0x08,    // step = 8 * 4 = 32 (half-semitone)
            glissando: true,
            ..Default::default()
        };
        // First tick: +32 raw → 4640, snap to nearest grid → either
        // 4608 or 4672. The round-half-to-even rule on `.round()` picks
        // 4640 → 4640/64 = 72.5 from the anchor's neg distance, so the
        // round goes to 73 → ANCHOR - 73*64 = 4672 (C-4 + 1 semi down).
        tone_porta_step(&mut ch, XmPitchTable::Linear);
        assert!(
            (ch.period - 4672.0).abs() < 0.5 || (ch.period - 4608.0).abs() < 0.5,
            "expected snap to neighbour semitone, got {}",
            ch.period
        );
        // Without glissando, after one tick we'd be at 4640.0 — i.e.
        // the period is *quantised*, not still mid-step.
        assert!(
            (ch.period - 4640.0).abs() > 1.0,
            "glissando snap should not leave the period at the raw \
             intermediate value 4640.0 (got {})",
            ch.period
        );
    }

    // ---------------- Lxy / set envelope position ----------------

    #[test]
    fn lxy_sets_volume_envelope_tick_and_zeroes_segment() {
        let mut ch = XmChannel {
            vol_env_tick: 3,
            vol_env_seg: 1,
            pan_env_tick: 7,
            pan_env_seg: 2,
            effect: 0x15,
            effect_param: 0x20,
            ..Default::default()
        };
        apply_tick0_effect(&mut ch);
        // Volume envelope cursor moved to the requested tick.
        assert_eq!(ch.vol_env_tick, 0x20);
        // Segment zeroed so `tick_envelope` re-aligns on the next call.
        assert_eq!(ch.vol_env_seg, 0);
        // Pan envelope untouched (Lxy targets the volume envelope per
        // the FT2 reading; pan envelope only moves under explicit
        // panning effects).
        assert_eq!(ch.pan_env_tick, 7);
        assert_eq!(ch.pan_env_seg, 2);
    }

    #[test]
    fn lxy_param_zero_rewinds_to_envelope_start() {
        let mut ch = XmChannel {
            vol_env_tick: 100,
            vol_env_seg: 3,
            effect: 0x15,
            effect_param: 0x00,
            ..Default::default()
        };
        apply_tick0_effect(&mut ch);
        assert_eq!(ch.vol_env_tick, 0);
        assert_eq!(ch.vol_env_seg, 0);
    }

    // ---------------- E3x captured into ch.glissando ----------------

    #[test]
    fn e3x_nonzero_enables_glissando() {
        let mut ch = XmChannel {
            effect: 0x0E,
            effect_param: 0x31,
            ..Default::default()
        };
        apply_tick0_effect(&mut ch);
        assert!(ch.glissando);
    }

    #[test]
    fn e3x_zero_disables_glissando() {
        let mut ch = XmChannel {
            glissando: true,
            effect: 0x0E,
            effect_param: 0x30,
            ..Default::default()
        };
        apply_tick0_effect(&mut ch);
        assert!(!ch.glissando);
    }

    // ---------- E4x / E7x vibrato + tremolo waveform shapes ----------

    #[test]
    fn waveform_sine_matches_table() {
        // Shape 0 (and the no-retrigger variant 4) must reproduce the
        // sine table exactly across the whole 64-step cycle.
        for pos in 0u8..64 {
            assert_eq!(waveform_lfo(0, pos), SINE_TABLE[pos as usize] as i32);
            assert_eq!(waveform_lfo(4, pos), SINE_TABLE[pos as usize] as i32);
        }
    }

    #[test]
    fn waveform_random_falls_back_to_sine() {
        // No PRNG is documented under docs/, so shape 3 is the
        // deterministic sine fallback (same choice as the MOD player).
        for pos in [0u8, 7, 16, 32, 48, 63] {
            assert_eq!(waveform_lfo(3, pos), SINE_TABLE[pos as usize] as i32);
        }
    }

    #[test]
    fn waveform_downward_saw_descends_through_cycle() {
        // "Waveform 1 is a downwards saw wave." Starts positive at the
        // top of the cycle and falls monotonically to negative.
        assert_eq!(waveform_lfo(1, 0), 127);
        let mut prev = waveform_lfo(1, 0);
        for pos in 1u8..64 {
            let v = waveform_lfo(1, pos);
            assert!(v < prev, "saw must descend at pos {pos}: {v} !< {prev}");
            prev = v;
        }
        // Crosses zero around mid-cycle and reaches the negative extreme.
        assert!(waveform_lfo(1, 31) > 0);
        assert!(waveform_lfo(1, 32) < 0);
        assert_eq!(waveform_lfo(1, 63), 127 - 63 * 4);
    }

    #[test]
    fn waveform_square_is_plus_then_minus() {
        // "Waveform 2 is a square wave, starting from +y." Positive over
        // the first half of the cycle, negative over the second.
        for pos in 0u8..32 {
            assert_eq!(waveform_lfo(2, pos), 127, "first half should be +127");
        }
        for pos in 32u8..64 {
            assert_eq!(waveform_lfo(2, pos), -127, "second half should be -127");
        }
    }

    #[test]
    fn e4x_sets_vibrato_waveform_shape_bits() {
        // E41 selects the downward-saw vibrato shape; E44 selects sine
        // with the don't-retrigger bit. The low three bits are stored.
        let mut ch = XmChannel {
            effect: 0x0E,
            effect_param: 0x41,
            ..Default::default()
        };
        apply_tick0_effect(&mut ch);
        assert_eq!(ch.vib_waveform & 0x03, 1, "shape bits = ramp-down");
        assert_eq!(ch.vib_waveform & 0x04, 0, "retrigger still enabled");

        let mut ch = XmChannel {
            effect: 0x0E,
            effect_param: 0x44,
            ..Default::default()
        };
        apply_tick0_effect(&mut ch);
        assert_eq!(ch.vib_waveform & 0x03, 0, "shape bits = sine");
        assert_eq!(ch.vib_waveform & 0x04, 0x04, "don't-retrigger set");
    }

    #[test]
    fn e7x_sets_tremolo_waveform_shape_bits() {
        // E72 selects the square tremolo shape.
        let mut ch = XmChannel {
            effect: 0x0E,
            effect_param: 0x72,
            ..Default::default()
        };
        apply_tick0_effect(&mut ch);
        assert_eq!(ch.trem_waveform & 0x03, 2, "shape bits = square");
    }
}
