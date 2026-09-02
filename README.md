# oxideav-mod

[![CI](https://github.com/OxideAV/oxideav-mod/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-mod/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-mod.svg)](https://crates.io/crates/oxideav-mod) [![docs.rs](https://docs.rs/oxideav-mod/badge.svg)](https://docs.rs/oxideav-mod) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Amiga ProTracker / SoundTracker module (MOD) codec for oxideav.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace) framework — a pure-Rust media transcoding and streaming stack. Codec, container, and filter crates are implemented from the spec (no C codec libraries linked or wrapped, no `*-sys` crates). Optional hardware-engine crates (`oxideav-videotoolbox` / `-audiotoolbox` / `-vaapi` / `-vdpau` / `-nvidia` / `-vulkan-video`) bridge to OS APIs via runtime `libloading`; pass `--no-hwaccel` (or omit the `hwaccel` feature) to opt out.

## What it does

- **Container**: reads the whole `.mod` file (ProTracker / SoundTracker) into a
  single packet. Probes the 4-byte format tag at offset 1080 and maps it to a
  channel count — see the full catalogue under *Format-tag channel map* below.
  The container probe delegates to the parser's tag classifier
  (`header::is_known_signature`), so probe acceptance and parse acceptance can
  never drift apart. Populates stream metadata (title, sample names, pattern /
  channel counts, plus a `restart_position` key when the +951 restart byte
  is live — see the restart-byte bullet under *Real-world MOD fidelity*)
  and an upper-bound duration.
- **Decoder**: parses the header, patterns, and raw signed-8-bit sample
  bodies; drives a `PlayerState` (rows → ticks, Paula periods, Protracker
  sine-table vibrato / tremolo, sample-offset, tone portamento, pattern
  loop, note-delay / note-cut, pattern-delay, full 16-finetune × 36-note
  period table); mixes samples with linear interpolation.
- **15-sample Ultimate SoundTracker**: the original Karsten-Obarski
  layout (no `M.K.` signature, 15 sample slots, pattern data at +600) is
  parsed by `header::parse_ust_header` and plays through the same
  `PlayerState`. See *Ultimate SoundTracker (15-sample)* below.
- **Decode only** — there is no MOD encoder, by design.

## Output modes

Three decoder implementations are registered with distinct codec IDs:

| Codec id     | Output shape                                                   | Use case                               |
| ------------ | -------------------------------------------------------------- | -------------------------------------- |
| `mod`        | Mixed stereo, interleaved `S16` at 44.1 kHz                    | Drop-in playback                       |
| `mod_planar` | Planar `S16P` at 44.1 kHz, one plane per MOD tracker channel   | Per-channel mixing, analysis, DAW export |
| `stm`        | Mixed stereo, interleaved `S16` at 44.1 kHz                    | Scream Tracker v1 playback             |
| `xm`         | Mixed stereo, interleaved `S16` at 44.1 kHz                    | FastTracker 2 playback                 |
| `it`         | Mixed stereo, interleaved `S16` at 44.1 kHz                    | Impulse Tracker playback               |

The mixed mode applies the Amiga hard-pan convention (channels 0 & 3 left,
1 & 2 right; the pattern repeats every 4 for >4-channel files) and a
`1/(N/2)` headroom scale so a fully-saturated 4-channel MOD stays within
-1..1.

The planar mode emits each tracker channel post-volume but pre-pan and
pre-mix — consumers get the raw per-channel signal and can apply their own
panning / mixing / effects downstream.

Both modes are driven by the same `PlayerState` tick machinery, so they
sample each channel from the same engine; only the output projection
differs.

## Usage

```toml
[dependencies]
oxideav-mod = "0.0"
```

```rust,ignore
use oxideav_codec::CodecRegistry;
use oxideav_container::ContainerRegistry;

let mut containers = ContainerRegistry::new();
let mut codecs = CodecRegistry::new();
oxideav_mod::register_containers(&mut containers);
oxideav_mod::register_codecs(&mut codecs);

// Select mixed stereo output:
//   CodecId::new(oxideav_mod::CODEC_ID_STR)          // "mod"
// Or planar per-channel output:
//   CodecId::new(oxideav_mod::CODEC_ID_PLANAR_STR)   // "mod_planar"
```

## Status

Spec-level effect coverage per
[Protracker-v1.1B-mod.txt](https://github.com/OxideAV/oxideav-workspace/tree/master/docs/audio/trackers/mod):

| Slot | Effect | Status |
| ---- | ------ | ------ |
| 0xy  | Arpeggio | implemented (period-table walk with finetune-aware semitone steps) |
| 1xx / 2xx | Portamento up / down (with last-param memory) | implemented |
| 3xy / 5xy | Tone portamento, with volume slide | implemented; E3x glissando snaps to nearest semitone |
| 4xy / 6xy | Vibrato, with volume slide | implemented (32-entry Protracker sine + ramp-down + square) |
| 7xy | Tremolo | implemented |
| 9xx | Sample offset (`param << 8`) with memory | implemented; an offset ≥ sample length plays no note (PT out-of-range quirk) |
| Axy | Volume slide | implemented |
| Bxx | Position jump | implemented (an out-of-range target — bound = the song-length byte, NOT the 128-byte order-table extent — wraps to order 0 per the staged erratum's working assumption and increments the public `oob_position_jumps` diagnostic; also clears a pending break row — see the same-row `Bxx`/`Dxy` bullet below) |
| Cxx | Set volume | implemented |
| Dxy | Pattern break (decimal `x*10 + y`) | implemented (row > 63 starts the destination at row 0; merges its row into a same-row earlier-channel `Bxx`; two breaks on one row advance the order exactly once; a break past the last order wraps the position to order 0 per FireLight §5.14 while still raising the song-over flag) |
| Fxx | Speed / BPM split (≤$1F = speed, ≥$20 = BPM; $00 = halt) | implemented (F00 raises the song-over `ended` flag) |
| E0x | Filter on/off | implemented (1-pole IIR lowpass at 11.5 kHz; LED defaults ON) |
| E1x / E2x | Fine portamento up / down (tick-0 one-shot) | implemented |
| E3x | Glissando control | implemented |
| E4x / E7x | Vibrato / tremolo waveform (sine / downward-saw / square / retrig bit) | implemented (64-step full cycle; saw is a true monotonic descent +y→-y, square starts from +y, random falls back to sine) |
| E5x | Set finetune (also re-derives period on same-row note trigger) | implemented |
| E6x | Pattern loop (per-channel start + count) | implemented (per-channel loop-start row + counter; loop point resets on every `Bxx`/`Dxy` — including same-order self-jumps — and on every pattern transition per `multimedia-cx-protracker.html` §E6x, so stale loop state cannot bleed across a jump; the rewind writes only the ROW destination variable per FireLight §4.3/§5.22, so a same-row earlier-channel `Bxx` keeps its target order and a `Dxy` keeps its order increment; with no prior `E60` the loop start defaults to row 0; an `E60`/`E61`/`E61` column loops forever) |
| E9x | Retrigger note every *x* ticks | implemented |
| EAx / EBx | Fine volume slide up / down | implemented |
| ECx | Note cut | implemented (`ECx` cuts at tick x; `EC0` cuts on tick 0 so "nothing will be heard") |
| EDx | Note delay | implemented (`ED0` = "play at tick 0" fires immediately like an ordinary note-on; only `ED1`+ defer the trigger) |
| EEx | Pattern delay | implemented (repeats suppress note retrigger + tick-0 re-fire while per-tick effects keep animating; a same-row `Bxx`/`Dxy`/`E6x` transition is deferred until the delay expires, never cancelled and never double-consumed — per MODFIL12 EE "the next line will be delayed … before it is executed" + FireLight §5.30) |
| EFx | Invert loop ("funkrepeat") | implemented (per-tick counter from the 16-entry speed table; XORs one loop byte at a time, position wraps mod loop length, resets on new sample) |
| 8xx | Set FINE Panning (FT extension) | implemented (raw 0..=255: $00 LEFT, $FF RIGHT; per-channel) |
| E8x | Set ROUGH Panning (FT extension) | implemented (nibble replicated: $0 LEFT, $F RIGHT; per-channel) |

### Format-tag channel map

The offset-1080 format tag selects the channel count. The full catalogue
is documented in
[`multimedia-cx-protracker.html`](https://github.com/OxideAV/oxideav-workspace/tree/master/docs/audio/trackers/mod)
("File Format" tag list) and corroborated by `archiveteam-amiga-module.html`:

| Tag(s) | Channels | Producer |
| ------ | -------- | -------- |
| `M.K.` / `M!K!` / `M&K!` | 4 | ProTracker (`M!K!` = >64 patterns; `M&K!` a one-off variant tag, "just a standard MOD") |
| `FLT4` / `FLT8` | 4 / 8 | Startrekker (`FLT8` paired patterns) |
| `OCTA` / `OKTA` / `CD81` | 8 | OctaMED / Oktalyzer / Falcon |
| `dCHN` (d = 1..9) | d | FastTracker (2/6/8) / TakeTracker (5/7/9); `4CHN` = explicit 4-channel |
| `xxCH` / `xxCN` (xx = 10..32) | xx | FastTracker (`CH`) / TakeTracker (`CN`) — identical layout |
| `TDZx` (x = 1..3) | x | TakeTracker 1/2/3-channel |

`xxCN` is the TakeTracker spelling of the same 10+-channel layout `xxCH`
carries, so both decode identically. Tags whose digits fall outside the
documented ranges (e.g. `0CHN`, `99CH`, `TDZ4`) are rejected by both the
probe and the parser.

**SoundTracker 2.6 / IceTracker** (`MTN\0` / `IT10` magic at offset
1464 instead of a tag at 1080) is also supported, per
`docs/audio/trackers/mod/Soundtracker-v2.6-IceTracker-st26.txt`: each
64-row track is stored once, and the 128-position pattern list names
four track indices (one per channel) per position. The parser
normalises the list to an identity order table with one synthesized
logical pattern per position, so the whole order-flow engine
(`Bxx`/`Dxy`/`E6x` rules above) runs unchanged; the unused finetune
byte is ignored, and track cells decode with the standard event
layout. A recognised offset-1080 tag always wins over a coincidental
ST2.6 magic.

Loop handling is forward-only per MOD spec — ping-pong / bidi loops are an
XM/IT/S3M-era extension and are deliberately not used here.

The header-side `Sample` struct exposes typed accessors for the loop
metadata: `Sample::is_looped()` returns `true` iff `repeat_length > 2`
(the spec sentinel — `0` *and* `2` both mean "no loop"), and
`Sample::loop_region()` returns `Some((repeat_start, repeat_length))`
when looped or `None` otherwise. The pair is the raw header view, not
the PCM-bounded clamp the mixer needs — callers reading the header for
metadata or UI use these; the mixer goes through `SampleBody` which
does its own clamp against the extracted body length.

The pattern-row `player::Note` struct exposes the matching typed
predicate set: `has_period()` / `has_sample()` / `has_effect()`
return `true` when the respective field would change channel state
(per `Protracker-effects-MODFIL12.txt` §3.4 the "no new note" /
"sample #0" / "no effect" semantics each key on one or both of the
four cell bytes being zero), and `is_empty()` returns `true` for the
canonical `0000 0000-0000` idle row from `Protracker-mod.txt`
§"Pattern data" — every field zero. `has_effect()` is the joint test
across the command nibble and the parameter byte, because command 0
with a non-zero parameter is the `0xy` arpeggio effect and command 0
with a zero parameter is the canonical "no effect" placeholder.
The internal playback engine consumes these predicates on the row-
dispatch path so the typed surface is exercised by every spec test
in `src/player.rs`.

The XM sample-header (`xm::XmSampleHeader`) ships the same shape on
its own type-byte / byte-length terms: `is_looped()` returns `true`
for `Forward` / `PingPong` loop modes (`None` returns `false`),
`loop_region_frames()` returns `Some((start, length))` as
**frame-indexed** values — the on-disk byte counts are divided by 2
when the sample is 16-bit so the result is directly comparable to a
mixer cursor — and `length_frames()` does the same byte→frame
conversion for the sample length. Same header-vs-mixer split as MOD:
the typed accessors return what the file authored, the
`SampleSource` impl clamps against the extracted PCM body. Cited in
`docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt` +0 / +4 / +8 /
+14 of the sample header.

For pitch-transposition metadata the same struct also exposes
`finetune_semitones()` — a typed view that converts the signed-byte
`finetune` field (+13 of the sample header) to a fractional-semitone
offset by dividing by 128. The 128 divisor comes straight from the
Linear-mode period formula on +96 of
`docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt`
(`Period = 10*12*16*4 - Note*16*4 - FineTune/2`), where 64 period
units = 1 semitone and 2 finetune units = 1 period unit, so 128
finetune units = 1 semitone — folding the conversion onto the
header type instead of re-deriving it at each call site. The
companion `transpose_semitones()` sums the integer-semitone
`relative_note` (+16 of the sample header) with the fractional
finetune offset and returns the total pitch shift relative to the
cell's note, the canonical surface for tuning UIs and transcription
tools.

## Ultimate SoundTracker (15-sample)

The original Karsten-Obarski *Ultimate SoundTracker* (UST) module layout
predates the `M.K.` format ID and uses **15** sample slots instead of 31,
so every field after the sample table sits at a different offset and the
fixed header is 600 bytes (vs 1084) before the pattern data. UST carries
no 4-byte signature, so it cannot be probed from a magic — the parser
entry point is selected explicitly: `header::parse_ust_header` instead of
`header::parse_header`. The doc itself recommends a caller-side switch
("either default to UST or provide a switch to toggle between UST and
ST"), so this crate exposes the UST path as an explicit parser rather
than guessing from heuristics.

`parse_ust_header` produces the *same* `ModHeader` / `Sample` /
pattern-cell shapes the 31-sample path produces, normalising the UST-only
field conventions so the existing pattern / sample-extraction / player
machinery runs unchanged:

- **Layout** — title @+0, 15 × 30-byte samples @+20, song length @+470,
  song-speed BPM byte @+471 (surfaced via `restart`; **not** a restart
  position), order table @+472, pattern data @+600. `ModVariant`
  records the origin and `ModHeader::pattern_data_offset` /
  `sample_data_offset` resolve the right offsets.
- **Repeat offset in bytes** — UST stores the loop start in *bytes*,
  unlike PT / NT / ST-2.5 which use word counts, so `parse_ust_header`
  passes it straight through without the ×2 word→sample scaling.
- **Looped samples play only the loop area** — per the doc's "Notes on
  playing repeat-samples" ("Unlike PT only the loop-area is played …
  Hence: Sample Start = Repeat Start, Sample Length = Repeat Length.
  UST modules often (!) sound screwed if repeat-samples are played
  incorrectly"), `samples::extract_samples` trims a looped UST sample's
  body to its loop region and rebases the loop to `[0, len)`, so every
  trigger path starts at the old repeat start and the pre-loop head is
  never emitted. One-shot UST samples and all 31-sample layouts keep
  the PT "play start → repeat end, then loop" behaviour.
- **No finetune** — UST has no finetune nibble (the byte at +24 is the
  high half of the volume word), so `finetune` is fixed to 0.
- **Effect translation** — UST defines only two effects, numbered
  differently from PT, translated in-place during `parse_patterns`
  (`Note::translate_ust_effect`): `1xy` arpeggio → PT `0xy`; pitchbend
  `20y` (pitch up) → PT slide-up `1·0y`, `2x0` (pitch down) → PT
  slide-down `2·0x`, `200` → no-op. A foreign command is passed through
  verbatim. Cited in
  `docs/audio/trackers/mod/Ultimate-Soundtracker-mod.txt`
  ("Conversion of UST effects to PT").

The UST song-speed byte (+471) follows a different timer convention from
the 31-sample BPM. UST has no `Fxx` tempo command — the whole-song tick
rate comes solely from the +471 byte via the Amiga Timer-IRQ formula
`freq = 716 kHz / ((240-bpm)*122)` (`Ultimate-Soundtracker-mod.txt`
§"Song speed in beats per minute"). `PlayerState::new` reads the byte off
`ModHeader::restart` for any UST-variant header and pre-computes the tick
rate (`PlayerState::ust_tick_hz_from_byte`), which `samples_per_tick`
then uses as `sample_rate / tick_hz` instead of the standard MOD's
`sample_rate * 2.5 / BPM`. At the documented UST default `0x78 = 120` BPM
the IRQ is ~48.9 Hz, so a 44.1 kHz render emits 901 samples per tick —
distinct from the standard MOD's 882-at-125-BPM, which is the playback-
speed bug that treating UST like a standard MOD produced. A byte outside
the valid `1..=239` range (which would divide by zero or a negative
period) falls back to the documented `0x78` default. The 31-sample path
is gated on `is_ust()`, so standard MODs are unaffected. The `716 kHz`
constant is read as `716 * 1000` per the doc's two readings (the closest
match to its nominal "120 BPM = 48 Hz" point).

## Real-world MOD fidelity

Spec coverage above is one half of the story; the other half is matching
the *Protracker replayer's well-known quirks* that real-world MODs target.
The following PT-vs-spec divergences are honoured by this crate (each has
a unit test in `src/player.rs`):

- **Loop boundary** — sample playback wraps at `loop_start + loop_length`,
  not at `pcm.len()`. The data past `loop_end` is the one-shot tail that
  PT discards; reading into it produced audible glitches on samples whose
  loop region is shorter than the declared length. Cited in
  `docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt` §2.2 + §2.8.
- **Loop metadata clamp** — out-of-range repeat start/length in real-world
  rips is clamped to `pcm.len()` so the mixer never reads past the buffer.
- **9xx out-of-range** — a sample-offset (`9xx`) whose target lands at or
  past the end of the sample plays *no note at all* on that channel,
  rather than silencing on the first mix call (one-shot) or wrapping the
  over-range cursor back into the loop region (looped). Cited in
  `Protracker-effects-MODFIL12.txt` 9:Set-sample-offset ("if the effect is
  out of range … NO NOTE WILL BE PLAYED!"). The `9xx` memory still latches
  so a later `900` continuation reuses the requested offset.
- **9xx without a note retriggers the playing sample** — a bare `9xx`
  (offset, no note cell) seeks the *currently-playing* sample's cursor to
  the requested offset rather than doing nothing, per
  `Protracker-effects-MODFIL12.txt` 9:Set-sample-offset ("If no sample is
  specified with the effect, but one is currently playing on the channel,
  then the sample currently playing is retriggered to offset specified") +
  `aes-modformat.html`. The seek latches the `9xx` memory, re-arms the
  anti-click ramp for the discontinuous jump, and carries the same
  out-of-range "NO NOTE" silencing quirk as the with-note path.
- **Sample-header finetune retunes every note** — a sample whose header
  finetune nibble (byte 44, signed -8..+7) is non-zero retunes *every* note
  played with that instrument, not only notes carrying an `E5x`. The period
  that fixes playback frequency is looked up "in a table based on the
  finetune setting" (`Protracker-effects-MODFIL12.txt` §3.3), so a C-2 on a
  finetune-+1 sample plays at period 425, not the cell's finetune-0 value
  428. The `EDx` note-delay fire path resolves the same way (a delayed note
  is still a note-on).
- **Tone-porta target is finetune-aware** — a `3xy` / `5xy` slide toward a
  note on a finetuned instrument glides to that note's period in the
  channel's *current* finetune row, not the finetune-0 period printed in
  the cell, because the period that fixes playback frequency is looked up
  through the finetune table (`Protracker-effects-MODFIL12.txt` §3.3). An
  A-2 cell (period 254) on a finetune-+1 voice reaches 253, one unit
  sharper than the standard table value.
- **`Axy` both-nibbles-set** — when an `Axy` (or `5xy` / `6xy`) volume
  slide has both nibbles non-zero (documented "illegal"), the volume
  slides UP by the x nibble (the y nibble is ignored), per
  `Pro-Noise-Soundtracker-rev4.txt` + `aes-modformat.html` ("If both x and
  y are non-zero, then the y value is ignored").
- **Sample swap without note** — when a sample number appears on a row
  without a note, PT loads the new sample's default volume + finetune
  immediately but defers the actual sample-PCM swap until the next
  note-on. Latching the sample index too early caused wrong-instrument
  artefacts on common idioms like setting up the next note's volume one
  row early. Cited in `Protracker-effects-MODFIL12.txt` §3.2 +
  `Pro-Noise-Soundtracker-rev4.txt:113-118`.
- **Amiga LED filter (E00 / E01)** — a 1-pole exponential lowpass at
  ~11.5 kHz is applied to the mixed output (and to each plane in the
  per-channel mode) when the LED is on. The Amiga power-on default is
  LED on, so the filter is engaged at song start. `E00` reconnects /
  `E01` disconnects, with last-channel-wins resolution per row (the
  same idiom as `Fxx`). Cutoff sourced from
  `multimedia-cx-protracker.html` E0x ("For a simple 1-pole low-pass
  filter, 11500Hz gives a fairly decent estimation").
- **Period range** — the *porta* effects (`1xx` / `2xx` / `E1x` / `E2x`)
  clamp to `[113, 856]` per `Protracker-v1.1B-mod.txt` Cmd 1/2 ("you
  cannot slide higher than B-3 / lower than C-1"). The mixer's
  `effective_period` and tone-porta storage clamp to the *extended*
  range `[108, 907]` so that finetune ±8 extremes (`PERIOD_TABLE[7][35]
  = 108`, `PERIOD_TABLE[8][0] = 907`) play at the right pitch instead
  of being snapped back to the standard limits.
- **Vibrato / tremolo waveform LFO** — the `E4x` / `E7x` waveform select
  drives a **64-step full-cycle** LFO via the shared `lfo_waveform` helper.
  Shape 1 is a *downwards saw* — a monotonic descent from `+y` at the
  cycle start (just after a retrigger) to `-y` at the end — matching
  `multimedia-cx-protracker.html` §4xy ("Waveform 1 is a downwards saw
  wave … a full cycle of 64 steps") and the shape numbering in
  `Protracker-2.3A-misc-info.txt` lines 387/390. Shape 2 is a square
  "starting from +y" (`+y` first half-cycle, `-y` second). Shape 3
  (random) has no documented PT PRNG and reuses the sine table. An earlier
  implementation generated the saw from a `|pos|`-mirrored magnitude that
  *rose* then jumped rather than descending, which mis-shaped the audible
  pitch/volume sweep of every saw-waveform vibrato/tremolo.
- **Vibrato sign convention** — we follow `FireLight §5.5` pseudocode:
  the sine-table value is *added* to the period (== "AMIGA frequency"
  in the doc) for `vibrato_pos >= 0` and subtracted for `< 0`. Adding
  to the period lowers the audible pitch, so the first half-cycle of a
  fresh vibrato dips below the base note. This is the canonical PT
  interpretation.
- **`Fxx` speed/BPM split** — `< 0x20` sets ticks/division (speed),
  `>= 0x20` sets BPM, matching `Protracker-v1.1B-mod.txt` Cmd F and
  the convention noted in `Pro-Noise-Soundtracker-rev4.txt:362-365`.
  `0x1F` is the largest speed value, `0x20` (= 32) is the smallest
  BPM value. A speed-range `Fxx` and a BPM-range `Fxx` on different
  channels of the same row **both** stick (the doc: "a SET SPEED
  command does NOT override a SET BPM command, even if these effects
  use the same effect nr ($F)").
- **`F00` halts playback** — a `Set speed` with parameter `0x00` stops
  the song. `Protracker-effects-MODFIL12.txt` F:Set-speed states "A
  value of xxxxyyyy=0 should technically cause playback to stop" and
  the in-doc annotation pins the replayer reality: "++ F00 stops the
  playback on ProTracker too. ++". The row carrying `F00` is still
  entered (its notes and tick-0 effects apply) and then the player's
  song-over `ended` flag is raised so both `render` and
  `render_per_channel` break out — the same termination path used when
  the order list runs off its end. `F00` is a halt, not a tempo value,
  so it leaves `speed` / `bpm` untouched and never collides with a
  live speed/BPM dual-set on the same row. Previously `F00` was
  silently ignored, so a module that ended on `F00` ran past its
  intended stop into trailing pattern data.
- **`Bxx` out-of-range wraps to order 0** — a position-jump `Bxx` whose
  target order is at or past the song length does **not** end the song;
  ProTracker wraps it back to order 0 and keeps playing. Previously an
  out-of-range `Bxx` fell into the same end-of-song path as a natural
  run-off the order list, so a module that used a high `Bxx` as its
  loop-back stopped instead of restarting. The natural run-off the end of
  the order list (and a `Dxy` pattern-break that overflows past the last
  order) still raise the song-over `ended` flag — only the explicit
  out-of-range `Bxx` target wraps. Cited in
  `docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt` B:Position-Jump
  ("If you do Bxx where xx is order_num or more, then it simply jumps to
  order 0. And yes, I have tested this in ProTracker.").
- **Restart byte (offset 951) steers the song-end wrap** — the header
  byte at +951 is a song-restart position in the NoiseTracker /
  FastTracker lineage and a filler everywhere else (`mod-spec-eblong.txt`
  +951 "Noisetracker uses this byte for restart, ProTracker doesn't";
  `Pro-Noise-Soundtracker-rev4.txt` header table; `multimedia-cx-
  protracker.html` header list "Traditionally $78 in SoundTracker …
  ProTracker uses $7F. FastTracker uses it as a restart point").
  `header::ModHeader::restart_position()` exposes the typed reading —
  `None` for the `$7F`/`$78` filler conventions, for values at/past the
  song length, and for the UST (+471 is a BPM) and ST2.6 (no such byte)
  layouts; `Some(order)` otherwise — and the player wraps the natural
  run-off and the `Dxy`-overflow paths to that order instead of order 0
  when it is live. The song-over `ended` flag still rises on every wrap,
  and the out-of-range-`Bxx` path is untouched (its order-0 wrap is the
  erratum's working assumption, pinned independently of the restart
  byte).
- **Startrekker `FLT8` paired patterns** — `FLT8` files keep the plain
  4-channel 0x400-byte pattern layout on disk and pair two consecutive
  stored patterns into one logical 8-channel pattern: stored `2k`
  carries channels 1-4, stored `2k+1` carries channels 5-8, and the
  order table holds the even stored indices ("Divide all patterns in
  the orderlist by 2"). `parse_header` halves the order entries up
  front and `parse_patterns` resolves the paired layout, so the
  player walks logical 8-channel patterns; previously the file was
  misread as flat interleaved 8-channel rows (scrambling every cell
  past channel 4 and mis-locating the sample bodies). `8CHN` / `OCTA`
  / `CD81` are unaffected — only the `FLT8` signature pairs. Cited in
  `docs/audio/trackers/mod/Startrekker-mod.txt` (format-author
  description: "just take two 4 channel patterns together! So pattern
  0 and 1 is one 8 channel pattern").
- **`Bxx` + `Dxy` on the same row (two-variable jump model)** —
  ProTracker keeps two independent jump-destination variables, a target
  *order* and a target *row in the destination pattern*, and processes
  channels left to right on tick 0
  (`docs/audio/trackers/mod/mod-position-jump-pattern-break.md`). `Bxx`
  writes the order (and clears the row to 0 — a bare `Bxx` "also performs
  a pattern-break", `mod-spec-eblong.txt` Cmd B); `Dxy` writes the row.
  So `Bxx` **left** of `Dxy` combines into "row `x*10+y` of the pattern
  at order `xx`" (the song-restart-at-position-and-row idiom), while
  `Bxx` **right** of `Dxy` discards the earlier channel's break row and
  lands on row 0 of order `xx`. A break row decoding past the 64-row
  pattern (`D64`..`D99`) starts the destination at row 0. Both
  directions, the D/B/D triple, the out-of-range-`Bxx`-plus-`Dxy`
  composition and the backward `B00`+`D05` loop idiom are pinned by
  directed tests.
- **`E6x` / `Dxy` same-row resolution** — both effects write to the
  same `pending_jump`; per the `Pro-Noise-Soundtracker-rev4.txt:375-377`
  channel-priority rule, the higher-numbered channel wins. The
  regression test pins this down so a future refactor doesn't quietly
  flip the ordering.
- **`8xx` / `E8x` per-channel pan vs. Amiga LRRL default** — the
  FastTracker pan extensions live alongside the player's global
  `pan_separation` knob: each channel carries a `pan: u8` (0 = LEFT,
  128 = centre, 255 = RIGHT) initialised to the classic Amiga LRRL
  layout (channels 0 & 3 → 0, 1 & 2 → 255, repeating every 4) so a
  MOD with no pan commands renders with the hard-LRRL panning.
  `8xx` overwrites the full byte; `E8x` replicates the nibble across
  both halves (`E80` → 0x00, `E8F` → 0xFF) — matching the endpoint
  semantics in `Protracker-effects-MODFIL12.txt` lines 1201-1207
  (8xx) and 1503-1505 (E8x), and the monotonic 16-step ramp echoed in
  `multimedia-cx-protracker.html` E8x. The per-channel gain helper
  `pan_gains(p, s)` collapses to the prior hard-LRRL formula at the
  endpoints (so the trace-reference-impl-calibrated headroom divisor still
  holds bit-for-bit), and splits a centred channel evenly regardless
  of `s` — so a MOD that pans a lead voice to centre stays centred
  even at `pan_separation = 1.0`.

## FastTracker 2 (.xm) playback coverage

`oxideav-mod` also drives an XM (FastTracker 2 Extended Module) playback
engine (`xm_player::XmPlayerState`), now wired through the codec
registry as codec id `"xm"`. The decoder accepts the whole-file packet
emitted by the `xm` container, parses the header / patterns /
instruments / delta-encoded sample bodies, and emits interleaved S16
stereo PCM at 44.1 kHz — the same output shape as the `"mod"` and
`"stm"` codec ids, so a generic player can swap between MOD, STM, and
XM without reshaping its audio pipeline. The engine drives audio for
every FT2 standard effect listed in
[`docs/audio/trackers/xm/FT2-effects-list.txt`](https://github.com/OxideAV/oxideav-workspace/tree/master/docs/audio/trackers/xm)
plus the eleven volume-column kinds. The "captured but not honoured"
items are all closed:

- **E4x / E7x vibrato + tremolo waveform shapes** — the LFO shape set by
  E4x (vibrato) / E7x (tremolo) is now honoured, not just the bit-2
  retrigger flag. `waveform_lfo` returns the per-cycle value on the same
  ±127 scale as the sine table: shape 0 sine, 1 downward saw, 2 square
  ("starting from +y"), 3 random (deterministic sine fallback — no PRNG
  is documented). Replaces the prior hardcoded-sine LFO for both effects.
  Cited in `docs/audio/trackers/mod/multimedia-cx-protracker.html` §4xy
  (the 64-step full-cycle shape catalogue) +
  `docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt` E4/E7 +
  `docs/audio/trackers/mod/Protracker-2.3A-misc-info.txt` lines 387/390
  (the "0 sine / 1 ramp-down / 2 square / 3 random" numbering, shared by
  the FT2 E4x/E7x effects).
- **E3x glissando control** — when on, tone-porta (3xy / 5xy / vol-col
  Mx) snaps the period to the nearest semitone after each tick's linear
  slide step. Works in both Linear and Amiga pitch tables; the Amiga
  snap walks `XmPitch::PERIOD_TAB_PUB` across the 10-octave span and
  picks the nearest table entry by absolute period error. Cited in
  `docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt` line 222.
- **Lxy set envelope position** — moves the volume-envelope tick cursor
  to `param`. The segment index is reset to 0 so the next
  `tick_envelope` call re-aligns from the start of the segment chain
  (which is monotonic without the loop bit, so re-alignment is exact).
  Pan envelope is left untouched, matching the FT2 reading. Cited in
  `docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt` line 226 and
  `docs/audio/trackers/xm/multimedia-cx-fasttracker-2.html` §2.1.20.
- **Rxy multi-retrig per-nibble memory** — the two nibbles of `Rxy`
  carry **independent** memories per
  `docs/audio/trackers/xm/multimedia-cx-fasttracker-2.html` §2.1.22:
  `y = 0` reuses the last nonzero retrig speed seen on the channel,
  and `x = 0` reuses the last nonzero volume modifier seen on the
  channel — the wiki snapshot explicitly flags the FT2 manual's
  "None" wording for `x = 0` as wrongly documented (the actual
  behaviour is "reuse last nonzero modifier", NOT "leave volume
  unchanged"). The per-tick resolver in `advance_tick` now consults
  separate `multi_retrig_x_mem` / `multi_retrig_y_mem` slots that
  are latched at row entry only on a nonzero nibble; when a memory
  slot has never been seeded, the unseeded fallback for `x` walks
  through the existing modifier-table entry 0 ("leave unchanged")
  and for `y` skips the retrig fire entirely. Five unit tests in
  `xm_player::tests` pin the surface: `y = 0` reuses last nonzero
  speed, `x = 0` reuses last nonzero modifier, `x = 0` with no
  prior nonzero `x` leaves the volume at its trigger value, `y = 0`
  with no prior nonzero `y` does not retrig, and the two memories
  evolve independently across rows.
- **Fine-slide last-non-zero memory** — `E1x` / `E2x` (fine porta up /
  down), `EAx` / `EBx` (fine volume slide up / down), and `X1x` / `X2x`
  (extra-fine porta up / down) are all marked `(*)` in
  `docs/audio/trackers/xm/FastTracker-2-v2.04-xm.txt` (line 233: "If the
  command byte is zero, the last nonzero byte for the command should be
  used"). Each of the six now carries an independent per-channel memory
  slot (up and down do not share a pool), latched on the last non-zero
  amount and reused when the cell's nibble is zero. Ten unit tests pin
  the reuse and up/down slot independence.
- **Note-delay (`EDx`) trigger consistency** — a deferred note is still a
  note-on, so the delayed fire now applies the same vibrato / tremolo /
  autovibrato phase resets as a tick-0 trigger, gated on the waveform
  "don't retrigger" flag (bit 2), and resets the `Txy` tremor counter
  and autovibrato sweep counter. Previously the delayed fire reset the
  LFO phases unconditionally and skipped the counter resets. (The `Rxy`
  counter is *not* reset by the fire itself — see the retrig-counter
  bullet below.)
- **`E90` retrigs exactly once, on tick 0** — per
  `docs/audio/trackers/xm/multimedia-cx-fasttracker-2.html` §2.1.15.10
  ("If the parameter x is 0, this effect retrigs the sample only once -
  on tick 0"), a bare `E90` on a row without a note restarts the
  playing sample from the top at tick 0. Previously `E90` was a no-op,
  which is only equivalent when the row also carries a note. The
  non-zero leg is unchanged (`E9x` retrigs on every `x`-th tick,
  *except* tick 0), and the tick-0 retrig honours the same section's
  carve-out that only a **non**-tick-0 `E9x` retrig resets the `Rxy`
  counter.
- **`Rxy` counter reset cases are exhaustive** — §2.1.22 lists exactly
  three resets: before the song starts, a row whose channel carries an
  **instrument number** ("doesn't matter if there's a note in the note
  column"), and a non-tick-0 `E9x` retrig. A bare note with no
  instrument byte is none of those, so its trigger now leaves the
  counter running (previously every note trigger reset it). Five
  directed tests pin the matrix: `E90` fires once on tick 0 and never
  later, the tick-0 fire preserves the counter, an `E91` tick-1 fire
  resets it, a note-without-instrument preserves it, and an
  instrument-only row resets it.
- **`Kxy` key-off equivalence to note 97** — `Kxy` ("Key off. Same as
  note number 97" per `multimedia-cx-fasttracker-2.html`) now silences a
  voice on an envelope-less instrument immediately, exactly like a
  note-97 cell, instead of only releasing the key.
- **Volume-column ordering is pinned to the v2.04 sentence** —
  `FastTracker-2-v2.04-xm.txt`: "The volume column is interpreted
  before the standard effects, so some standard effects may override
  volume column effects." The row-entry pipeline now runs
  instrument-default load → volume-column value writes → standard
  effects, so a vol-col `SetVolume` on a note+instrument cell survives
  the trigger's sample-default load (previously the default clobbered
  it) while a same-cell `Cxx` still overrides the volume column.
  Directed tests pin both directions.
- **Broken `sample_header_size` dwords are ignored** — the instrument
  extended header's sample-header-size field is surfaced for metadata
  fidelity but never steers the parse: sample headers stride at the
  fixed on-disk 40 bytes. Per `multimedia-cx-fasttracker-2.html` §File
  Format the field "is completely ignored by Fast Tracker 2" and
  modules in the wild carry completely broken values in it — honouring
  it either hard-rejected the file (value < 40) or mis-located every
  subsequent sample header and PCM body (value > 40). A regression
  test drives both broken shapes end-to-end through PCM extraction.

The instrument-level autovibrato (`vibrato_type` byte) now honours the
type byte's waveform shape and the +4 "don't retrigger" flag, sharing
the same `waveform_lfo` helper as the E4x / E7x effects: `0 = Sine`,
`1 = Ramp down`, `2 = Square` (value 3 is undefined in FT2 and falls
back to the deterministic sine, per `xm-instrument-autovibrato.md`'s
"FT2 documents only three waveforms" finding). With bit 2 set, the LFO
phase persists across note triggers; the sweep-in counter still
restarts on every trigger because the sweep is a separate ramp-in
envelope rather than a phase register. Numeric mapping + the +4 flag
sourced from the in-tree clean-room note
[`docs/audio/trackers/xm/xm-instrument-autovibrato.md`](https://github.com/OxideAV/oxideav-workspace/tree/master/docs/audio/trackers/xm/xm-instrument-autovibrato.md)
(which cites `FastTracker-2.08-manual.doc` §3.15.4 / §4.2.1 / §4.2.6
and the `FastTracker-2-v2.04-xm.txt` field table at +235..+238).

### Shared-mixer loop boundary (STM + XM)

STM and XM samples drive the shared `mixer::MixerVoice` core rather than
MOD's bespoke mix loop, but they observe the **same** loop-boundary rule:
a forward (or ping-pong) loop wraps at `loop_end`, never at the PCM
buffer end. XM samples in particular very commonly declare a loop region
shorter than the sample body (`loop_end < len`) — the bytes past
`loop_end` are a one-shot tail the loop must discard, exactly as PT
discards a MOD sample's tail (`Protracker-effects-MODFIL12.txt` §2.2 +
§2.8). The voice's playback cursor wraps on `loop_end`, and the
linear-interpolation partner folds back to `loop_start` when `i + 1`
reaches `loop_end`, so the boundary frame never interpolates against a
discarded tail sample. Ping-pong loops reflect at the same `loop_end`
edge. Unit tests in `mixer::tests` poison the tail with an out-of-band
sentinel and assert no tail frame is ever emitted for either loop mode.

## Scream Tracker v1 (.stm) playback coverage

`oxideav-mod` also drives a Scream Tracker v1 (`.stm`) playback engine
(`stm_player::StmPlayerState`), now wired through the codec registry as
codec id `"stm"`. The decoder accepts the whole-file packet emitted by
the `stm` container, parses the header / patterns / sample bodies, and
emits interleaved S16 stereo PCM at 44.1 kHz — the same output shape as
the `"mod"` codec id, so a generic player can swap between MOD and STM
without reshaping its audio pipeline.

STM declares its effects as "in ProTracker format" per
[`docs/audio/trackers/stm/ScreamTracker-v1.0-stm.txt`](https://github.com/OxideAV/oxideav-workspace/tree/master/docs/audio/trackers/stm),
so the implemented columns track the ProTracker semantics documented under
`docs/audio/trackers/mod/`:

| Slot | Effect | Status |
| ---- | ------ | ------ |
| 0xy  | Arpeggio | implemented (note / note+x / note+y half-steps cycling on `counter mod 3`; pure additive offset, inert at param 0) |
| 1xy / 2xy | Portamento up / down | implemented (semitone-space, shared last-param memory) |
| 3xy / 5xy | Tone portamento, with volume slide | implemented |
| 4xy / 6xy | Vibrato, with volume slide | implemented (shared waveform LFO; sine default, shape set by E4x) |
| 7xy | Tremolo | implemented (independent `trem_pos` register; volume offset clamped to `[0, 64]`; per-nibble memory; shape set by E7x) |
| 9xx | Sample offset (`param << 8`) with memory | implemented; an offset ≥ sample length plays no note (PT out-of-range quirk); `900` reuses the latched `xx` per the canonical PT memory reading |
| Axy | Volume slide | implemented |
| Bxy | Position jump | implemented |
| Cxx | Set volume | implemented |
| Dxy | Pattern break (FT2-style decimal landing row) | implemented |
| Fxx | Speed / tempo split (≤$1F = speed, ≥$20 = tempo) | implemented |
| E1x / E2x | Fine portamento up / down | implemented |
| E3x | Glissando control (snap tone-porta to nearest semitone) | implemented |
| E4x / E7x | Vibrato / tremolo waveform (sine / ramp-down / square / +4 no-retrigger bit) | implemented (shared `waveform_lfo` shape catalogue; the +4 bit keeps the LFO phase across note-ons, note-delay triggers, and E9x retrigs) |
| E6x | Pattern loop (per-channel start + count) | implemented |
| E9x | Retrigger note every *x* ticks | implemented (cursor rewind + vibrato/tremolo phase reset on each retrig; row-scoped period; gated on `voice.active` so an idle channel isn't resurrected) |
| EAx / EBx | Fine volume slide up / down | implemented |
| ECx | Note cut | implemented |
| EDx | Note delay | implemented |
| EEx | Pattern delay (row repeats without retrigger) | implemented |

Pitch effects operate in a fractional **semitone** domain (STM has no
Amiga periods — pitch is derived from each instrument's C3 Hz), so the
arpeggio offset is a direct semitone addition on top of the live pitch.
The 0xy walk follows the canonical algorithm in
`docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt` 0:Arpeggio
("if (counter mod 3) = 0/1/2 then play note / note+x / note+y").

Tremolo (7xy) follows the same MOD/PT contract: a sine LFO modulates the
output volume around the current `ch.volume` baseline (Cxx / Axy /
EAx / EBx set the baseline; tremolo does not write back to it), the
result is clamped to the spec's `[0, 64]` range before the global-volume
scale, and the per-nibble memory (`trem_speed` from a non-zero `x`,
`trem_depth` from a non-zero `y`) is independent of vibrato's so an
LFO on volume can stack with an LFO on pitch on the same channel
without phase-bleed. Cited in
`docs/audio/trackers/mod/Protracker-effects-MODFIL12.txt` 7:Tremolo
("If either xxxx or yyyy are 0, then values from the most recent prior
tremolo will be used") and `multimedia-cx-protracker.html` 7xy ("Like
vibrato, except we modify the output volume … clamped to 0 <= vol <=
64").

Pattern loop (`E6x`) and pattern delay (`EEx`) round out the structural
side of the PT extended-command set. `E60` records the current row as
the channel's loop-start; a later `E6y` (y > 0) seeds an iteration
counter and schedules a rewind back to the recorded start row inside
the *same* pattern, decrementing on each visit until exhausted (per
`Protracker-effects-MODFIL12.txt` E6 — "If yyyy=0 … specifies the
loop's start point. Otherwise, it specifies the number of times to
play this line and the preceding lines from the start point"). Loop
state is per-channel, so two voices can drive independent loops
without trampling each other's counters. `EEx` stalls `next_row` by
`y` additional row-equivalents while a `in_pattern_delay_repeat` flag
suppresses `enter_row` on the looped tick-0 — so held notes don't
retrigger, tick-0 effects don't re-fire, and fine-volume slides don't
compound across the delay (per `Protracker-effects-MODFIL12.txt` EE —
"All notes and effects continue during this delay"). Per-tick effects
(vibrato, volume slides, arpeggio, tone porta) keep animating
underneath, which is what gives EE its characteristic
held-note-with-LFO-still-running texture on real-world rips.

## Impulse Tracker (.it) playback coverage

`oxideav-mod` also plays Impulse Tracker modules (`it_player::ItPlayerState`),
wired through the codec registry as codec id `"it"` (and reachable
directly via `decoder::make_it_decoder`). The `it` container probes the
`IMPM` magic at offset 0, surfaces title / song message / tracker
version / sample names / a structural `extra_info` line and a duration
estimate, and hands the whole file to the decoder as one packet; the
decoder emits interleaved S16 stereo at 44.1 kHz — the same output
shape as `"mod"`, `"stm"` and `"xm"`. Structural callers use
`it::parse_module` (or `it::parse_header` / `it::parse_instruments` /
`it::parse_samples` / `it::parse_patterns` individually).

Every layout, table and formula comes from
[`docs/audio/trackers/it/ImpulseTracker-it.txt`](https://github.com/OxideAV/oxideav-workspace/tree/master/docs/audio/trackers/it)
(the tracker author's format text; the `.html` mirror is the same
document); the effect letters IT shares with Scream Tracker 3 are read
from
[`docs/audio/trackers/s3m/ScreamTracker-v3.20-effects.txt`](https://github.com/OxideAV/oxideav-workspace/tree/master/docs/audio/trackers/s3m).

### Format surface

| Block | Coverage |
| ----- | -------- |
| Header | counts, `Cwt` / `Cmwt`, flags (stereo, instruments-vs-samples, linear-vs-Amiga slides, old effects, compatible `Gxx`, MIDI bits), `Special` (message), global / mix volume, speed / tempo, pan separation, 64 channel pans (incl. surround `100` and the `+128` disabled bit) + volumes, order list (`254` skip / `255` end), the three offset tables, song message (`0Dh` → `\n`) |
| Samples | 8/16-bit, signed / unsigned (`Cvt` bit 0), Intel / Motorola order, delta storage, global + default volume, default pan (bit 7 = use), `C5Speed`, normal + sustain loops (forward / ping-pong) validated against the decoded body, sample vibrato (speed / depth / rate / waveform), truncation tolerance. **Compressed bodies (flag bit 3) are recognised and left silent** — the staged text names the flag but not the scheme. Stereo bodies (flag bit 2, "not supported yet" in the text) decode their first `Length` frames. |
| Instruments | old 1.x layout (flag byte, node-number loop points, 200-byte per-tick table + node pairs, fadeout 0–64/512 rescaled to the 2.x count) and 2.x layout (NNA / DCT / DCA, fadeout, pitch-pan separation + centre, global volume, default pan, random volume / pan, filter + MIDI bytes, 120-entry note→sample keymap, volume / panning / pitch envelopes with 25 `(y, tick)` nodes, loop + sustain-loop points) |
| Patterns | channel-marker / mask-byte walk with the per-channel previous-mask and last-note / instrument / volume / command memories; `Rows` clamped to `1..=200`; truncated data leaves the rest empty; offset `0` = empty 64-row pattern |

### Player semantics

- **Volume**: `FV = Vol*SV*CV*GV / 2^18` (sample mode) and
  `Vol*SV*IV*CV*GV*VEV*NFC / 2^41` (instrument mode) exactly as
  §"Mathematics" states, with `NFC` counting down from 1024 by the
  instrument fadeout; output = `FV/128 × MV/128`. Fade starts on
  envelope end, on note-off without a volume envelope (or with a looping
  one), on NNA fade and on the "other note values = note fade" cells.
- **Pitch**: `Hz = C5Speed × PitchTable[note] / 65536` (the 16.16 table
  from §"Internal Tables"); linear slides multiply by `2^(n/768)`, Amiga
  slides add `n` to the ST3 period `14317056 / Hz`. `Exx`/`Fxx` = 4·xx
  per tick, `EFx`/`FFx` = 4·x once, `EEx`/`FEx` = x once; `E`/`F`/`G`
  share memory under compatible `Gxx`; `Gxx` with an instrument under
  that flag retunes by `NewC5/OldC5` and retriggers envelopes.
- **Envelopes**: linear node interpolation, sustain loop while the note
  is held, normal loop otherwise, end → hold + fade. Pitch envelope units
  are **half a semitone** (32 linear-slide units), the panning envelope
  scales by the room toward the nearer edge
  (`pan + env × (32 − |pan − 32|) / 32`), `S7x` toggles each envelope
  per voice.
- **NNA virtual channels** (§"General Info"): a new note in instrument
  mode applies the old voice's NNA (cut reuses the slot; continue /
  note-off / fade push it to the background), runs `DCT` (note / sample /
  instrument) with `DCA` (cut / off / fade) over the host channel's
  voices, then allocates a free voice — the pool grows to
  `IT_MAX_VOICES` (256), beyond which the quietest background voice is
  stolen. `S70`–`S72` act on the background voices, `S73`–`S76` set the
  current note's NNA.
- **Effects**: `Axx` speed (0 ignored), `Bxx` / `Cxx` (hex row) with
  the row-from-break / order-from-jump merge, `Dxy` in all four forms
  (`Dx0`/`D0x` per tick with the `x = F` "straight away" S3M compat,
  `DxF`/`DFx` fine), `Exx`/`Fxx`/`Gxx`, `Hxy` (depth ×4, table position
  advanced every tick incl. tick 0) / `Uxy` (depth ×1) with the shared
  vibrato memory, `Ixy` tremor (counter runs every tick, across rows),
  `Jxy`, `Kxy`, `Lxy`, `Mxx` / `Nxy` channel volume, `Oxx` (+ `SAy` high
  offset; past-the-end ignored, or "from the end" under old effects),
  `Pxy` (x = left, y = right; fine forms), `Qxy` (S3M volume table;
  the retrig counter persists across rows), `Rxy` tremolo
  (`table × depth / 32`), every `Sxx` (`S1x` glissando, `S2x` finetune
  via the ST3 C-speed table, `S3x`/`S4x`/`S5x` waveforms, `S6x` tick
  delay, `S7x`, `S8x` pan, `S9x` surround, `SAy`, `SBx` loop, `SCx` cut
  (`SC0` immediately), `SDx` delay, `SEx` row delay; `S00` reuses the
  last `Sxx`), `Txx` (`T0x`/`T1x` tempo slides per tick), `Vxx` /
  `Wxy` global volume, `Xxx` pan (0–255 → 0–64), `Yxy` panbrello
  (4× table, random-shape delay), `Zxx` MIDI macro = documented no-op
  (the text pins no macro semantics). Volume column: volume, pan
  (`128–192`), fine / normal volume slides with their own shared memory,
  pitch slides (`x×4` into the `E`/`F` memory), portamento via the
  `SlideTable` into the `G` memory, vibrato depth into the `H` memory.
- **Old Effects** flag: vibrato / tremolo skip the row tick and vibrato
  depth doubles; `Oxx` past the end plays from the end. Muted channels
  process effects but stay silent. Sample vibrato follows the
  `Add AL, Rate / AdC AH, 0` sweep with the 256-entry fine tables.

### Black-box oracle gates

`tests/it_oracle_compare.rs` renders 18 synthetic fixtures (built with
the crate's own writer) through this engine and through an installed
command-line player — `openmpt123 --render` or `xmp -o`, invoked as
opaque binaries whose PCM alone is read — and asserts per-row dominant
pitch (cents), normalised RMS profiles, stereo balance and length. All
18 pass locally against `openmpt123`: pitch table and slides within
1 cent, vibrato depth + phase tick-exact, pitch-envelope units,
pan-envelope scaling, `Pxy` direction, retrig continuity, NNA / DCT
behaviour, sample vibrato sweep, `Sxx` memory and the absolute mix level
(within 2%) were each *fixed* by these gates during round 455. The
residual RMS deltas (2–7%, growing toward silence) are the oracle's
half-tick volume ramp, not an engine difference. The tests print a
clean SKIP when no oracle binary is on `PATH`, so CI stays green.

### Known gaps (docs asks filed by the round parent)

- IT 2.14/2.15 **compressed samples**: the staged text has only the flag
  bit; bodies stay silent until the decompression scheme is staged.
- **Stereo sample layout**: the text says "not supported yet".
- **Resonant filter** (`IFC` / `IFR`, filter envelopes): fields parsed,
  no filter is applied — no cutoff / resonance formula is staged.
- **Envelope carry flags**, `S9x` values other than 0/1, `Zxx` macros
  and MIDI configuration: no staged semantics; parsed / no-op.
- The staged `LinearSlideUpTable` / `LinearSlideDownTable` listings
  carry three transcription typos (entries 35, 243 and the duplicated
  `65359` at fine-down index 7); the engine uses the `2^(n/768)` formula
  the text gives instead of the tables.

## Reference-compare conformance gates

Two integration harnesses drive this crate's engines side-by-side with a
runtime-loaded black-box reference render binary (dlopen'd via
`libloading`; only its published C entry-points are called, and it is
used strictly as an opaque behaviour oracle whose output is compared
against ours — the tests skip cleanly when the dylib is absent):

- **`tests/tracker_reference_compare.rs`** (MOD) — per-chunk
  `(order, pattern, row, speed, tempo)` traces plus PCM RMS / scaled-diff
  analysis over real-world MOD fixtures, with gain-calibration pins.
- **`tests/stm_xm_reference_compare.rs`** (STM + XM, round 451) —
  synthetic fixtures whose order flow exercises pattern break (`Dxy`
  decimal row), order jump (`Bxx`), and a mid-song speed change (`Fxx`):
  - the **XM** gate asserts `(order, row, speed)` **lockstep at all 240
    probes** of a 30-second trace (measured: 0 mismatches);
  - the **STM** gate asserts the reference *accepts* the staged header
    layout (it demonstrably validates the file-type byte) and that our
    engine traverses the whole 3-order flow audibly. The reference's own
    STM *playback* is recorded as an observation, not asserted: it
    renders every variant we tried (fixed cells with each documented
    marker byte, packed marker reading, several version/tempo stampings)
    as ~0.12 s of silence with the order cursor racing to the end, and
    with no known-good real-world STM available and no staged
    effect/tempo semantics to arbitrate
    (`docs/audio/trackers/stm/stm-effect-semantics-gap.md`), the
    divergence is unattributable.

## Fuzz harness

A `cargo-fuzz` harness under `fuzz/` drives the four parser
pipelines (MOD / STM / XM / IT) against arbitrary attacker-controlled
bytes and asserts the call always returns rather than panicking /
aborting / OOMing.

| Target | Driven pipeline |
| ------ | --------------- |
| `mod_decode` | `header::parse_header` → `player::parse_patterns` → `samples::extract_samples` → `player::PlayerState::new` → 2048-frame `render` |
| `stm_decode` | `stm::parse_header` → `stm::parse_patterns` → `stm::extract_samples` → `stm_player::StmPlayerState::new` → 2048-frame `render` |
| `xm_decode`  | `xm::parse_header` → `xm::parse_patterns` → `xm::parse_instruments` → `xm::extract_sample_bodies` → `xm_player::XmPlayerState::new` → 2048-frame `render` |
| `it_decode`  | `it::parse_module` (header + message + instruments + samples + patterns) → `it_player::ItPlayerState::new` → 2048-frame `render` |

Run with `cargo +nightly fuzz run <target>` from `crates/oxideav-mod/`.
Each target has a minimal valid-header seed under
`fuzz/corpus/<target>/minimal.{mod,stm,xm,it}` (IT also seeds an
instrument-mode and an effect-heavy module) so libfuzzer's coverage
hill-climb starts from a parser-accepting input. A previously-found
`xm::parse_patterns` slice-index panic (a hostile `header_length`
pushing the packed-data slice's start past EOF) is fixed and pinned by
a regression test in `src/xm.rs`. A previously-found `header::parse_header`
integer-overflow panic (an order table whose maximum entry is `0xFF`
overflowing the `1 + max` "pattern count" derivation in the `u8` domain)
is fixed by saturating the `+1` and pinned by a full-MOD-pipeline
hostile-input regression in `src/player.rs`
(`hostile_inputs_never_panic_the_decode_pipeline`).

## License

MIT — see [LICENSE](LICENSE).
