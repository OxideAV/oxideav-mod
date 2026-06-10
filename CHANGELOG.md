# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Startrekker `FLT8` paired-pattern layout** (`src/header.rs`,
  `src/player.rs`). `FLT8` modules keep the normal 4-channel
  0x400-byte stored-pattern layout and pair two consecutive stored
  patterns into one logical 8-channel pattern — stored `2k` carries
  channels 1-4 and stored `2k+1` carries channels 5-8 for every row,
  while the on-disk order table references the even stored-pattern
  indices. Per `docs/audio/trackers/mod/Startrekker-mod.txt` (the
  format author's own description: "the patterns are PAIRED … in a 8
  track FLT8 module, patterns 00 and 01 is 'really' pattern 00", and
  the format summary's "Divide all patterns in the orderlist by 2").
  `parse_header` now halves the `FLT8` order entries so `order` /
  `n_patterns` are in logical-pattern terms (which also fixes the
  sample-body offset — the previous flat read over-counted the
  pattern region whenever the order table held the doubled indices),
  exposes the new `ModHeader::is_flt8()` predicate, and
  `player::parse_patterns` resolves the paired layout into logical
  64-row × 8-channel patterns. `8CHN` / `OCTA` / `CD81` keep the flat
  interleaved read. Four tests pin the surface: order-halving +
  byte-count identity (`flt8_order_entries_are_halved_to_logical_patterns`),
  the 8CHN non-pairing control
  (`non_flt8_eight_channel_signature_is_not_paired`), the paired
  cell remap incl. the row/channel math inside the second stored
  pattern (`flt8_pairs_stored_patterns_into_one_logical_pattern`),
  and a playback smoke asserting voices fire on both halves of the
  pair (`flt8_playback_triggers_both_pattern_halves`).

- **STM `E4x` / `E7x` vibrato + tremolo waveform control**
  (`src/stm_player.rs`). The Scream Tracker v1 player now honours the
  ProTracker "set vibrato waveform" / "set tremolo waveform"
  sub-commands per `docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt`
  E4/E7 — STM declares its effect column as "in ProTracker format" per
  `docs/audio/trackers/stm/ScreamTracker-v1.0-stm.txt`, so the PT
  semantics carry across verbatim. Two new sticky per-channel
  selectors (`vib_waveform` / `trem_waveform`) are latched by the
  tick-0 Exy dispatcher; the 4xy/6xy vibrato and 7xy tremolo LFO
  lookups now route through the shared
  `crate::xm_player::waveform_lfo` helper (made `pub(crate)`; the
  STM-local 64-entry sine table is removed since shape 0 of the
  shared catalogue is the identical table) — 0 sine (default),
  1 ramp down, 2 square, 3 random (deterministic sine fallback, no
  PRNG documented). Bit 2 (+4) is the "No Retrigger" flag: per the
  E4 doc table ("A 'retriggered' waveform will be reset to the start
  of a cycle at the beginning of each new note. If a wave is
  selected 'without retrigger', the previous waveform will be
  continued"), the LFO phase reset is now gated on that bit at all
  three realignment sites — row-entry note-on, EDx delayed trigger,
  and the E9x per-tick retrigger (whose previous unconditional reset
  was justified by the waveform flags not being honoured yet). Four
  unit tests in `stm_player::tests` pin the surface:
  `e4x_square_vibrato_shifts_pitch_at_phase_zero` (square at LFO
  phase 0 sits at its +127 peak and deviates the pitch on the
  row's first tick, while the default sine is zero-deviation),
  `e4x_no_retrigger_bit_preserves_vibrato_phase_across_notes`
  (E44 keeps `vib_pos = 20` across a fresh note-on and the
  continued phase audibly deviates tick-0 pitch; E40 resets to 0),
  `e7x_square_tremolo_lifts_volume_at_phase_zero` (square tremolo
  lifts a 32/64 baseline to ~1.0 at phase 0; sine leaves it at
  0.5), and `e9x_retrig_respects_no_retrigger_waveform_bit` (the
  E9 cursor rewind still happens but the +4 bit keeps both LFO
  phases). README STM effect table picks up the row.

- **Typed MOD pattern-row predicates on `player::Note`**
  (`src/player.rs`). Four purely additive `#[inline]` methods fold
  the field-vs-zero idioms scattered across the playback engine
  into one canonical surface, mirroring the
  `header::Sample::is_looped` / `loop_region` pair and the XM
  sample-header accessors already shipping in the crate:
  - `Note::has_period()` — true when the 12-bit period field is
    non-zero, i.e. the row carries a new note to trigger. The
    "no new note" semantics come straight from
    `docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt` §3.4
    where each of effects 1 (Slide up), 2 (Slide down) and 3 (Slide
    to note) closes with "at the beginning of the next line, if
    there is not a new note to be played the period is again
    decremented…" — a zero period field is the canonical
    placeholder that lets the prior channel state continue.
  - `Note::has_sample()` — true when the 8-bit sample number is
    non-zero. Per the same spec §2.7 "If sample number is
    specified on a channel (sample #0), then the last sample used
    on that channel will be remembered if new notes come along."
    Counts 1..=31 are valid sample indices into the header's
    sample table.
  - `Note::has_effect()` — true when either the command nibble or
    the parameter byte is non-zero. The joint test is needed
    because command 0 with a non-zero param is the `0xy` arpeggio
    effect (not "no effect") and command 0 with a zero param is
    the canonical "no effect" placeholder — single-field tests
    would mis-classify both edges.
  - `Note::is_empty()` — true iff every field is zero. Models the
    `0000 0000-0000` idle row from the `Protracker-mod.txt` §"Pattern
    data" sample table; pattern walkers can fast-skip per-channel
    branches when the row contributes nothing.
  Internal call sites in `player.rs` (the row-dispatch path around
  the tone-portamento, note-delay and normal-trigger branches) now
  consume these accessors instead of open-coding
  `note.period != 0` / `note.sample != 0` against the struct
  fields directly, so the typed surface is exercised by the
  existing playback test suite rather than only the new unit
  tests. Five unit tests in `player::tests` pin the surface:
  `note_has_period_keys_on_period_field`,
  `note_has_sample_keys_on_sample_field`,
  `note_has_effect_keys_on_both_command_and_param`,
  `note_is_empty_requires_every_field_zero`, and
  `note_predicates_agree_on_decoded_pattern_row` (which feeds a
  synthetic 4-byte cell through `Note::decode` and checks the
  predicates agree with direct field inspection).

- **Typed XM sample-header pitch-transpose accessors** (`src/xm.rs`).
  Two purely additive methods on `XmSampleHeader` fold the FT2
  pitch-field conventions into floating-point surfaces, mirroring
  the byte-vs-frame split already shipping for the loop accessors:
  - `XmSampleHeader::finetune_semitones()` converts the signed-byte
    `finetune` field at +13 of the sample header
    (`docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt` +13 —
    "Finetune (signed byte)") to a fractional-semitone offset by
    dividing by 128. The divisor comes straight from the Linear
    period formula on +96 of the same doc
    (`Period = 10*12*16*4 - Note*16*4 - FineTune/2`), where 64
    period units = 1 semitone and 2 finetune units = 1 period
    unit, so 128 finetune units = 1 semitone. Documents the
    discrepancy between the spec's UI-range wording ("signed byte
    -16..+15", echoed verbatim by the multimedia-cx aggregator on
    +213) and the on-disk -128..+127 byte range — the period
    formula is the load-bearing definition.
  - `XmSampleHeader::transpose_semitones()` sums the integer-
    semitone `relative_note` field (+16 of the sample header) with
    the fractional `finetune_semitones()` result, returning the
    total pitch offset relative to the cell's note as a single
    `f32`. The xm_player engine keeps the two fields separate
    because `note_to_period` consumes them at different sub-unit
    scales (16 sub-units per semitone for the note term, 2
    sub-units per period for the finetune term); this accessor
    is the canonical metadata surface for tuning UIs and
    transcription tools.
  Five unit tests in `xm::tests` pin the surface:
  `xm_sample_finetune_semitones_is_zero_at_neutral` (finetune 0
  → 0.0 semitones),
  `xm_sample_finetune_semitones_scales_at_one_over_128`
  (finetune 64 → 0.5 semitones, finetune -128 → -1.0 semitone),
  `xm_sample_finetune_semitones_symmetric_around_zero`,
  `xm_sample_transpose_semitones_sums_relative_note_and_finetune`
  (relative_note=12 + finetune=64 → 12.5 semitones), and
  `xm_sample_transpose_semitones_pure_relative_note` (zero
  finetune passes through unchanged).

- **Typed XM sample-header accessors** (`src/xm.rs`). Three purely
  additive methods on `XmSampleHeader` fold the byte-vs-frame and
  loop-mode bookkeeping into one canonical surface, mirroring the
  `header::Sample::is_looped` / `loop_region` pair already shipping
  for the MOD parser:
  - `XmSampleHeader::is_looped()` returns `true` for `Forward` and
    `PingPong` loop modes, `false` for `None` — keyed on the type
    byte's bits 0-1 per the FT2 sample-header field table at
    `docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt` +14
    ("Bit 0-1: 0 = No loop, 1 = Forward loop, 2 = Ping-pong loop").
    Unlike MOD's `repeat_length == 2` sentinel, FT2 keys loop
    presence on the type byte alone, so a length-zero loop is still
    classified as looped here (the mixer's `SampleSource::loop_end`
    impl owns the PCM-aware clamp).
  - `XmSampleHeader::loop_region_frames()` returns
    `Some((start_frames, length_frames))` when looped, `None`
    otherwise. The on-disk `loop_start` / `loop_length` fields are
    **byte** offsets per the +4 / +8 entries of the same field
    table; this accessor divides by 2 when `is_16_bit` is set so
    callers reading the header land in the same frame-index space
    as the `SampleSource` cursor. No clamp against the extracted
    PCM body — that's still the trait impl's job, because the
    extracted body length can be shorter than the declared
    `length` on truncated rips.
  - `XmSampleHeader::length_frames()` returns the frame count
    (`length` divided by 2 for 16-bit samples), folding the same
    width conversion into a single canonical surface so callers
    reasoning in frame indices don't repeat the conditional.
  Seven unit tests in `xm::tests` pin the surface:
  `xm_sample_is_looped_tracks_loop_mode_enum` (None / Forward /
  PingPong classification), `xm_sample_loop_region_none_when_not_looped`
  (typed view returns None even when raw byte fields carry leftover
  values — the type byte is authoritative),
  `xm_sample_loop_region_passes_through_8bit_bytes_as_frames`,
  `xm_sample_loop_region_halves_16bit_bytes_into_frames`,
  `xm_sample_loop_region_returns_header_pair_unclamped` (raw values
  pass through even when start + length exceed declared length),
  `xm_sample_length_frames_handles_both_widths`, and
  `xm_sample_pingpong_is_classified_as_looped`.

- **STM `E3x` glissando control** (`src/stm_player.rs`). The Scream
  Tracker v1 player now honours the ProTracker `E3x` "set glissando
  on/off" sub-command per
  `docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt` E3 ("If
  glissando is on, then the 'Slide to note' will slide a half note
  at a time. Otherwise, it will perform the default smooth slide.")
  — STM declares its effect column as "in ProTracker format" per
  `docs/audio/trackers/stm/ScreamTracker-v1.0-stm.txt`, so the PT
  semantics carry across verbatim. A new `glissando: bool` flag on
  `StmChannel` is set by `E3y` (y != 0 → on, y == 0 → off) and is
  sticky across rows until a subsequent `E3y` overwrites it. The
  tone-porta tick handlers for `3xy` and `5xy` now go through a
  shared `tone_porta_step` helper that, when the flag is on,
  quantises `cur_semis` to the nearest whole semitone via
  `.round()` after every linear-slide increment. Six tests pin
  the surface in `stm_player::tests`: `e3x_set_glissando_latches_flag_on`,
  `e30_clears_glissando_flag`,
  `glissando_snaps_tone_porta_to_nearest_semitone` (`tone_porta_step`
  walking 48.0 → 50.0 at speed 4 = 0.25 semis/tick produces only
  integer-semitone values when glissando is on),
  `no_glissando_lets_tone_porta_walk_fractional_semitones` (the
  same walk produces fractional values when off),
  `glissando_works_with_5xy_tone_porta_plus_volume_slide` (the
  combined-effect path snaps too while the volume-slide piece
  still increments), and `glissando_persists_across_rows_until_cleared`
  (an `E31` flag survives an empty intermediate row and is cleared
  by a later `E30`). README STM effect table picks up the row.
- **`header::Sample::is_looped` + `loop_region` typed accessors**
  (`src/header.rs`). Two purely-additive methods on the public
  `Sample` struct that fold the "repeat_length of `0` or `2` means
  no loop" sentinel rule into one place: `is_looped()` returns
  `true` iff `repeat_length > 2`, and `loop_region()` returns
  `Some((repeat_start, repeat_length))` when the sample loops or
  `None` when it does not. Per
  `docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt`
  lines 357-365 ("A sample is only looped if this value is greater
  than 2 bytes"), so callers (metadata reporters, tracker UIs,
  diagnostics) that previously had to spell `s.repeat_length > 2`
  inline at every check site now have one canonical accessor.
  The accessors deliberately return the header-side raw values
  without PCM-bounded clamping — `samples::extract_samples` still
  owns the PCM-aware clamp when it builds the mixer-facing
  `SampleBody`, because the extracted body length can be shorter
  than the declared `length` on truncated rips and the two views
  are documented as serving different consumers. Four unit tests
  in `header::tests` pin the surface:
  `sample_is_looped_rejects_repeat_length_zero_and_two` (both
  no-loop sentinels return `false`),
  `sample_is_looped_accepts_repeat_length_above_two` (length 3 +
  4 + 256 all loop),
  `sample_loop_region_none_when_not_looped` (`Option` shape
  matches `is_looped`, and a non-zero `repeat_start` with the
  no-loop length sentinel still returns `None` because PT
  consults the length, not the start), and
  `sample_loop_region_returns_header_pair_unclamped` (the raw
  `(start, length)` pair passes through even when the values
  exceed the declared `length`, which is the contract that lets
  the PCM-aware path do its own clamp).

- **XM `Rxy` multi-retrig per-nibble memory** (`src/xm_player.rs`).
  The two nibbles of `Rxy` carry **independent** memories per the FT2
  wiki snapshot at
  `docs/audio/trackers/xm/multimedia-cx-fasttracker-2.html` §2.1.22:
  `y = 0` reuses the last nonzero retrig speed seen on the channel,
  and `x = 0` reuses the last nonzero volume modifier seen on the
  channel — the wiki explicitly flags the FT2 manual's "None"
  wording for `x = 0` as wrongly documented (the actual behaviour is
  "reuse last nonzero modifier", NOT "leave volume unchanged"). Two
  new per-channel state fields, `multi_retrig_x_mem` and
  `multi_retrig_y_mem`, are latched at row entry on a nonzero nibble
  (a zero nibble does NOT clobber its memory). The per-tick resolver
  in `advance_tick` now consults the per-nibble memories instead of
  the combined-byte fallback: `rx = row_x != 0 ? row_x : x_mem` and
  `ry = row_y != 0 ? row_y : y_mem`. When a memory slot has never
  been seeded, the `x` fallback walks through the existing modifier
  table entry 0 ("leave volume unchanged") and the `y` fallback
  short-circuits the retrig fire entirely (the `ry > 0` gate). The
  combined-byte `multi_retrig_mem` field is preserved for legacy
  callers. Five unit tests in `xm_player::tests` pin the surface:
  `rxy_speed_nibble_zero_reuses_last_nonzero_speed` (R01 on row 0
  seeds y_mem = 1, R00 on row 1 must reuse it and retrig every
  tick), `rxy_volume_nibble_zero_reuses_last_nonzero_modifier` (R51
  on row 0 seeds x_mem = 5 with modifier −16, R01 on row 1 must
  reuse the −16 modifier and drop the volume further),
  `rxy_x_zero_without_memory_is_inert_on_volume` (R03 with no prior
  nonzero x leaves volume at 64),
  `rxy_y_zero_without_memory_does_not_retrigger` (R50 with no prior
  nonzero y never retriggers), and
  `rxy_x_and_y_have_independent_memories` (a row with `(x=0, y=3)`
  updates only y_mem, leaving x_mem at the previous row's value).
  Spec source: `docs/audio/trackers/xm/multimedia-cx-fasttracker-2.html`
  §2.1.22 ("If y is 0, use the last nonzero retrig speed value …" +
  "Values for x: 0 — Use the last nonzero volume modifier …
  Wrongly documented as: None").

- **STM `E9x` retrigger-note effect** (`src/stm_player.rs`). On each row
  entry the `E9y` cell latches `y` as the per-channel retrigger period;
  the per-tick handler in `advance_tick` then rewinds the active voice's
  sample cursor to `0.0` (and resets the vibrato + tremolo LFO phases)
  whenever the current tick is a non-zero multiple of `retrig_ticks`.
  Tick 0 is the row's initial note-on so the modulo schedule starts from
  tick 1; `E90` (explicit zero period) is documented as the inert /
  no-op selection and leaves the cursor alone. The schedule is gated on
  `voice.active`, so a silenced channel isn't resurrected by a residual
  retrig schedule. The period register is row-scoped: row entry zeroes
  `retrig_ticks` before the tick-0 effect handler captures a fresh `E9y`
  value, so a row without `E9` cannot inherit the previous row's period.
  Spec source: `docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt`
  E9 ("re-trigger a specified sample at a particular note after yyyy
  ticks during the line … This effect is used mostly with samples of
  hi-hats") — STM declares its effect column as "in ProTracker format"
  per `docs/audio/trackers/stm/ScreamTracker-v1.0-stm.txt`, so the PT
  semantics carry across verbatim. Five tests pin the surface:
  `e9x_retrigger_resets_sample_cursor_every_y_ticks` (E91 rewinds on
  ticks 1 and 2), `e90_does_not_retrigger` (E90 captures period = 0 and
  performs no rewind), `e9x_only_fires_on_tick_y_multiples` (E93 leaves
  ticks 1, 2, 4 alone and rewinds tick 3),
  `e9x_does_not_retrigger_on_inactive_voice` (silenced channel is not
  resurrected), and `e9x_period_does_not_leak_into_next_row` (a fresh
  row without E9 clears `retrig_ticks` so no spurious rewind fires).

- **STM `9xx` set-sample-offset effect** (`src/stm_player.rs`). On a
  note-trigger row, `9xx` places the channel's playback cursor at
  `xx * 0x100` bytes into the sample body. A non-zero `xx` is latched
  into a per-channel `mem_sample_offset` register so a subsequent
  `900` row reuses the same offset (per the canonical PT reading in
  `Protracker-effects-MODFIL12.txt` 9:Set-sample-offset: "9xx has its
  own memory. 900 plays the sample at 9xx_memory*0x100"). If the
  resulting offset lands at or past the end of the sample, the
  channel is silenced rather than letting the mixer wrap the cursor
  (the spec's "if the effect is out of range … NO NOTE WILL BE
  PLAYED!" quirk). The note's pitch metadata still latches so a
  follow-up tone-porta / arpeggio row anchors to the intended
  semitone. STM declares its effect column as "in ProTracker format"
  per `docs/audio/trackers/stm/ScreamTracker-v1.0-stm.txt`, so the PT
  semantics carry across verbatim. Five tests pin the surface:
  `nine_xx_starts_sample_at_param_times_0x100` (basic in-range
  landing), `nine_xx_param_zero_reuses_memory` (900 reuses latched
  `xx` and does not overwrite memory with zero),
  `nine_xx_out_of_range_plays_no_note` (offset past sample end
  silences but still latches pitch), `nine_xx_at_exact_end_plays_no_note`
  (boundary == sample_len also silences), and
  `nine_xx_just_inside_end_plays` (offset < sample_len still
  triggers, cursor lands exactly on the requested frame).

- **STM `E6x` pattern loop + `EEx` pattern delay**
  (`src/stm_player.rs`). E6x is per-channel: `y=0` records the
  current row as the channel's loop-start; a later `y>0` seeds an
  iteration counter and schedules a rewind back to the recorded
  start row inside the same pattern, decrementing on each visit
  until exhausted. EEx is song-level: it stalls `next_row` for `y`
  additional row-equivalents while suppressing `enter_row` on the
  repeated tick-0 (so held notes don't retrigger, tick-0 effects
  don't re-fire, and fine-volume slides don't compound). Per-tick
  effects (vibrato, volume slides, arpeggio, tone porta) keep
  animating across the delay. Six tests pin the surface:
  `e6x_pattern_loop_rewinds_to_recorded_start_row`,
  `e60_without_followup_does_not_loop`,
  `e6x_loop_state_is_per_channel`,
  `eex_pattern_delay_repeats_row_without_retriggering`,
  `eex_zero_param_is_inert`, `eex_and_e6x_compose_predictably`
  (the last covers the EE-inside-E6 composition). Spec sources:
  `docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt`
  E6 ("If yyyy=0 … specifies the loop's start point") + EE
  ("forces a small delay … all notes and effects continue during
  this delay") — STM declares its effect column as "in ProTracker
  format" per `docs/audio/trackers/stm/ScreamTracker-v1.0-stm.txt`,
  so the PT semantics carry across verbatim.

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
