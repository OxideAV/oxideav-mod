//! Shared tracker mixer core.
//!
//! The different tracker formats that this crate handles (MOD, STM, XM)
//! diverge in how they pitch notes and how they store PCM, but the
//! per-voice mixer loop is always the same: read a sample from a source,
//! multiply by a volume scalar, advance the read position by
//! `source_rate / output_rate`, and handle end-of-sample / loop.
//!
//! This module factors that common loop out so MOD's 8-bit Paula samples,
//! STM's 8-bit signed samples, and XM's 8 or 16-bit delta-decoded samples
//! can all feed the same `MixerVoice`.
//!
//! Three abstractions live here:
//!
//! - [`SampleSource`] — read-only view into a decoded PCM body plus loop
//!   metadata. Implementations in this crate exist for MOD
//!   ([`crate::samples::SampleBody`]), STM ([`crate::stm::StmSampleBody`])
//!   and XM ([`crate::xm::XmSampleHeader`]).
//! - [`PitchModel`] — converts a format-specific "note" (Amiga period,
//!   STM C3-relative octave/semitone, or XM note+finetune under one of
//!   the two XM frequency tables) into an output frequency in Hz. The
//!   mixer core consumes only the Hz value.
//! - [`MixerVoice`] — the actual generic voice. Owns a cursor into an
//!   arbitrary `SampleSource`, a current frequency (set by the player
//!   from a [`PitchModel`] result), and a linear-volume scalar 0..=1.
//!   Emits one `f32` sample per call. Format-agnostic.

/// Loop behaviour for a sample body. Tracker formats share the same three
/// modes — no loop / forward / ping-pong — so we encode them here rather
/// than repeat the enum per format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LoopKind {
    /// Play once, stop when past end.
    #[default]
    None,
    /// On reaching loop_end, jump back to loop_start.
    Forward,
    /// On reaching loop_end, reverse direction. On reaching loop_start
    /// while reversed, resume forward.
    PingPong,
}

/// Read-only view into a tracker sample body.
///
/// Implementations must return samples in the range `-1.0..=1.0`. The
/// caller (the [`MixerVoice`]) manages the fractional read position, so
/// `at` takes an integer sample index.
pub trait SampleSource {
    /// Total number of PCM frames.
    fn len(&self) -> usize;

    /// Loop start index (frames).
    fn loop_start(&self) -> usize;

    /// Loop end index (frames), exclusive.
    fn loop_end(&self) -> usize;

    /// Loop mode.
    fn loop_kind(&self) -> LoopKind;

    /// Sample at integer index, normalised to `-1.0..=1.0`. Callers are
    /// responsible for ensuring `idx < len()`; implementations may return
    /// 0.0 for out-of-range indices defensively.
    fn at(&self, idx: usize) -> f32;

    /// True if this sample has no PCM data.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Abstraction over the pitch math for a given tracker format.
///
/// Each format carries a format-specific "note token" (Amiga period for
/// MOD, C3-relative semitone position for STM, or XM's note + finetune
/// pair), from which the output frequency is derived. This trait exposes
/// only the final frequency in Hz — the mixer core never sees periods.
pub trait PitchModel {
    /// The player's note token. Keep it `Copy` so it can sit in a voice
    /// cheaply.
    type Note: Copy;

