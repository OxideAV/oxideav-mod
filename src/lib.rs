//! Amiga ProTracker / SoundTracker module ("MOD") support, plus a
//! structural parser for Scream Tracker v1 (`.stm`) files.
//!
//! MOD files are self-contained song data: a 20-byte title, 31 sample
//! descriptors, a pattern order list, a 4-character signature that
//! identifies the channel count, 64×N-channel patterns, then raw signed
//! 8-bit sample bodies.
//!
//! This crate registers:
//!
//! - A **container** (`mod`) that slurps the entire file and emits it as
//!   a single packet. The "packets" abstraction isn't natural for MOD —
//!   playback is driven by song position + effect state, not per-packet
//!   decode — so the container just delivers the bytes to the codec.
//! - A **mixed-stereo decoder** under codec id [`CODEC_ID_STR`] = `"mod"`.
//!   Emits one interleaved S16 stereo `AudioFrame` every ~1024 samples;
//!   the drop-in option for plug-and-play playback. Stereo pan defaults
//!   to a partial L/R separation rather than the strict Amiga hard pan
//!   — see [`player::PlayerState::set_pan_separation`] for the override
//!   and the rationale (`Protracker-effects-MODFIL12.txt` §11 itself
//!   recommends *against* full hard pan "Especially when using
//!   headphones").
//! - A **per-channel decoder** under codec id [`CODEC_ID_PLANAR_STR`] =
//!   `"mod_planar"`. Emits planar S16P `AudioFrame`s with one plane per
//!   MOD tracker channel (4 / 6 / 8 / … / 32), post-volume but
//!   pre-pan/pre-mix. Consumers that need independent channel streams
//!   (DAWs, visualisers, per-instrument remastering) select this codec
//!   id instead of `"mod"`.
//! - A **container** (`stm`) that recognises Scream Tracker v1 modules
//!   (pre-S3M, 4-channel-fixed), parses the header + instrument table +
//!   pattern data + sample bodies, and exposes them for downstream
//!   consumers. The associated codec id [`CODEC_ID_STM_STR`] = `"stm"`
//!   is a **full playback decoder**: it parses the packet, builds an
//!   [`stm_player::StmPlayerState`] over the shared [`mixer::MixerVoice`]
//!   core + [`mixer::StmC3Pitch`] pitch model, and emits interleaved
//!   `S16` stereo PCM at the same output rate as the MOD decoder.
//!   Honours every effect that the Scream Tracker v1 spec lists as "in
//!   ProTracker format" — `0xy` arpeggio, `1xy` / `2xy` portamento up /
//!   down, `3xy` / `5xy` tone portamento (with volume slide), `4xy` /
//!   `6xy` vibrato (with volume slide), `7xy` tremolo, `Axy` volume
//!   slide, `Bxy` position jump, `Cxx` set volume, `Dxy` pattern break,
//!   `Fxx` speed/tempo split, and the `E1x` / `E2x` / `EAx` / `EBx` /
//!   `ECx` / `EDx` Exy subcommands. Callers that need structural-only
//!   access (no PCM) can still reach for [`stm::parse_header`] /
//!   [`stm::parse_patterns`] / [`stm::extract_samples`] directly.
//! - A **container** (`xm`) that recognises FastTracker 2 Extended
//!   Module files by the 17-byte `"Extended Module: "` ASCII banner at
//!   offset 0. Parses the 336-byte file header (banner, module/tracker
//!   names, version, header size, song length, restart position,
//!   channel / pattern / instrument counts, frequency-table flag,
//!   default tempo / BPM, 256-entry order table), the variable-length
//!   bit-packed patterns (note / instrument / volume-column / effect /
//!   effect-param, each optional per mask byte), and the instrument
//!   table (per-note sample mapping, volume + panning envelopes, vibrato
//!   state, fadeout, multiple samples per instrument with delta-encoded
//!   8- or 16-bit PCM bodies). The associated codec id
//!   [`CODEC_ID_XM_STR`] = `"xm"` is a **full playback decoder**: it
//!   parses the whole-file packet, builds an
//!   [`xm_player::XmPlayerState`] over the shared [`mixer::MixerVoice`]
//!   core and [`mixer::XmPitch`] pitch model (both Amiga and Linear
//!   frequency tables supported), and emits interleaved `S16` stereo
//!   PCM at the same output rate as the MOD and STM decoders. Volume +
//!   panning envelopes (tick-based linear interpolation with
//!   sustain-point hold and loop-start/loop-end looping), fadeout (on
//!   key-off / note 97), key-off events, every FT2 standard effect
//!   listed in `docs/audio/trackers/xm/FT2-effects-list.txt`, the
//!   eleven volume-column kinds, and the instrument auto-vibrato
//!   waveform-shape + don't-retrigger flag are honoured by the
//!   playback engine. Callers that need structural-only access can
//!   still reach for [`xm::parse_header`] / [`xm::parse_patterns`] /
//!   [`xm::parse_instruments`] / [`xm::extract_sample_bodies`]
//!   directly.
//! - A **container** (`it`) that recognises Impulse Tracker modules by
//!   the `IMPM` magic at offset 0 and parses the header, order list,
//!   instrument / sample / pattern offset tables, song message,
//!   instruments (old 1.x and 2.x layouts), samples (8/16-bit,
//!   signed/unsigned, delta, both loop pairs) and packed patterns per
//!   `docs/audio/trackers/it/ImpulseTracker-it.txt`. The associated
//!   codec id [`CODEC_ID_IT_STR`] = `"it"` is a **full playback
//!   decoder** driving [`it_player::ItPlayerState`] — sample mode and
//!   instrument mode (envelopes, fadeout, NNA virtual channels,
//!   duplicate checks), linear and Amiga slides, and the `Axx`..`Zxx`
//!   effect set — over the shared [`mixer::MixerVoice`]. Structural
//!   callers use [`it::parse_module`] (or the per-block
//!   [`it::parse_header`] / [`it::parse_instruments`] /
//!   [`it::parse_samples`] / [`it::parse_patterns`]) directly.
//!
//! The tracker convention of exposing per-channel streams alongside a
//! mixed stereo mix is shared across tracker formats — see
//! `MEMORY.md → MOD multichannel` for the broader sketch.
//!
//! Decode only — there is no MOD, STM, XM or IT encoder, by design.

