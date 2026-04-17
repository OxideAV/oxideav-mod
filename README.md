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
  bodies; drives a `PlayerState` (rows → ticks, Paula periods, volume / volume
  slide, portamento, arpeggio, `Cxx` set-volume, `Bxx` position jump, `Dxy`
  pattern break, `Fxx` speed / BPM); mixes samples with linear interpolation.
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

Early but functional. The effect set is a practical subset (arpeggio, 1/2
portamento up/down, Axy volume slide, Bxx position jump, Cxx set-volume, Dxy
pattern break, Fxx speed / BPM). More exotic effects (vibrato 4/6, tone
portamento 3, tremolo 7, fine slides E-commands, sample offset 9, pattern
loop E6) are not yet implemented — samples on patterns that use them will
still play, but without those effect modulations.

## License

MIT — see [LICENSE](LICENSE).
