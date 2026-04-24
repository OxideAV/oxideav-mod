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
//!   the drop-in option for plug-and-play playback.
//! - A **per-channel decoder** under codec id [`CODEC_ID_PLANAR_STR`] =
//!   `"mod_planar"`. Emits planar S16P `AudioFrame`s with one plane per
//!   MOD tracker channel (4 / 6 / 8 / … / 32), post-volume but
//!   pre-pan/pre-mix. Consumers that need independent channel streams
//!   (DAWs, visualisers, per-instrument remastering) select this codec
//!   id instead of `"mod"`.
//! - A **container** (`stm`) that recognises Scream Tracker v1 modules
//!   (pre-S3M, 4-channel-fixed), parses the header + instrument table +
//!   pattern data + sample bodies, and exposes them for downstream
//!   consumers. A minimum STM playback engine is wired via the shared
//!   [`mixer::MixerVoice`] core and [`mixer::StmC3Pitch`] pitch model;
//!   drive it via [`stm_player::StmPlayerState::render`]. The associated
//!   codec id [`CODEC_ID_STM_STR`] = `"stm"` is still a parsing-only
//!   decoder registration (it validates the packet then returns an
//!   explicit `unsupported`), because effect support and global mixer
//!   parameters are not yet feature-complete; callers wiring STM into
//!   the broader oxideav pipeline should hook up
//!   `StmPlayerState::render` directly for now.
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
//!   8- or 16-bit PCM bodies). A minimum XM playback engine is wired via
//!   the shared [`mixer::MixerVoice`] core and [`mixer::XmPitch`] pitch
//!   model (both Amiga and Linear frequency tables supported); drive it
//!   via [`xm_player::XmPlayerState::render`]. Volume + panning envelopes
//!   (tick-based linear interpolation with sustain-point hold and
//!   loop-start/loop-end looping), fadeout (on key-off / note 97), and
//!   key-off events are supported. Vibrato, tone portamento, and the
//!   bulk of the Bxy/Dxy/Exy/Fxy/Kxy/Lxy effect space remain
//!   unimplemented. The codec id [`CODEC_ID_XM_STR`] = `"xm"` remains
//!   a parsing-only decoder pending effect-set completeness — use
//!   [`xm::parse_header`] / [`xm::parse_patterns`] / [`xm::parse_instruments`]
//!   / [`xm::extract_sample_bodies`] for structural access, or drive
//!   `XmPlayerState` directly for PCM output.
//!
//! The tracker convention of exposing per-channel streams alongside a
//! mixed stereo mix is shared across tracker formats — see
//! `MEMORY.md → MOD multichannel` for the broader sketch.
//!
//! Decode only — there is no MOD or STM encoder, by design.

pub mod container;
pub mod decoder;
pub mod header;
pub mod mixer;
pub mod player;
pub mod samples;
pub mod stm;
pub mod stm_player;
pub mod xm;
pub mod xm_player;

use oxideav_codec::CodecRegistry;
use oxideav_container::ContainerRegistry;

/// Codec id for the mixed-stereo MOD decoder.
pub const CODEC_ID_STR: &str = "mod";

/// Codec id for the planar per-channel MOD decoder.
pub const CODEC_ID_PLANAR_STR: &str = "mod_planar";

/// Codec id for the STM (Scream Tracker v1) parsing-only decoder.
pub const CODEC_ID_STM_STR: &str = "stm";

/// Codec id for the XM (FastTracker 2 Extended Module) parsing-only decoder.
pub const CODEC_ID_XM_STR: &str = "xm";

pub fn register_codecs(reg: &mut CodecRegistry) {
    decoder::register(reg);
}

pub fn register_containers(reg: &mut ContainerRegistry) {
    container::register(reg);
}
