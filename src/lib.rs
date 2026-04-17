//! Amiga ProTracker / SoundTracker module ("MOD") support.
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
//!   the drop-in option for plug-and-play playback.
//! - A **per-channel decoder** under codec id [`CODEC_ID_PLANAR_STR`] =
//!   `"mod_planar"`. Emits planar S16P `AudioFrame`s with one plane per
//!   MOD tracker channel (4 / 6 / 8 / … / 32), post-volume but
//!   pre-pan/pre-mix. Consumers that need independent channel streams
//!   (DAWs, visualisers, per-instrument remastering) select this codec
//!   id instead of `"mod"`.
//!
//! The tracker convention of exposing per-channel streams alongside a
//! mixed stereo mix is shared across tracker formats — see
//! `MEMORY.md → MOD multichannel` for the broader sketch.
//!
//! Decode only — there is no MOD encoder, by design.

pub mod container;
pub mod decoder;
pub mod header;
pub mod player;
pub mod samples;

use oxideav_codec::CodecRegistry;
use oxideav_container::ContainerRegistry;

/// Codec id for the mixed-stereo MOD decoder.
pub const CODEC_ID_STR: &str = "mod";

/// Codec id for the planar per-channel MOD decoder.
pub const CODEC_ID_PLANAR_STR: &str = "mod_planar";

pub fn register_codecs(reg: &mut CodecRegistry) {
    decoder::register(reg);
}

pub fn register_containers(reg: &mut ContainerRegistry) {
    container::register(reg);
}
