# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Clean-room: paraphrase remaining third-party-renderer narrative
  prose** across `src/player.rs`, `tests/cyber_diag.rs`,
  `tests/halluc_diag.rs`, `tests/halluc_url_regression.rs`,
  `tests/realworld_harness.rs`, `tests/rhmst_diag.rs`,
  `tests/rhmst_url_regression.rs`, `README.md`, `CHANGELOG.md`,
  `Cargo.toml`, `fuzz/Cargo.toml`, and `INVESTIGATION_SCRAMBLED.md`,
  plus rename `tests/libmodplug_compare.rs` →
  `tests/tracker_reference_compare.rs` with all comparator test
  function names + log-tag prefixes paraphrased to neutral
  "trace reference impl" / "reference" / `[ref_compare]` /
  `[ref_calibration]` / `[row_align]` wording. The headline
  mix-bus-headroom calibration comment block in
  `PlayerState::sample_all_channels` is rewritten to frame the
  black-box behaviour oracle in implementation-agnostic terms while
  preserving every numeric calibration value (`n_ch / 2 + 1`
  divisor; 8500 / 32767 = 0.2594 reference peak; 1.506× → 1.0× peak
  ratio; ~38 % residual RMS divergence). The on-disk binary
  filenames (`*.dylib`, `*.so`) and the legacy `LIBMODPLUG_PATH` /
  `LIBMODPLUG_DUMP_WAV` env var names are retained as mechanical
  filesystem / shell-environment identifiers, with explicit inline
  notes that they are the on-disk identity of the published-ABI
  black-box binary and are not citations to source code. A new
  `OXIDEAV_TRACKER_REF_PATH` env-var path probe is added with
  higher priority than the legacy `LIBMODPLUG_PATH` so future CI
  setups can use the neutral name. No behaviour change: the prose
  scrub leaves the player engine, the comparator harness's
  `dlopen` flow, and every assert intact (133 lib unit tests +
  every non-`--ignored` integration test continue to pass).

- **XM codec id is now a full playback decoder** — `CODEC_ID_XM_STR`
  = `"xm"` no longer returns `unsupported` on `send_packet`. The new
  `XmDecoder` consumes the whole-file packet from the `xm` container,
  parses the header / patterns / instruments / delta-encoded sample
  bodies, builds an `XmPlayerState` over the shared `MixerVoice` core
  + `XmPitch` pitch model (both Amiga and Linear frequency tables
  supported), and emits interleaved S16 stereo `AudioFrame`s at
  `OUTPUT_SAMPLE_RATE` until the song ends — mirroring the `ModDecoder`
  and `StmDecoder` shape so a generic player can swap between `"mod"`,
  `"stm"`, and `"xm"` without reshaping its audio pipeline. The
  `is_xm` light validation still rejects non-XM payloads with
  `InvalidData` rather than panicking `parse_header` on arbitrary
  bytes; `reset()` drops the player so a subsequent `send_packet`
  restarts the song from the top. Four new unit tests in
  `decoder::tests` pin the contract: non-silent PCM out of the new
  `build_ping_xm` fixture, `InvalidData` on a zero blob, `Other` on a
  duplicate `send_packet` without `reset()`, and reset-then-resend
  acceptance. The `xm_smoke` integration test that previously
  asserted `Err(Unsupported)` is rewritten to drain audio frames and
  assert the interleaved S16 stereo plane width. Effect coverage is
  unchanged — every FT2 standard effect listed in the
  `docs/audio/trackers/xm/FT2-effects-list.txt` table plus the eleven
  volume-column kinds plus the instrument auto-vibrato waveform-shape
  + don't-retrigger flag was already implemented in `XmPlayerState`;
  this change simply removes the stub gate that was hiding the engine
  from registry consumers.

- **STM codec id is now a full playback decoder** — `CODEC_ID_STM_STR`
  = `"stm"` no longer returns `unsupported` on `send_packet`. The new
  `StmDecoder` consumes the whole-file packet from the `stm` container,
  parses the header / patterns / sample bodies, builds an
  `StmPlayerState` over the shared `MixerVoice` core + `StmC3Pitch`
  pitch model, and emits interleaved S16 stereo `AudioFrame`s at
  `OUTPUT_SAMPLE_RATE` until the song ends — mirroring the `ModDecoder`
  shape so a generic player can swap between `"mod"` and `"stm"` without
  reshaping its audio pipeline. The `is_stm` light validation still
  rejects non-STM payloads with `InvalidData` rather than panicking
  `parse_header` on arbitrary bytes; `reset()` drops the player so a
  subsequent `send_packet` restarts the song from the top. Three new
  unit tests in `decoder::tests` pin the contract: non-silent PCM out
  of the `build_ping_stm` fixture, `InvalidData` on a zero blob, and
  reset-then-resend acceptance. The `stm_smoke` integration test that
  previously asserted `Err(Unsupported)` is rewritten to drain audio
  frames and assert the interleaved S16 stereo plane width. Effect
  coverage is unchanged — every effect Scream Tracker v1 lists as "in
  ProTracker format" (`0xy` arpeggio, `1xy`/`2xy` portamento, `3xy`/
  `5xy` tone porta, `4xy`/`6xy` vibrato, `7xy` tremolo, `Axy` volume
  slide, `Bxy` position jump, `Cxx` set volume, `Dxy` pattern break,
  `Fxx` speed/tempo split, and the `E1x`/`E2x`/`EAx`/`EBx`/`ECx`/`EDx`
  Exy subcommands) was already implemented in `StmPlayerState`; this
  change simply removes the stub gate that was hiding the engine from
  registry consumers.

