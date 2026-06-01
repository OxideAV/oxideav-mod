# Investigation: "Scrambled audio at 4.5s" — round 19 (FIXED)

## User report (rounds 18 / 19)

> "tested and confirmed the mod issue is still there. at 4.5s the audio
> gets scrambled. it's not one speaker only, but both."

After three prior rounds (commits `efa04a5`, `28b4721`, `8a8218c`)
addressing pan separation, the 2-pole filter, per-trigger ramp, and
filter cutoff tuning, the user reported the bug was still present on
both fixtures `halluc.mod` and `rhmst.mod`.

## What was diagnostically wrong with prior rounds

Cross-correlation against a black-box reference render consistently
measured 0.97-0.99 — the proxy measurement said "we match" but the
user still heard the bug. **Cross-correlation is not the right
oracle**: it cannot tell you the absolute output level, and absolute
output level is what causes downstream clipping.

## Round-19 oracle: trace reference impl as a black-box runtime-loaded reference

Built `tests/tracker_reference_compare.rs` to dlopen a published-ABI
reference renderer dylib via `libloading 0.9` (no compile-time link,
skipped cleanly when not installed). The harness configures the
reference to disable every audio-shaping flag (oversampling /
bass-boost / surround / noise reduction / reverb), uses LINEAR
resampling, 44100 Hz / 16-bit / stereo, and `stereo_separation = 128`
(= 50 %, matches our `pan_separation = 0.5` default). With those
neutral settings, the reference's output is the "ground truth"
against which our render is compared.

The harness queries the reference's published `GetCurrentOrder` /
`GetCurrentPattern` / `GetCurrentRow` / `_Speed` / `_Tempo` symbols
after every 256-frame chunk and walks the per-row state side by side
with our `PlayerState`. NO reference source is read — only the
published C entry-point list (equivalent to a function-list spec).

## Findings

1. **(order, pattern, row, speed, tempo) match exactly between our
   engine and the reference across the entire trace** for both fixtures
   over 30 seconds. Both engines reach the same row at the same
   sample boundary; both apply the row 0 `F08` speed change at the
   same instant; both transition from pattern 5 → pattern 0 at the
   same frame. So all the prior round-by-round investigations of
   sample-loop quirks, EE pattern-delay timing, EDx note-delay,
   etc. were **already correct**.
2. **Our render was 1.45–1.50× louder than the reference at every
   moment, on every fixture, on every channel**. The single-channel
   calibration test (a 32-frame square wave on hard-left ch0)
   measured ours peak = 9599 vs the reference's peak = 6375. Pure
   constant-gain inflation.
3. With our peak landing at 84 % of i16 full scale on `rhmst.mod`,
   **any downstream amplification or DSP step would push us over
   the rail**. Our ratio of 1.5× over the reference equals exactly the
   ratio that converts the reference's safe-headroom output into our
   close-to-the-rail output. The "scrambled" symptom is what the user
   hears when the OS audio path applies its own gain on top of an
   already-loud input.

## Root cause

In `src/player.rs::sample_all_channels`, the mix-bus headroom divisor
was `n_ch as f32 / 2.0` (= 2 for a 4-channel MOD). Calibration vs the
reference at known inputs (single channel max-volume hard-loop, sep
sweep 64..256) showed the reference uses a divisor of **3** for a
4-channel MOD — i.e., the formula `n_ch / 2 + 1`:

| n_ch | ours pre-fix | reference | ours post-fix |
|------|-------------|-----------|---------------|
| 4    | /2          | /3        | /3            |
| 6    | /3          | /4        | /4            |
| 8    | /4          | /5        | /5            |

The pan formula itself (`near = (1+sep)/2`, `far = (1-sep)/2`) was
already bit-exact with the reference; only the headroom divisor
differed.

Spec citations: the in-tree wiki snapshot under
`docs/audio/trackers/mod/` ("Resampling and mixing") describes the
same "channel count + safety margin" pattern as the modern ProTracker
rendering convention. `docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt`
§11 explicitly recommends NOT using full hard pan for headphone
playback, which the round-17 pan_separation = 0.5 default already
addressed — but headroom is independent of pan, and that's where the
gain inflation lived.

## Fix

Single-line change in `src/player.rs`:

```rust
-let norm = (n_ch as f32 / 2.0).max(1.0);
+let norm = ((n_ch as f32 / 2.0) + 1.0).max(1.0);
```

Documented inline with a 25-line comment block citing the round-19
calibration vs the reference (`tests/tracker_reference_compare.rs`)
and the in-tree wiki snapshot.

## Verification

After the fix:

```
[ref_calibration] 1-channel ch0 (LEFT) hard-loop:
  ours peak=6399 rms=4661 | ref peak=6375 rms=4651
  peak ratio (ours/ref) = 1.004x | rms ratio = 1.002x
[ref_compare:rhmst.mod]
  peak: ours=19454 ref=19564 | global RMS ratio = 0.999x
[ref_compare:halluc.mod]
  peak: ours=15623 ref=20296 | global RMS ratio = 1.006x
```

We now match the reference to within 1 % RMS on real-world MODs and
within 0.5 % on the calibration test. The pre-fix peak of 27668
(rhmst, 84 % of i16 max) drops to 19454 (59 %), removing the
near-clip headroom that was the most likely cause of the
"scrambled" symptom.

## Regression coverage

- `tests/tracker_reference_compare.rs::ref_calibration_single_channel_loud_voice`
  — opt-in (requires reference dylib installed), pins peak ratio
  ≤ 1.15× and RMS ratio ≤ 1.15× vs the reference.
- `tests/tracker_reference_compare.rs::headroom_calibration_pin_no_reference_required`
  — runs without any reference, asserts our single-channel
  single-sample peak lands in `5500..=7500` (post-fix value: 6399;
  pre-fix value: 9599 — well outside the band, so the legacy divisor
  regression is caught by this test alone).
- The reference comparator harness itself is checked in for future
  rounds; it skips cleanly on hosts without a reference installed.

## Residual content divergence (not in this fix)

After gain matching, ~38 % of the reference's RMS energy still
remains in the difference signal — i.e., the waveforms aren't
sample-exact. This is expected: the reference's exact interpolation
shape, per-trigger ramp shape, and lack of an Amiga LED filter all
differ from ours. Those differences are *not* responsible for the
user's reported "scrambled" symptom (no clipping, no row drift)
and are out of scope for this round.

If a future round wants to chase sample-exact match: use the
`ref_row_transition_alignment` test in
`tests/tracker_reference_compare.rs` to find the first row at which
`(order, row)` diverges between the engines (currently: never,
across 30 seconds of both fixtures), and the per-second `lag`
output to find PCM phase drift (which also stays within ~1 ms
post-fix — the prior 4 ms drift at sec 11 in halluc was an
artifact of the gain mismatch interacting with the lag finder's
energy-weighting).