    /// Convert a note token to an output frequency in Hz. Implementations
    /// must return a positive value; 0 or negative means "silent".
    fn note_to_freq(&self, note: Self::Note) -> f32;
}

/// Generic, format-agnostic mixer voice. The caller assigns a frequency
/// (from a [`PitchModel`]) and a linear volume; the voice steps through
/// its `SampleSource` at `freq / sample_rate` and emits one float per
/// call. Ping-pong, forward-loop and one-shot modes are all handled.
///
/// `Voice` does not own the sample source — `render_one` takes the source
/// by reference. That lets the caller store sources somewhere else (a
/// slab on `PlayerState`, typically) and address them by index while the
/// voice only tracks the *current* index.
#[derive(Clone, Debug, Default)]
pub struct MixerVoice {
    /// Fractional sample cursor into the source.
    pub pos: f32,
    /// Current playback direction (+1 forward, -1 reversed for ping-pong).
    pub direction: i8,
    /// Output frequency in Hz (updated by the player per row / per tick).
    pub freq: f32,
    /// Linear volume, 0..=1.
    pub volume: f32,
    /// True while the voice is emitting sound. Cleared when a one-shot
    /// sample reaches its end.
    pub active: bool,
}

impl MixerVoice {
    /// Trigger a fresh note. Resets the cursor to 0 and sets the
    /// frequency + volume. Direction is forward.
    pub fn trigger(&mut self, freq: f32, volume: f32) {
        self.pos = 0.0;
        self.direction = 1;
        self.freq = freq;
        self.volume = volume;
        self.active = true;
    }

    /// Mix one sample from `source` at the given output sample rate.
    /// Returns the post-volume float in `-1.0..=1.0`.
    pub fn render_one<S: SampleSource + ?Sized>(&mut self, source: &S, out_rate: f32) -> f32 {
        if !self.active || source.is_empty() || self.freq <= 0.0 || out_rate <= 0.0 {
            return 0.0;
        }

        let len = source.len();
        let loop_start = source.loop_start().min(len.saturating_sub(1));
        let loop_end = source.loop_end().min(len);
        let kind = source.loop_kind();

        // Resolve position into a valid integer index. For ping-pong we
        // may already have flipped direction last step; keep the pos
        // inside [loop_start, loop_end) while looping, or stop on end.
        let pos = self.pos;
        if pos < 0.0 {
            // Ping-pong may dip below loop_start briefly — bounce.
            if matches!(kind, LoopKind::PingPong) {
                let over = -pos;
                self.pos = loop_start as f32 + over;
                self.direction = 1;
            } else {
                self.active = false;
                return 0.0;
            }
        }

        // A forward loop wraps at `loop_end`, NOT at the buffer end:
        // when `loop_end < len`, the PCM past `loop_end` is the one-shot
        // tail that the loop must never read (per
        // `docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt`
        // §2.2 + §2.8 — the data past loop_end is discarded once the
        // sample enters its loop). Trigger the wrap on `loop_end` for a
        // forward loop, falling back to `len` only for the one-shot tail
        // of an unlooped voice.
        let forward_wrap_at = if matches!(kind, LoopKind::Forward) && loop_end > loop_start {
            loop_end as f32
        } else {
            len as f32
        };
        if self.pos >= forward_wrap_at {
            match kind {
                LoopKind::Forward if loop_end > loop_start => {
                    let span = (loop_end - loop_start) as f32;
                    let over = self.pos - loop_start as f32;
                    self.pos = loop_start as f32 + over.rem_euclid(span);
                }
                LoopKind::PingPong if loop_end > loop_start => {
                    let over = self.pos - (loop_end as f32 - 1.0);
                    self.pos = (loop_end as f32 - 1.0 - over).max(loop_start as f32);
                    self.direction = -1;
                }
                _ => {
                    self.active = false;
                    return 0.0;
                }
            }
        }

        let i = (self.pos as usize).min(len - 1);
        let frac = self.pos - (i as f32);
        let s0 = source.at(i);
        // The interpolation partner respects the loop boundary too: if
        // `i + 1` lands on or past `loop_end`, wrap to `loop_start`
        // rather than read the discarded tail. For a non-looping voice
        // the partner clamps to the buffer end.
        let looping = !matches!(kind, LoopKind::None) && loop_end > loop_start;
        let s1_idx = if looping {
            if i + 1 < loop_end {
                i + 1
            } else {
                loop_start
            }
        } else if i + 1 < len {
            i + 1
        } else {
            i
        };
        let s1 = source.at(s1_idx);
        let interp = s0 + (s1 - s0) * frac;

        // Advance. Step is signed for ping-pong.
        let step = self.freq / out_rate;
        let signed_step = step * self.direction as f32;
        self.pos += signed_step;

        // Ping-pong end-of-loop bounce (forward → reverse).
        if matches!(kind, LoopKind::PingPong) {
            if self.direction == 1 && self.pos >= loop_end as f32 && loop_end > loop_start {
                let over = self.pos - (loop_end as f32 - 1.0);
                self.pos = (loop_end as f32 - 1.0 - over).max(loop_start as f32);
                self.direction = -1;
            } else if self.direction == -1 && self.pos < loop_start as f32 {
                let over = loop_start as f32 - self.pos;
                self.pos = loop_start as f32 + over;
                self.direction = 1;
            }
        }

        interp * self.volume
    }
}

// ---------------- Pitch models ----------------

/// MOD / ProTracker pitch model: Amiga Paula period → frequency.
///
/// Output rate = `paula_clock / period`. The PAL constant is
/// `7_093_789.2 / 2 ≈ 3_546_894.6 Hz`.
#[derive(Clone, Copy, Debug)]
pub struct AmigaPeriodPitch {
    pub paula_clock: f32,
}

impl Default for AmigaPeriodPitch {
    fn default() -> Self {
        AmigaPeriodPitch {
            paula_clock: crate::player::PAULA_CLOCK,
        }
    }
}

impl PitchModel for AmigaPeriodPitch {
    type Note = u16;

