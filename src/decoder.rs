//! MOD codec decoder — ProTracker playback.
//!
//! Consumes the whole-file packet from the MOD container, parses the
//! header + patterns + sample bodies, and drives a `PlayerState` forward,
//! emitting PCM until the song ends.
//!
//! Two output modes are available:
//!
//! - [`ModDecoder`] (codec id [`crate::CODEC_ID_STR`] = `"mod"`) produces
//!   mixed stereo S16 interleaved PCM. One `AudioFrame` every
//!   `CHUNK_FRAMES` samples.
//! - [`ModPlanarDecoder`] (codec id [`crate::CODEC_ID_PLANAR_STR`] =
//!   `"mod_planar"`) produces one planar `AudioFrame` per tick, with one
//!   S16P plane per MOD tracker channel. Downstream consumers that want
//!   to mix / pan / analyse channels independently (DAWs, visualizers,
//!   per-instrument tooling) drive the decoder via this codec id.

use oxideav_codec::{CodecInfo, CodecRegistry, Decoder};
use oxideav_core::{
    AudioFrame, CodecCapabilities, CodecId, CodecParameters, Error, Frame, Packet, Result,
    SampleFormat, TimeBase,
};

use crate::container::OUTPUT_SAMPLE_RATE;
use crate::header::parse_header;
use crate::player::{parse_patterns, PlayerState};
use crate::samples::extract_samples;

pub fn register(reg: &mut CodecRegistry) {
    let mixed_caps = CodecCapabilities::audio("mod_sw")
        .with_lossy(false)
        .with_lossless(true)
        .with_intra_only(false)
        .with_max_channels(32)
        .with_max_sample_rate(OUTPUT_SAMPLE_RATE);
    reg.register(
        CodecInfo::new(CodecId::new(crate::CODEC_ID_STR))
            .capabilities(mixed_caps)
            .decoder(make_mixed_decoder),
    );

    let planar_caps = CodecCapabilities::audio("mod_sw_planar")
        .with_lossy(false)
        .with_lossless(true)
        .with_intra_only(false)
        .with_max_channels(32)
        .with_max_sample_rate(OUTPUT_SAMPLE_RATE);
    reg.register(
        CodecInfo::new(CodecId::new(crate::CODEC_ID_PLANAR_STR))
            .capabilities(planar_caps)
            .decoder(make_planar_decoder),
    );
}

fn make_mixed_decoder(_params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(ModDecoder {
        codec_id: CodecId::new(crate::CODEC_ID_STR),
        state: DecoderState::AwaitingPacket,
    }))
}

fn make_planar_decoder(_params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(ModPlanarDecoder {
        codec_id: CodecId::new(crate::CODEC_ID_PLANAR_STR),
        state: DecoderState::AwaitingPacket,
    }))
}

struct ModDecoder {
    codec_id: CodecId,
    state: DecoderState,
}

enum DecoderState {
    /// Haven't seen the file yet.
    AwaitingPacket,
    /// File parsed; the player is driving the mixer.
    Playing {
        player: Box<PlayerState>,
        emit_pts: i64,
    },
    /// All samples produced.
    Done,
}

const CHUNK_FRAMES: u32 = 1024;

