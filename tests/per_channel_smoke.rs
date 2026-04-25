//! Integration smoke tests for the planar (per-channel) MOD decoder.
//!
//! Confirms that selecting codec id `mod_planar` through the registry
//! produces one S16P plane per MOD tracker channel, and that a pattern
//! triggering notes only on channel 0 leaves channels 1..=3 silent in
//! their dedicated planes — which a mixed-stereo output cannot verify.

use oxideav_core::{CodecId, CodecParameters, Error, Frame, Packet, SampleFormat, TimeBase};
use oxideav_core::{CodecRegistry, Decoder};
use oxideav_mod::{container::OUTPUT_SAMPLE_RATE, register_codecs, CODEC_ID_PLANAR_STR};

const HEADER_FIXED_SIZE: usize = 1084;
const PATTERN_BYTES: usize = 64 * 4 * 4;

/// Same shape as the mixed-stereo integration test: a 4-channel `M.K.`
/// MOD with one looping 32-byte half-wave sample. Row 0 channel 0
/// triggers the sample; all other cells are empty.
fn build_channel0_trigger_mod() -> Vec<u8> {
    let mut out = vec![0u8; HEADER_FIXED_SIZE];
    out[0..4].copy_from_slice(b"test");

    out[20 + 22..20 + 24].copy_from_slice(&16u16.to_be_bytes());
    out[20 + 24] = 0;
    out[20 + 25] = 64;
    out[20 + 26..20 + 28].copy_from_slice(&0u16.to_be_bytes());
    out[20 + 28..20 + 30].copy_from_slice(&16u16.to_be_bytes());

    out[950] = 1;
    out[951] = 0x7F;
    out[952] = 0;
    out[1080..1084].copy_from_slice(b"M.K.");

    let mut pat = vec![0u8; PATTERN_BYTES];
    let period: u16 = 428;
    let p_hi = ((period >> 8) & 0x0F) as u8;
    let p_lo = (period & 0xFF) as u8;
    pat[0] = p_hi;
    pat[1] = p_lo;
    pat[2] = 1 << 4;
    pat[3] = 0;
    out.extend(pat);

    for i in 0..32 {
        let v: i8 = if i < 16 { 80 } else { -80 };
        out.push(v as u8);
    }
    out
}

#[test]
fn planar_decoder_via_registry_emits_one_plane_per_channel() {
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    let codec_id = CodecId::new(CODEC_ID_PLANAR_STR);
    let params = CodecParameters::audio(codec_id);
    let mut dec: Box<dyn Decoder> = reg.make_decoder(&params).expect("planar decoder available");

    let pkt = Packet::new(
        0,
        TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64),
        build_channel0_trigger_mod(),
    );
    dec.send_packet(&pkt).expect("send_packet");

    // Accumulate per-plane energy across however many frames we decode
    // until we've seen at least ~0.1s worth — enough to span several
    // ticks.
    let mut plane_energy: Vec<u64> = Vec::new();
    let mut total_frames = 0usize;
    let target_frames = 4410;

    loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(a)) => {
                assert_eq!(a.format, SampleFormat::S16P, "expected planar S16P");
                assert_eq!(a.sample_rate, OUTPUT_SAMPLE_RATE);
                assert_eq!(a.channels as usize, a.data.len());
                assert_eq!(a.channels, 4, "M.K. MOD has 4 tracker channels");

                let plane_bytes_expected = a.samples as usize * 2;
                if plane_energy.is_empty() {
                    plane_energy.resize(a.channels as usize, 0);
                }
                for (idx, plane) in a.data.iter().enumerate() {
                    assert_eq!(plane.len(), plane_bytes_expected);
                    for chunk in plane.chunks_exact(2) {
                        let s = i16::from_le_bytes([chunk[0], chunk[1]]) as i64;
                        plane_energy[idx] += (s * s) as u64;
                    }
                }
                total_frames += a.samples as usize;
                if total_frames >= target_frames {
                    break;
                }
            }
            Ok(_) => unreachable!("MOD emits audio only"),
            Err(Error::Eof) => break,
            Err(e) => panic!("decode error: {e:?}"),
        }
    }

    assert!(total_frames > 0, "planar decoder produced no samples");
    assert!(
        plane_energy[0] > 1_000_000,
        "channel 0 must carry energy (got {})",
        plane_energy[0]
    );
    for (i, &energy) in plane_energy.iter().enumerate().skip(1) {
        assert_eq!(
            energy, 0,
            "channel {i} must be silent (got energy {energy}); per-channel isolation broken"
        );
    }

    eprintln!("per_channel_smoke: plane energy = {plane_energy:?}");
}