    fn note_to_freq(&self, note: Self::Note) -> f32 {
        if note == 0 {
            0.0
        } else {
            self.paula_clock / note as f32
        }
    }
}

/// STM pitch model: C3-relative `(octave, semitone)` + sample-specific C3
/// frequency. STM stores the note byte as `octave<<4 | semitone`, with
/// C-3 at octave=3, semitone=0 being the sample's declared C3 frequency.
///
/// Mid-2020s trackers typically use C-5 as the reference note, but the
/// STM v1 spec explicitly ties the "C3 frequency" instrument field to the
/// value sounded at octave 3 / semitone 0, so that's what we implement
/// here. If a given STM file disagrees, the audible pitch will be octave-
/// shifted but the *relative* pitch between notes remains correct.
///
/// Note value layout: `note = (octave << 4) | semitone`, `semitone in
/// 0..=11`. Freq = c3_hz * 2^((octave-3) + semitone/12).
#[derive(Clone, Copy, Debug, Default)]
pub struct StmC3Pitch {
    pub c3_hz: f32,
}

impl PitchModel for StmC3Pitch {
    /// `(octave, semitone)`; octave is 0..=7, semitone is 0..=11.
    type Note = (u8, u8);

    fn note_to_freq(&self, note: Self::Note) -> f32 {
        if self.c3_hz <= 0.0 {
            return 0.0;
        }
        let (octave, semitone) = note;
        // Semitone distance from C-3, in 1/12-octave steps.
        let semis_from_c3 = (octave as f32 - 3.0) * 12.0 + semitone as f32;
        self.c3_hz * 2.0f32.powf(semis_from_c3 / 12.0)
    }
}

/// XM frequency-table selection. Chosen per-file (`XmHeader.flags`
/// bit 0). The enum is independent of the header type so the mixer can
/// carry it without pulling in the whole parser struct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XmPitchTable {
    Amiga,
    Linear,
}