impl Decoder for ModDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        // The MOD "container" delivers the whole file in one packet.
        if !matches!(self.state, DecoderState::AwaitingPacket) {
            return Err(Error::other(
                "MOD decoder received a second packet; only one is expected per song",
            ));
        }
        let header = parse_header(&packet.data)?;
        let samples = extract_samples(&header, &packet.data);
        let patterns = parse_patterns(&header, &packet.data);
        let player = PlayerState::new(&header, samples, patterns, OUTPUT_SAMPLE_RATE);
        self.state = DecoderState::Playing {
            player: Box::new(player),
            emit_pts: 0,
        };
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match &mut self.state {
            DecoderState::AwaitingPacket => Err(Error::NeedMore),
            DecoderState::Done => Err(Error::Eof),
            DecoderState::Playing { player, emit_pts } => {
                // Allocate stereo interleaved buffer.
                let mut pcm = vec![0i16; CHUNK_FRAMES as usize * 2];
                let produced = player.render(&mut pcm);
                if produced == 0 {
                    self.state = DecoderState::Done;
                    return Err(Error::Eof);
                }
                // Truncate to what we actually produced.
                pcm.truncate(produced * 2);

                // Convert to little-endian S16 byte buffer.
                let mut bytes = Vec::with_capacity(pcm.len() * 2);
                for s in &pcm {
                    bytes.extend_from_slice(&s.to_le_bytes());
                }

                let pts = *emit_pts;
                *emit_pts += produced as i64;
                Ok(Frame::Audio(AudioFrame {
                    format: SampleFormat::S16,
                    channels: 2,
                    sample_rate: OUTPUT_SAMPLE_RATE,
                    samples: produced as u32,
                    pts: Some(pts),
                    time_base: TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64),
                    data: vec![bytes],
                }))
            }
        }
    }

    fn flush(&mut self) -> Result<()> {
        if let DecoderState::Playing { .. } = self.state {
            // Draining is implicit — `receive_frame` will return Eof once
            // the player reports no more samples.
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        // The entire PlayerState (pattern-order cursor, per-channel mixer
        // state with sample position, volume/period history, arpeggio /
        // vibrato phases, BPM + tick counter) is dropped. The MOD
        // container delivers the whole file in one packet, so after a
        // reset we go back to `AwaitingPacket` and the container is
        // expected to re-send the file from the top of the song.
        self.state = DecoderState::AwaitingPacket;
        Ok(())
    }
}

/// Planar per-channel decoder. Emits one `AudioFrame` per render chunk,
/// with `AudioFrame.channels` equal to the MOD tracker channel count and
/// `AudioFrame.data` holding one S16P plane per channel (post-volume,
/// pre-pan, pre-mix).
struct ModPlanarDecoder {
    codec_id: CodecId,
    state: DecoderState,
}