pub mod container;
pub mod decoder;
pub mod header;
pub mod it;
pub mod it_player;
#[doc(hidden)]
pub mod it_writer;
pub mod mixer;
pub mod player;
pub mod samples;
pub mod stm;
pub mod stm_player;
pub mod xm;
pub mod xm_player;

use oxideav_core::CodecRegistry;
use oxideav_core::ContainerRegistry;
use oxideav_core::RuntimeContext;

/// Codec id for the mixed-stereo MOD decoder.
pub const CODEC_ID_STR: &str = "mod";

/// Codec id for the planar per-channel MOD decoder.
pub const CODEC_ID_PLANAR_STR: &str = "mod_planar";

/// Codec id for the STM (Scream Tracker v1) parsing-only decoder.
pub const CODEC_ID_STM_STR: &str = "stm";

/// Codec id for the XM (FastTracker 2 Extended Module) playback decoder.
pub const CODEC_ID_XM_STR: &str = "xm";

/// Codec id for the IT (Impulse Tracker) playback decoder.
pub const CODEC_ID_IT_STR: &str = "it";

pub fn register_codecs(reg: &mut CodecRegistry) {
    decoder::register(reg);
}

pub fn register_containers(reg: &mut ContainerRegistry) {
    container::register(reg);
}

/// Unified entry point: install every codec and container provided by
/// `oxideav-mod` into a [`RuntimeContext`].
///
/// Also wired into [`oxideav_meta::register_all`] via the
/// [`oxideav_core::register!`] macro below. The
/// short-name `amiga_mod` matches the umbrella's existing trace name
/// from the #502 sweep.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("amiga_mod", register);

#[cfg(test)]
mod register_tests {
    use super::*;

    #[test]
    fn register_via_runtime_context_installs_factories() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        assert!(
            ctx.codecs.decoder_ids().next().is_some(),
            "register(ctx) should install codec decoder factories"
        );
        assert!(
            ctx.containers.demuxer_names().next().is_some(),
            "register(ctx) should install container demuxer factories"
        );
    }
}
