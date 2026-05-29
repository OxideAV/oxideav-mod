# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Clean-room: scrub decorative external-implementation attributions**
  in `src/lib.rs` + `src/player.rs` doc-comments. The MOD player's
  default pan separation, ramp length, always-on RC stage cutoff, and
  LED-filter cutoff were previously rationalised by parenthetical
  mentions of `xmp / openmpt / libxmp / libopenmpt / openmpt123 --render`
  ("matching xmp/openmpt/libxmp's default", "the 'ramping=2' default in
  libopenmpt", "(xmp, openmpt) effectively bypass this stage in their
  default render path", "rendering chains (xmp, openmpt's default)
  converge on", "every modern PT player (xmp, openmpt, libxmp's stock
  build) does the same partial bleed"). Rephrased in-place to "modern
  PT rendering convention" / "black-box reference render" so the
  empirical observations are preserved without naming third-party
  implementations. The `libmodplug 0.8.9.0` calibration block stays as
  written — it is explicitly framed as a black-box behaviour oracle
  measured through the runtime-`dlopen`'d public C API (the
  `tests/libmodplug_compare.rs` harness), which the workspace
  clean-room policy allows. Citations to the in-tree wiki snapshot
  `docs/audio/trackers/mod/openmpt-module-formats.html` are likewise
  retained — that is staged documentation per the IMPLEMENTOR
  allow-list. Cleared the stale "we tolerate it as a no-op for now"
  comment on vol-col `$d0-$ef` / `$e0-$ef` panning slides in
  `xm_player.rs::enter_row` — the per-tick step is in fact wired
  through `apply_tickn_effect`'s `vol_col` arm.

### Added

- XM volume-column **panning-slide unit tests** in `xm_player::tests`
  pinning the per-tick `$d0-$df` (slide left) and `$e0-$ef` (slide
  right) semantics under `apply_tickn_effect`: nibble-magnitude step,
  `0`/`255` clamps, and per-tick repeat. Cited from
  `docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt` §"Effects in
  volume column" lines 257-258 + the prelude "All effects in the
  volume column should work as the standard effects".
- **`cargo-fuzz` harness** under `fuzz/` driving the three parser
  pipelines (MOD / STM / XM) against arbitrary attacker-controlled
  bytes. Three targets: `mod_decode` runs `header::parse_header` →
  `player::parse_patterns` → `samples::extract_samples` →
  `player::PlayerState::new` → 2048-frame `render`; `stm_decode`
  runs the equivalent STM pipeline through `stm_player::
  StmPlayerState::render`; `xm_decode` runs the equivalent XM
  pipeline including `xm::parse_instruments` and
  `xm::extract_sample_bodies` through `xm_player::
  XmPlayerState::render`. Each target ships a minimal valid-header
  seed (`fuzz/corpus/<target>/minimal.{mod,stm,xm}`) so libfuzzer's
  coverage hill-climb starts from a parser-accepting input rather
  than coin-flipping for the signature byte. Per the
  workspace clean-room rule the harness asserts "the call returns"
  rather than cross-decoding against an external oracle. Bootstrap
  session caught one XM bug, fixed below.

### Fixed

- XM **`parse_patterns` slice-index panic on hostile `header_length`**
  — a pattern whose declared `header_length` pushed `data_start`
  past EOF would slice `&bytes[data_start..data_end]` and panic
  with "slice index 0xFFFF out of bounds" rather than clamping. The
  prior `.min(bytes.len())` on `data_end` only protected the upper
  bound; the lower bound is now also clamped against `bytes.len()`,
  so a hostile header_length collapses to an empty packed-data
  slice and `decode_packed_cell` returns the default cell with
  zero consumed bytes. Caught by `oxideav-mod-fuzz/xm_decode`
  (crash `212b2111`) in the harness bootstrap session above; pinned
  by a new unit test (`parse_patterns_hostile_header_length_does_
  not_panic` in `src/xm.rs`) that constructs a 1-pattern XM whose
  pattern header declares `header_length = 0xFFFF` and asserts the
  parser returns `Ok` with a single default-cell row rather than
  panicking.

- MOD **9xx out-of-range quirk** — a `9xx` sample-offset that lands at
  or past the end of the sample now plays **no note at all** on that
  channel, matching the ProTracker replayer behaviour documented in
  `docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt`
  9:Set-sample-offset (lines 1240-1242: "Note that if the effect is out
  of range (e.g. if it tries to jump beyond the end of the sample) NO
  NOTE WILL BE PLAYED!"). Previously the player set `sample_pos` to the
  over-range offset unconditionally, which silenced a one-shot sample on
  the first mix call but let a **looped** sample's over-range cursor wrap
  back into the loop region — audibly retriggering the loop from a fresh
  note, exactly the artefact the spec says must not happen. The check
  runs at note-trigger time in `enter_row`: when a `9xx` is present and
  `offset >= sample.pcm.len()`, the channel is left inactive
  (`active = false`, `sample_pos = 0`) and the sample cursor / per-trigger
  ramp / LFO retrigger are skipped. The `9xx` parameter memory is still
  latched (the note info is updated; only playback is suppressed). Three
  unit tests pin the contract: a far-out-of-range offset renders silence
  (no wrapped-loop artefact), an offset landing exactly at the sample
  length is suppressed (`>= end`), and an offset comfortably inside the
  sample still triggers.
- STM **7xy tremolo** — the Scream Tracker v1 (`.stm`) player now
  honours effect 7 with a sine LFO on the output volume, parallel
  to the existing 4xy vibrato pipeline. New `trem_pos` / `trem_speed`
  / `trem_depth` state on `StmChannel` (independent of vibrato's
  so 4xy + 7xy on the same channel don't share phase), per-nibble
  parameter memory (zero nibble = reuse last non-zero value, per
  `Protracker-effects-MODFIL12.txt` 7:Tremolo "If either xxxx or
  yyyy are 0, then values from the most recent prior tremolo will
  be used"), phase reset on note-trigger and note-delay trigger
  (matching the vibrato retrigger contract), and `[0, 64]` volume
  clamping per `multimedia-cx-protracker.html` 7xy ("Like vibrato,
  except we modify the output volume … clamped to 0 <= vol <= 64").
  The offset is added on top of the live `ch.volume` baseline so
  Cxx / Axy / EAx / EBx continue to control the centre value the
  tremolo modulates around. Three unit tests pin the contract:
  symmetric swing below and above baseline at depth 15, memory
  reuse on a `700` continuation row, and inert behaviour when both
  nibbles are zero with empty memory.
- STM **0xy arpeggio** — the Scream Tracker v1 (`.stm`) player now
  honours effect 0: with at least one non-zero nibble it cycles the
  pitch through note / note+x / note+y half-steps across the row's
  ticks (`counter mod 3`), then back to the note, matching the
  `Protracker-effects-MODFIL12.txt` 0:Arpeggio algorithm STM inherits
  via its "ProTracker format" effect column. The offset is a pure
  additive shift on the live semitone position, so porta / vibrato
  continue underneath and `cur_semis` is left unmodified (no drift).
  A zero parameter (`000`) is inert per the spec. Cited in
  `docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt` lines
  1000-1045 and `docs/audio/trackers/stm/ScreamTracker-v1.0-stm.txt`
  ("Effect command in ProTracker format").
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
