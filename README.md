# oxideav-mod

Amiga ProTracker / SoundTracker module (MOD) codec for oxideav.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace) framework — a
100% pure Rust media transcoding and streaming stack. No C libraries, no FFI
wrappers, no `*-sys` crates.

## What it does

- **Container**: reads the whole `.mod` file (ProTracker / SoundTracker) into a
  single packet. Probes the 4-byte signature at offset 1080 (`M.K.`, `M!K!`,
  `4CHN`/`6CHN`/`8CHN`, `FLT4`/`FLT8`, `OCTA`, `CD81`, `xxCH` for 10..=32
  channels). Populates stream metadata (title, sample names, pattern /
  channel counts) and an upper-bound duration.
- **Decoder**: parses the header, patterns, and raw signed-8-bit sample
  bodies; drives a `PlayerState` (rows → ticks, Paula periods, Protracker
  sine-table vibrato / tremolo, sample-offset, tone portamento, pattern
  loop, note-delay / note-cut, pattern-delay, full 16-finetune × 36-note
  period table); mixes samples with linear interpolation.
- **Decode only** — there is no MOD encoder, by design.

## Output modes

Two decoder implementations are registered with distinct codec IDs:

| Codec id     | Output shape                                                   | Use case                               |
| ------------ | -------------------------------------------------------------- | -------------------------------------- |
| `mod`        | Mixed stereo, interleaved `S16` at 44.1 kHz                    | Drop-in playback                       |
| `mod_planar` | Planar `S16P` at 44.1 kHz, one plane per MOD tracker channel   | Per-channel mixing, analysis, DAW export |

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
| 9xx | Sample offset (`param << 8`) with memory | implemented |
| Axy | Volume slide | implemented |
| Bxx | Position jump | implemented |
| Cxx | Set volume | implemented |
| Dxy | Pattern break (decimal `x*10 + y`) | implemented |
| Fxx | Speed / BPM split (≤$1F = speed, ≥$20 = BPM) | implemented |
| E0x | Filter on/off | accepted, no-op (hardware only) |
| E1x / E2x | Fine portamento up / down (tick-0 one-shot) | implemented |
| E3x | Glissando control | implemented |
| E4x / E7x | Vibrato / tremolo waveform (sine / ramp-down / square / retrig bit) | implemented |
| E5x | Set finetune (also re-derives period on same-row note trigger) | implemented |
| E6x | Pattern loop (per-channel start + count) | implemented |
| E9x | Retrigger note every *x* ticks | implemented |
| EAx / EBx | Fine volume slide up / down | implemented |
| ECx | Note cut | implemented |
| EDx | Note delay | implemented |
| EEx | Pattern delay | implemented |
| EFx | Invert loop | deliberately not implemented per spec ("don't bother") |
| 8xx | Set pan | not implemented (ProTracker ignores; Amiga uses hard-pan LRRL) |

Loop handling is forward-only per MOD spec — ping-pong / bidi loops are an
XM/IT/S3M-era extension and are deliberately not used here.

## License

MIT — see [LICENSE](LICENSE).
