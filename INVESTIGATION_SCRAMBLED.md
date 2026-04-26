# Investigation: "Scrambled audio at 4.5s" (round 18)

## User report

> "tested and confirmed the mod issue is still there. at 4.5s the audio gets
> scrambled. it's not one speaker only, but both."

## What "scrambled" means in concrete spectral terms

Per-200ms `sox stats` and FFT comparison against an `openmpt123 --render` of
`rhmst.mod` shows two distinct phenomena:

1. **Global high-frequency over-attenuation (FIXED in this commit)** — the
   prior LED filter constants (`FIXED_RC_CUTOFF_HZ = 5000`,
   `LED_FILTER_CUTOFF_HZ = 3300`) modeled the real Amiga schematic but
   produced a noticeably "muffled" render compared to openmpt's defaults.
   Cross-correlation against `openmpt123 --render` measured 0.94–0.97
   per 1 s window across the entire song. After moving to the 16000 Hz /
   11500 Hz "modern player" cutoffs documented in
   `multimedia-cx-protracker.html` the same windows measure 0.97–0.99.
   Combined with `DEFAULT_PAN_SEPARATION 0.7 → 0.5` (also better-aligned
   to openmpt's default), `halluc.mod`'s first 11 s now sit at 0.99–0.997
   xcorr with the reference.

2. **Localized ch2/ch3 content divergence on `rhmst.mod` rows 28-33 of
   pattern 1 (3.4–4.0 s) — UNFIXED.** Even with all filtering, ramping,
   and pan changes disabled, the per-200ms cross-correlation against
   the openmpt reference collapses from ~0.95 to ~0.50–0.70 over this
   exact window, then recovers cleanly. The mid-range FFT shows
   excess energy in 200–500 Hz bands (where the music is) and a
   deficit at 400–510 Hz, with a sample-by-sample diff RMS that
   builds gradually from ~1500 LSB at 2.6 s to ~9000 LSB at 3.5 s
   then drops sharply back to <500 LSB at 4.05 s. Lag analysis shows
   wild swings (-30 to +22 samples in 50 ms windows) inside this
   region, vs a stable -2 sample baseline elsewhere — this is
   *content* divergence, not a clock drift. Channel-ablation
   measurement (zero one channel at a time, recompute xcorr vs ref)
   pinpoints the divergence to ch2 + ch3, both playing sample 11
   ("79144 falun", 28160 bytes, no loop, drum/percussion content)
   somewhere between sample positions 13000 and 25000.

## Where I bracketed the bug

**File**: `rhmst.mod` (`samples.oxideav.org/magicaltux/mod/rhmst.mod`,
sha256 `31f2f8fe…`).

**When**: order 0, pattern 1, rows 28–33 (3.4–4.0 s wall clock at
default speed=6 / bpm=125).

**What's playing**: ch0/ch1 alternate sample 1 retriggers (period 285),
ch2/ch3 hold sample 11 (period 285) without retrigger.

**Which channels are wrong**: ch2 and ch3 (the ones playing sample 11).
Per-channel renders read the right PCM values from sample 11 with linear
interpolation, but the resulting waveforms diverge from openmpt by
±2000-9000 LSB (bipolar, oscillating). The same divergence reappears
verbatim at 11–12 s (rows 26–31 of pattern 2, identical content).

## What I ruled out

- LED filter — bypass tested, divergence persists at 0.69 xcorr.
- Per-trigger ramp — bypass tested (`RAMP_FRAMES = 0`), no change.
- Linear-vs-nearest interpolation — both worse overall, neither moves
  the 3-4 s window.
- Pan separation — tested 0.0 / 0.5 / 0.7 / 1.0, all see the same dip.
- Sample 11 PCM extraction — verified byte-exact against direct file
  read; first/last 4 bytes are the canonical PT zero-terminator pair.
- Tempo / row timing — `samples_per_tick(125 bpm) = 882` exactly, no
  rounding. Row period stable at 5292 frames throughout the test
  region; openmpt's render aligns to within 1-2 samples elsewhere.

## What I have NOT yet ruled out

- Sub-Paula DMA latency: a real Amiga only fetches a new sample word
  every `period * 2` clock cycles, and the player driving the DMA
  registers can be late by up to one full DMA word. Modern PT
  rendering chains may model this latency; ours does not. This
  would manifest as a small phase noise on the long sample 11
  playback that *only* shows up when the sample data has high
  intra-period variance (which is exactly what 13000–25000 of
  sample 11 contains — high-entropy percussion).

- 8-tap windowed-sinc interpolation antialiasing artefacts —
  openmpt's `--filter 1` (nearest, no anti-aliasing) shows the same
  divergence as `--filter 8` (default sinc), which weakly argues
  against this — but openmpt's own resampler may apply a low-pass
  even in `--filter 1` mode that we don't.

- Sample-end behaviour at the looped-region boundary on a different
  channel: ch2 plays sample 11 to its full 28160-byte end at
  ~4.18 s, just *after* the 3.4–4.0 s divergence window. The
  divergence vanishes precisely where ch2 falls silent — perhaps
  openmpt fades sample 11 out earlier than we do.

## Concrete next-round attack plan

1. Render only ch2 from openmpt by **muting all other channels**
   (`openmpt123 --filter 8 --render` does not have a `--mute`
   flag, but libopenmpt's API supports `set_channel_mute_status`
   — wrap a tiny C program that loads the file, mutes ch0/ch1/ch3,
   and writes the WAV; that is documentation-spec usage of the
   public API, not source-level peeking). Compare ch2-only ours
   to ch2-only openmpt sample-for-sample. The exact frame at
   which they diverge will pinpoint whether it's a sample-end
   handling, a loop wrap, or a sample-data-reading bug.

2. Re-read `Protracker-effects-MODFIL12.txt` §2.6 ("Repeat
   point/length") and `Pro-Noise-Soundtracker-rev4.txt` §[5]
   ("Sample data") looking for the specific PT quirk about how
   `loop_length=1` (no-loop) samples behave when their cursor
   reaches `length` — does PT continue playing past `length`
   into the **next sample's data buffer** because the DMA
   register isn't reset until the playroutine pokes a new
   AUDxLEN? That would produce exactly the kind of "scrambled"
   noise described, and would only manifest when there's
   another sample physically following in the file (sample 12
   in this case is "sweden....  bye...").

3. Check if openmpt is doing "DC blocking" that we aren't —
   the energy-buildup pattern (rms_diff ramps from 1500 →
   9000 over 0.6s then drops to 500) looks suspiciously like
   a slow filter capacitor effect.

4. Time-align ch2 ours vs ch2 openmpt with a per-100 ms lag
   search (already implemented in the diagnostic harness in
   the `/tmp/finer_diag.rs` test file used during this
   investigation — re-create as a Python helper). The wild
   ±30-sample swings in lag suggest a **periodic glitch**
   that resets — find the period, identify the trigger.