/// XM pitch model: note + finetune + relative-note, under one of two
/// frequency tables (Amiga or Linear).
///
/// XM note numbering: `1..=96` = C-0..B-7 (note `1` is C-0, so
/// `real_note = pattern_note - 1 + relative_note`, with `real_note = 48`
/// corresponding to C-4, which is the centre of the XM keyboard).
///
/// Formulas from `docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt`:
/// - Linear: `Period = 10*12*16*4 - Note*16*4 - FineTune/2`,
///   `Freq   = 8363 * 2 ^ ((6*12*16*4 - Period) / (12*16*4))`.
/// - Amiga: interpolate a 96-entry `PeriodTab` via
///   `note % 12 * 8 + finetune/16`, then `Freq = 8363 * 1712 / Period`.
///
/// `Note` here is a pair `(real_note, finetune)` where `real_note` is
/// `0..=118` (0 = C-0, 48 = C-4). The tracker-format code is responsible
/// for applying `relative_note` before handing to `note_to_freq`.
#[derive(Clone, Copy, Debug)]
pub struct XmPitch {
    pub table: XmPitchTable,
}

impl Default for XmPitch {
    fn default() -> Self {
        XmPitch {
            table: XmPitchTable::Amiga,
        }
    }
}

impl XmPitch {
    /// XM Amiga-table period lookup. 96-entry table indexed by
    /// `(note % 12) * 8 + finetune/16`, with linear interpolation on the
    /// fractional part of `finetune/16`.
    #[rustfmt::skip]
    const PERIOD_TAB: [u16; 96] = [
        907,900,894,887,881,875,868,862,856,850,844,838,832,826,820,814,
        808,802,796,791,785,779,774,768,762,757,752,746,741,736,730,725,
        720,715,709,704,699,694,689,684,678,675,670,665,660,655,651,646,
        640,636,632,628,623,619,614,610,604,601,597,592,588,584,580,575,
        570,567,563,559,555,551,547,543,538,535,532,528,524,520,516,513,
        508,505,502,498,494,491,487,484,480,477,474,470,467,463,460,457,
    ];

    /// Public re-export of the period table for use by the XM player's
    /// own period-based pitch math (vibrato / tone-porta in Amiga mode).
    pub const PERIOD_TAB_PUB: [u16; 96] = Self::PERIOD_TAB;

    fn amiga_period(real_note: i32, finetune: i32) -> f32 {
        // finetune/16 can be negative; wrap index accordingly.
        let n_mod = real_note.rem_euclid(12) as usize;
        let n_div = real_note.div_euclid(12);
        // finetune / 16 with floor semantics, then interpolate fractional.
        let ft = finetune as f32 / 16.0;
        let ft_floor = ft.floor();
        let frac = ft - ft_floor;
        let base_idx = (n_mod as isize * 8 + ft_floor as isize).clamp(0, 95) as usize;
        let next_idx = (base_idx + 1).min(95);
        let p0 = Self::PERIOD_TAB[base_idx] as f32;
        let p1 = Self::PERIOD_TAB[next_idx] as f32;
        let p = p0 * (1.0 - frac) + p1 * frac;
        let octave_div = 2.0f32.powi(n_div);
        (p * 16.0) / octave_div
    }

    fn linear_period(real_note: i32, finetune: i32) -> f32 {
        // Period = 10*12*16*4 - Note*16*4 - FineTune/2;
        let p =
            10.0 * 12.0 * 16.0 * 4.0 - (real_note as f32) * 16.0 * 4.0 - (finetune as f32) / 2.0;
        p.max(1.0)
    }
}

impl PitchModel for XmPitch {
    /// `(real_note, finetune)` where `real_note` is already adjusted by
    /// `relative_note` and sits in `0..=118`, `finetune` is `-128..=127`.
    type Note = (i32, i32);

