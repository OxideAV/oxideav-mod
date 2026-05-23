# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- XM **E4x / E7x vibrato + tremolo waveform shapes** — the LFO shape
  selected by E4x (vibrato) / E7x (tremolo) is now applied to the
  modulation, not just the bit-2 retrigger flag. New `waveform_lfo`
  helper returns the per-cycle value on the same ±127 scale as the
  sine table: shape 0 sine, 1 downward saw, 2 square ("starting from
  +y"), 3 random (deterministic sine fallback — no PRNG documented).
  Both the vibrato and tremolo motion paths now route through it
  instead of the hardcoded sine table. Cited in
  `multimedia-cx-protracker.html` §4xy (64-step shape catalogue),
  `Protracker-effects-MODFIL12.txt` E4/E7, and
  `Protracker-2.3A-misc-info.txt` lines 387/390.
- XM **E3x glissando control** — when on, tone-porta (3xy / 5xy /
  vol-col Mx) snaps the period to the nearest semitone after each
  tick's linear slide step. Works in both Linear and Amiga pitch
  tables (Linear snaps to the 64-unit semitone grid; Amiga walks
  `XmPitch::PERIOD_TAB_PUB` across the 10-octave span and picks the
  nearest table entry by absolute period error). Cited in
  `FastTracker-2-v2.04-xm.txt` line 222.
- XM **Lxy set envelope position** (effect 0x15) — moves the volume
  envelope tick cursor to `param`; segment index is reset to 0 so the
  next `tick_envelope` call re-aligns from the segment chain. Pan
  envelope is left untouched, matching the FT2 reading. Cited in
  `FastTracker-2-v2.04-xm.txt` line 226 and
  `multimedia-cx-fasttracker-2.html` §2.1.20.
- `tone_porta_step` / `snap_to_semitone` helpers, factored out of
  `apply_tickn_effect` so the 3xy, 5xy, and volume-column Mx
  branches share the same single-step + glissando-snap path.
- `glissando: bool` field on `XmChannel` (default off).
- `8xx` Set FINE Panning (FT extension): per-channel `pan: u8`
  (0 = hard LEFT, 255 = hard RIGHT) overwrites the Amiga LRRL
  hard-pan default. Cited in `Protracker-effects-MODFIL12.txt`
  lines 1201-1207.
- `E8x` Set ROUGH Panning (FT extension): per-channel pan from
  the nibble replicated into both halves of the byte (`E80` →
  0x00, `E8F` → 0xFF). Cited in `Protracker-effects-MODFIL12.txt`
  lines 1503-1505 and `multimedia-cx-protracker.html` E8x.
- New `pan_gains(p, s)` helper that derives per-channel L/R gains
  from the 8-bit pan and the global `pan_separation`. Collapses to
  the prior hard-LRRL formula at the endpoints (pan = 0 or 255)
  so the libmodplug-calibrated headroom divisor is preserved
  bit-for-bit on MODs that don't use 8xx / E8x. Centred channels
  (pan = 128) split evenly regardless of separation.

## [0.0.7](https://github.com/OxideAV/oxideav-mod/compare/v0.0.6...v0.0.7) - 2026-05-06

### Other

- reframe FFI claim — HW-engine crates use OS FFI by necessity
- drop stale REGISTRARS / with_all_features intra-doc links
- drop dead `linkme` dep
- registry calls: rename make_decoder/make_encoder → first_decoder/first_encoder
- auto-register via oxideav_core::register! macro (linkme distributed slice)
- unify entry point on register(&mut RuntimeContext) ([#502](https://github.com/OxideAV/oxideav-mod/pull/502))

## [0.0.6](https://github.com/OxideAV/oxideav-mod/compare/v0.0.5...v0.0.6) - 2026-05-03

### Other

- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows
- round 19 — close standard-effect coverage gaps
- pin Sadness.mod (infinite-stream MOD) URL regression
- adopt slim VideoFrame/AudioFrame shape
- fix arpeggio base period persistence across no-note rows

## [0.0.5](https://github.com/OxideAV/oxideav-mod/compare/v0.0.4...v0.0.5) - 2026-04-26

### Other

- fix mix-bus headroom calibration vs libmodplug oracle
- re-tune Amiga LED filter + pan separation to match openmpt reference
- pin 2-pole filter + per-trigger ramp on rhmst.mod fixture
- configurable pan separation + 2-pole Amiga output filter
- fix EE pattern-delay note-retrigger + add real-world harness
- implement Amiga LED filter, extended period range, plus PT-fidelity tests
- fix loop-boundary + sample-swap PT quirks for real-world fidelity
- pin release-plz to patch-only bumps

## [0.0.4](https://github.com/OxideAV/oxideav-mod/compare/v0.0.3...v0.0.4) - 2026-04-25

### Other

- fix clippy 1.95 lints
- drop oxideav-codec/oxideav-container shims, import from oxideav-core
- add decoder-path playback tests covering vibrato / break / vol slide / speed
- replace "main effects" blurb with spec-coverage matrix
- fill in full ProTracker 1.1B effect coverage
- add vibrato, tone porta, pattern jumps + Exy fine effects
- add vibrato, tone porta, pattern jumps, restart + fine effects
- add volume+panning envelopes, fadeout, and key-off handling
- shared tracker mixer + STM & XM basic playback
- add FastTracker 2 (.xm) structural parser + container
- add Scream Tracker v1 (.stm) structural parser + container
- bump oxideav-container dep to "0.1"
- drop Cargo.lock — this crate is a library
- bump to oxideav-core 0.1.1 + codec 0.1.1
- migrate register() to CodecInfo builder
- bump oxideav-core + oxideav-codec deps to "0.1"
- thread &dyn CodecResolver through open()