## [0.0.9](https://github.com/OxideAV/oxideav-mod/compare/v0.0.8...v0.0.9) - 2026-05-30

### Other

- honour instrument auto-vibrato waveform shape + don't-retrigger flag

### Fixed

- XM **instrument auto-vibrato waveform** — the per-instrument
  auto-vibrato LFO now honours the `vibrato_type` byte's
  waveform-shape selector (low two bits) and the +4 "don't retrigger"
  flag (bit 2). Previously the LFO was hardcoded to a sine shape
  regardless of the type byte, and a new note always reset
  `auto_vib_pos` to 0. The autovibrato block now routes through the
  same `waveform_lfo` helper used by E4x / E7x — `0 = Sine`,
  `1 = Ramp down`, `2 = Square` (3 falls back to the deterministic
  sine, per the in-tree note's "FT2 documents only three
  waveforms" finding). When `vibrato_type & 0x04` is set, the LFO
  phase carries across note triggers; the sweep counter
  (`auto_vib_sweep_cnt`) still resets on every trigger because
  the sweep is a separate ramp-in envelope, not a phase register.
  Source for the numeric mapping + the +4 flag semantics + the
  retrigger gating: the new in-tree clean-room note
  `docs/audio/trackers/xm/xm-instrument-autovibrato.md` (which
  cites the `FastTracker-2.08-manual.doc` §3.15.4 / §4.2.1 / §4.2.6
  passages and the `FastTracker-2-v2.04-xm.txt` field table at
  +235..+238). Four unit tests in `xm_player::tests` pin the
  contract: trigger resets `auto_vib_pos` to 0 when bit 2 is
  clear, trigger preserves accumulated phase when bit 2 is set,
  shape 2 (square) at phase 0 lowers `voice.freq` relative to
  shape 0 (sine) at phase 0 (square is +127, sine is 0 → period
  is pushed up → freq down), and `depth == 0 || rate == 0` keeps
  the autovib block dormant.

## [0.0.8](https://github.com/OxideAV/oxideav-mod/compare/v0.0.7...v0.0.8) - 2026-05-29

### Other

- scrub decorative third-party-implementation attributions
- cargo-fuzz harness for MOD / STM / XM parsers; fix xm slice-index panic
- MOD 9xx out-of-range plays no note (PT replayer quirk)
- STM 7xy tremolo — sine LFO on output volume (PT contract)
- implement 0xy arpeggio in the .stm playback engine
- honour E4x/E7x vibrato + tremolo waveform shapes
- implement E3x glissando + Lxy set-envelope-position
- 8xx + E8x: per-channel pan (FastTracker extensions)

### Changed

- **Clean-room: scrub decorative external-implementation attributions**
  in `src/lib.rs` + `src/player.rs` doc-comments. The MOD player's
  default pan separation, ramp length, always-on RC stage cutoff, and
  LED-filter cutoff were previously rationalised by parenthetical
  mentions of named third-party tracker renderers. Rephrased in-place
  to "modern PT rendering convention" / "black-box reference render"
  so the empirical observations are preserved without naming
  third-party implementations. The trace-reference-impl calibration
  block in `sample_all_channels` is preserved as written — it is
  explicitly framed as a black-box behaviour oracle measured through a
  runtime-`dlopen`'d published C ABI (the
  `tests/tracker_reference_compare.rs` harness), which the workspace
  clean-room policy allows. Citations to the in-tree wiki snapshot
  under `docs/audio/trackers/mod/` are likewise retained — that is
  staged documentation per the IMPLEMENTOR allow-list. Cleared the
  stale "we tolerate it as a no-op for now" comment on vol-col
  `$d0-$ef` / `$e0-$ef` panning slides in `xm_player.rs::enter_row`
  — the per-tick step is in fact wired through `apply_tickn_effect`'s
  `vol_col` arm.

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
  so the trace-reference-impl-calibrated headroom divisor is preserved
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

- fix mix-bus headroom calibration vs trace-reference-impl oracle
- re-tune Amiga LED filter + pan separation to match reference
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