impl Decoder for ModPlanarDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if !matches!(self.state, DecoderState::AwaitingPacket) {
            return Err(Error::other(
                "MOD decoder received a second packet; only one is expected per song",
            ));
        }
        let header = parse_header(&packet.data)?;
        let samples = extract_samples(&header, &packet.data);
        let patterns = parse_patterns(&header, &packet.data);
        let player = PlayerState::new(&header, samples, patterns, OUTPUT_SAMPLE_RATE);
        self.state = DecoderState::Playing {
            player: Box::new(player),
            emit_pts: 0,
        };
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match &mut self.state {
            DecoderState::AwaitingPacket => Err(Error::NeedMore),
            DecoderState::Done => Err(Error::Eof),
            DecoderState::Playing { player, emit_pts } => {
                let n_ch = player.channels.len();
                let n_frames = CHUNK_FRAMES as usize;

                // One i16 buffer per MOD channel, pre-sized to the chunk.
                let mut bufs: Vec<Vec<i16>> = (0..n_ch).map(|_| vec![0i16; n_frames]).collect();
                let produced = {
                    let mut views: Vec<&mut [i16]> =
                        bufs.iter_mut().map(|b| b.as_mut_slice()).collect();
                    player.render_per_channel(&mut views, n_frames)
                };
                if produced == 0 {
                    self.state = DecoderState::Done;
                    return Err(Error::Eof);
                }

                // Truncate each plane and convert to little-endian bytes.
                let mut planes: Vec<Vec<u8>> = Vec::with_capacity(n_ch);
                for buf in &bufs {
                    let mut bytes = Vec::with_capacity(produced * 2);
                    for &s in &buf[..produced] {
                        bytes.extend_from_slice(&s.to_le_bytes());
                    }
                    planes.push(bytes);
                }

                let pts = *emit_pts;
                *emit_pts += produced as i64;
                Ok(Frame::Audio(AudioFrame {
                    format: SampleFormat::S16P,
                    channels: n_ch as u16,
                    sample_rate: OUTPUT_SAMPLE_RATE,
                    samples: produced as u32,
                    pts: Some(pts),
                    time_base: TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64),
                    data: planes,
                }))
            }
        }
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.state = DecoderState::AwaitingPacket;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::tests::synth_square_mod;
    use oxideav_core::TimeBase;

    #[test]
    fn decoder_emits_nonsilent_pcm() {
        let bytes = synth_square_mod();
        let params = CodecParameters::audio(CodecId::new(crate::CODEC_ID_STR));
        let mut dec = make_mixed_decoder(&params).unwrap();
        let pkt = Packet::new(0, TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64), bytes);
        dec.send_packet(&pkt).unwrap();

        let mut total_samples = 0u64;
        let mut total_nonzero = 0u64;
        loop {
            match dec.receive_frame() {
                Ok(Frame::Audio(a)) => {
                    assert_eq!(a.channels, 2);
                    assert_eq!(a.sample_rate, OUTPUT_SAMPLE_RATE);
                    assert_eq!(a.format, SampleFormat::S16);
                    total_samples += a.samples as u64;
                    // Count non-zero bytes in the PCM plane.
                    let plane = &a.data[0];
                    for chunk in plane.chunks_exact(2) {
                        let s = i16::from_le_bytes([chunk[0], chunk[1]]);
                        if s != 0 {
                            total_nonzero += 1;
                        }
                    }
                }
                Ok(_) => unreachable!("MOD emits audio only"),
                Err(Error::Eof) => break,
                Err(e) => panic!("unexpected decode error: {e:?}"),
            }
        }
        assert!(
            total_samples > 1000,
            "expected substantial sample output, got {total_samples}"
        );
        assert!(
            total_nonzero > 100,
            "expected non-silent PCM, got {total_nonzero} non-zero samples"
        );
    }

    #[test]
    fn planar_decoder_emits_one_plane_per_channel() {
        let bytes = synth_square_mod();
        let params = CodecParameters::audio(CodecId::new(crate::CODEC_ID_PLANAR_STR));
        let mut dec = make_planar_decoder(&params).unwrap();
        let pkt = Packet::new(0, TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64), bytes);
        dec.send_packet(&pkt).unwrap();

        let mut got_frame = false;
        let mut ch0_nonzero = 0u64;
        let mut other_nonzero = 0u64;
        loop {
            match dec.receive_frame() {
                Ok(Frame::Audio(a)) => {
                    got_frame = true;
                    // synth_square_mod uses the 4-channel "M.K." layout.
                    assert_eq!(a.channels, 4);
                    assert_eq!(a.format, SampleFormat::S16P);
                    assert_eq!(a.sample_rate, OUTPUT_SAMPLE_RATE);
                    assert_eq!(a.data.len(), 4, "one plane per MOD channel");
                    let expected_plane_len = a.samples as usize * 2;
                    for plane in &a.data {
                        assert_eq!(plane.len(), expected_plane_len);
                    }
                    for (idx, plane) in a.data.iter().enumerate() {
                        for chunk in plane.chunks_exact(2) {
                            let s = i16::from_le_bytes([chunk[0], chunk[1]]);
                            if s != 0 {
                                if idx == 0 {
                                    ch0_nonzero += 1;
                                } else {
                                    other_nonzero += 1;
                                }
                            }
                        }
                    }
                }
                Ok(_) => unreachable!("MOD emits audio only"),
                Err(Error::Eof) => break,
                Err(e) => panic!("unexpected decode error: {e:?}"),
            }
        }
        assert!(got_frame, "planar decoder produced no frames");
        // synth_square_mod triggers notes only on channel 0; other
        // channels must be pure silence.
        assert!(
            ch0_nonzero > 100,
            "expected channel-0 signal, got {ch0_nonzero} non-zero samples"
        );
        assert_eq!(
            other_nonzero, 0,
            "expected silence on channels 1..=3 (got {other_nonzero} non-zero samples)"
        );
    }
}