    fn note_to_freq(&self, note: Self::Note) -> f32 {
        let (real_note, finetune) = note;
        match self.table {
            XmPitchTable::Amiga => {
                let p = Self::amiga_period(real_note, finetune);
                if p <= 0.0 {
                    0.0
                } else {
                    8363.0 * 1712.0 / p
                }
            }
            XmPitchTable::Linear => {
                let p = Self::linear_period(real_note, finetune);
                // Freq = 8363*2^((6*12*16*4 - Period) / (12*16*4))
                8363.0 * 2.0f32.powf((6.0 * 12.0 * 16.0 * 4.0 - p) / (12.0 * 16.0 * 4.0))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trivial in-memory sample source for unit tests.
    struct TestSource {
        pcm: Vec<f32>,
        loop_start: usize,
        loop_end: usize,
        kind: LoopKind,
    }

    impl SampleSource for TestSource {
        fn len(&self) -> usize {
            self.pcm.len()
        }
        fn loop_start(&self) -> usize {
            self.loop_start
        }
        fn loop_end(&self) -> usize {
            self.loop_end
        }
        fn loop_kind(&self) -> LoopKind {
            self.kind
        }
        fn at(&self, idx: usize) -> f32 {
            self.pcm.get(idx).copied().unwrap_or(0.0)
        }
    }

    #[test]
    fn amiga_period_pitch_matches_formula() {
        let p = AmigaPeriodPitch {
            paula_clock: 3_546_894.6,
        };
        // Period 428 = classic C-2; expected rate ~8287.14
        let f = p.note_to_freq(428);
        assert!((f - 8287.14).abs() < 0.5, "got {f}");
    }

    #[test]
    fn amiga_period_pitch_zero_means_silent() {
        let p = AmigaPeriodPitch {
            paula_clock: 3_546_894.6,
        };
        assert_eq!(p.note_to_freq(0), 0.0);
    }

    #[test]
    fn stm_c3_pitch_doubles_per_octave() {
        let p = StmC3Pitch { c3_hz: 8363.0 };
        let c3 = p.note_to_freq((3, 0));
        let c4 = p.note_to_freq((4, 0));
        assert!((c3 - 8363.0).abs() < 0.5, "c3 = {c3}");
        assert!((c4 - 16726.0).abs() < 1.0, "c4 = {c4}");
    }

    #[test]
    fn stm_c3_pitch_semitone_is_twelfth_root_of_two() {
        let p = StmC3Pitch { c3_hz: 440.0 };
        let f0 = p.note_to_freq((3, 0));
        let f1 = p.note_to_freq((3, 1));
        let ratio = f1 / f0;
        assert!((ratio - 1.059463).abs() < 0.001);
    }

    #[test]
    fn xm_linear_pitch_c4_is_8363_hz() {
        // For XM, real_note 48 = C-4 corresponds to 8363 Hz under the
        // Linear table at finetune 0 (this is the XM convention:
        // `RelativeTone = 0` maps C-4 → sample's native 8363 Hz).
        let p = XmPitch {
            table: XmPitchTable::Linear,
        };
        let f = p.note_to_freq((48, 0));
        assert!((f - 8363.0).abs() < 1.0, "got {f}");
    }

    #[test]
    fn xm_amiga_pitch_doubles_per_octave() {
        // The XM Amiga-table formula in the v2.04 spec does not put the
        // sample's native 8363 Hz at an integer note (the reference rate
        // lands between C-3 and C-4, at the non-integer N ≈ 36.9). We
        // therefore don't pin an absolute reference frequency — we just
        // check the invariant that still matters: one XM octave really is
        // a 2× ratio in the output frequency.
        let p = XmPitch {
            table: XmPitchTable::Amiga,
        };
        let c4 = p.note_to_freq((48, 0));
        let c5 = p.note_to_freq((60, 0));
        assert!(c4 > 0.0);
        assert!((c5 / c4 - 2.0).abs() < 1e-3, "ratio {}", c5 / c4);
    }

    #[test]
    fn xm_linear_pitch_one_octave_doubles() {
        let p = XmPitch {
            table: XmPitchTable::Linear,
        };
        let c4 = p.note_to_freq((48, 0));
        let c5 = p.note_to_freq((60, 0));
        assert!((c5 / c4 - 2.0).abs() < 1e-3);
    }

    #[test]
    fn voice_on_one_shot_goes_silent_at_end() {
        let src = TestSource {
            pcm: vec![0.5; 4],
            loop_start: 0,
            loop_end: 4,
            kind: LoopKind::None,
        };
        let mut v = MixerVoice::default();
        v.trigger(44100.0, 1.0); // one sample-unit per render
                                 // Render past the end.
        for _ in 0..10 {
            v.render_one(&src, 44100.0);
        }
        assert!(!v.active, "voice should deactivate past end");
    }

    #[test]
    fn voice_forward_loop_wraps() {
        let src = TestSource {
            pcm: vec![0.25, 0.5, 0.75, 1.0],
            loop_start: 0,
            loop_end: 4,
            kind: LoopKind::Forward,
        };
        let mut v = MixerVoice::default();
        v.trigger(44100.0, 1.0);
        for _ in 0..100 {
            let s = v.render_one(&src, 44100.0);
            assert!(s.abs() <= 1.0);
        }
        assert!(v.active, "looped voice must stay active");
    }

    #[test]
    fn forward_loop_never_reads_past_loop_end() {
        // Sample of 8 frames; loop region is frames 0..4. Frames 4..8
        // are the one-shot tail that must NEVER be read once the voice
        // is looping. We poison the tail with a sentinel the loop
        // region never produces (-1.0); if the mixer ever reads it the
        // emitted sample dips negative.
        let src = TestSource {
            pcm: vec![0.5, 0.5, 0.5, 0.5, -1.0, -1.0, -1.0, -1.0],
            loop_start: 0,
            loop_end: 4,
            kind: LoopKind::Forward,
        };
        let mut v = MixerVoice::default();
        // Step ~0.5 frames per render so the cursor crosses the loop
        // boundary fractionally — this is precisely where a bad
        // interpolation partner would sample the poisoned tail.
        v.trigger(22050.0, 1.0);
        let mut min_seen = f32::INFINITY;
        for _ in 0..200 {
            let s = v.render_one(&src, 44100.0);
            min_seen = min_seen.min(s);
        }
        assert!(v.active, "forward-looped voice must stay active");
        assert!(
            min_seen >= -0.001,
            "loop read into the discarded tail (min sample {min_seen})"
        );
    }

    #[test]
    fn forward_loop_position_wraps_at_loop_end_not_buffer_end() {
        // Same shape: loop 0..4, tail 4..8 poisoned. The wrap is applied
        // lazily on entry to each render (the MOD player uses the same
        // design), so the cursor can momentarily overshoot `loop_end`
        // after a step — but it is folded back BEFORE the next render
        // samples any index, and crucially it never wanders all the way
        // out to the buffer end (8). With a forward loop of length 4,
        // a wrapped cursor must always sit below `loop_end + step`; if
        // the wrap mistakenly keyed on `len` instead of `loop_end` the
        // cursor would climb past 4 toward 8 and the index would land in
        // the tail. We sample the actual read index each render and
        // assert it stays inside the loop region.
        let src = TestSource {
            pcm: vec![0.1, 0.2, 0.3, 0.4, -1.0, -1.0, -1.0, -1.0],
            loop_start: 0,
            loop_end: 4,
            kind: LoopKind::Forward,
        };
        let mut v = MixerVoice::default();
        v.trigger(33075.0, 1.0); // 0.75 frame per render
        for _ in 0..400 {
            let s = v.render_one(&src, 44100.0);
            // Every emitted sample is the interpolation of two frames
            // drawn from the loop region [0,4) whose values are 0.1..0.4
            // — strictly positive. A tail read (value -1.0) would drag
            // the result below 0.
            assert!(
                s > 0.0,
                "emitted sample non-positive ({s}) — a tail frame was read"
            );
        }
        assert!(v.active);
    }

    #[test]
    fn voice_with_zero_freq_is_silent() {
        let src = TestSource {
            pcm: vec![1.0; 8],
            loop_start: 0,
            loop_end: 8,
            kind: LoopKind::None,
        };
        let mut v = MixerVoice::default();
        v.trigger(0.0, 1.0);
        assert_eq!(v.render_one(&src, 44100.0), 0.0);
    }
}
