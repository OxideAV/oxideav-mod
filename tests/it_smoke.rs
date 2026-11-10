//! End-to-end smoke test for Impulse Tracker (`.it`) container + decoder
//! over the public registry path.
//!
//! - `probe` via the container registry: the `IMPM` magic must win over
//!   the MOD / STM / XM probes.
//! - `open` → demuxer metadata + single-packet emission.
//! - The IT playback decoder: parses the packet, drives
//!   `ItPlayerState`, emits interleaved S16 stereo `AudioFrame`s.
//! - `it::parse_module` on the demuxed payload — the structural route.

use std::io::Cursor;

use oxideav_core::ContainerRegistry;
use oxideav_core::{CodecId, CodecParameters, Error, Frame};
use oxideav_core::{CodecRegistry, Decoder};
use oxideav_mod::it_writer::{ItWriter, ItWriterPattern, square_sample};
use oxideav_mod::{
    CODEC_ID_IT_STR, container::OUTPUT_SAMPLE_RATE, it, register_codecs, register_containers,
};

fn build_smoke_it() -> Vec<u8> {
    let mut w = ItWriter {
        song_name: "smoke-it".into(),
        ..ItWriter::default()
    };
    w.orders = vec![0, 255];
    w.samples.push(square_sample(64, 8, 16000));
    let mut p = ItWriterPattern::new(8);
    p.note(0, 0, 60, 1).note(4, 1, 67, 1);
    w.patterns.push(p);
    w.build()
}

#[test]
fn it_registry_probe_demux_decode() {
    let bytes = build_smoke_it();

    let mut containers = ContainerRegistry::new();
    register_containers(&mut containers);
    let mut codecs = CodecRegistry::new();
    register_codecs(&mut codecs);

    let mut input = Cursor::new(bytes.clone());
    let name = containers
        .probe_input(&mut input, Some("it"))
        .expect("probe_input must classify a valid IT");
    assert_eq!(name, "it");
    let mut input = Cursor::new(bytes.clone());
    let name = containers
        .probe_input(&mut input, None)
        .expect("the IMPM magic alone classifies the file");
    assert_eq!(name, "it");

    let mut dm = containers
        .open_demuxer("it", Box::new(Cursor::new(bytes.clone())), &codecs)
        .expect("open it");
    assert_eq!(dm.format_name(), "it");
    assert_eq!(dm.streams().len(), 1);
    let params = dm.streams()[0].params.clone();
    assert_eq!(params.codec_id, CodecId::new(CODEC_ID_IT_STR));
    assert_eq!(params.sample_rate, Some(OUTPUT_SAMPLE_RATE));
    assert!(
        dm.metadata()
            .iter()
            .any(|(k, v)| k == "title" && v == "smoke-it")
    );
    assert_eq!(dm.duration_micros(), Some(960_000));

    let pkt = dm.next_packet().expect("one packet");
    assert!(matches!(dm.next_packet(), Err(Error::Eof)));

    let m = it::parse_module(&pkt.data).unwrap();
    assert_eq!(m.num_channels, 2);
    assert_eq!(m.patterns[0].cell(4, 1).note, 67);

    let mut dec: Box<dyn Decoder> = codecs
        .first_decoder(&params)
        .expect("IT decoder registered");
    dec.send_packet(&pkt).unwrap();
    let mut frames = 0u64;
    let mut nonzero = 0u64;
    loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(a)) => {
                frames += a.samples as u64;
                for chunk in a.data[0].as_chunks::<2>().0 {
                    if i16::from_le_bytes([chunk[0], chunk[1]]) != 0 {
                        nonzero += 1;
                    }
                }
            }
            Ok(_) => unreachable!(),
            Err(Error::Eof) => break,
            Err(e) => panic!("{e:?}"),
        }
    }
    // 8 rows × 6 ticks × 882 frames.
    assert_eq!(frames, 8 * 6 * 882);
    assert!(nonzero > 1000);

    let _ = CodecParameters::audio(CodecId::new(CODEC_ID_IT_STR));
}

/// The two example songs shipped with Impulse Tracker 2.14 patch 5,
/// read from the staged docs tree when it is checked out next to this
/// crate (skips, passing, when absent). Both were written by the
/// tracker itself and carry 30 block-compressed sample bodies between
/// them — the ground truth for the decompressor: every block's bit
/// stream must end exactly at its declared length with every frame
/// decoded.
#[test]
fn it214_distribution_songs_decompress_exactly_and_play_one_pass() {
    use oxideav_mod::it;
    use oxideav_mod::it_player::ItPlayerState;

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/audio/trackers/it/fixtures/it214v5-distribution");
    let mut compressed_total = 0;
    for (name, want_compressed, want_16bit) in [("CHRIS31B.IT", 16, 0), ("JEFF93.IT", 14, 1)] {
        let Ok(bytes) = std::fs::read(dir.join(name)) else {
            eprintln!("[it] SKIP: staged {name} not found");
            return;
        };
        let module = it::parse_module(&bytes).expect("module");
        let mut compressed = 0;
        let mut sixteen = 0;
        for s in &module.samples {
            if !s.has_sample() || !s.compressed() {
                continue;
            }
            compressed += 1;
            sixteen += s.is_16_bit() as usize;
            assert_eq!(s.pcm.len(), s.length as usize, "{name}: {}", s.name);
            assert!(!s.truncated, "{name}: {}", s.name);
            let report = it::decompress_it214(
                &bytes[s.sample_pointer as usize..],
                s.length as usize,
                s.is_16_bit(),
                false,
            );
            assert_eq!(report.short_blocks, 0, "{name}: {}", s.name);
            assert_eq!(
                report.exact_blocks, report.blocks,
                "{name}: {} — every block must end on its declared length",
                s.name
            );
            let frames_per_block = if s.is_16_bit() { 0x4000 } else { 0x8000 };
            assert_eq!(
                report.blocks,
                (s.length as usize).div_ceil(frames_per_block)
            );
            // Decoded audio is smooth: the mean step between frames is
            // a small fraction of full scale (noise would average ~1/3).
            let rough: f64 = s
                .pcm
                .windows(2)
                .map(|w| (w[1] as f64 - w[0] as f64).abs())
                .sum::<f64>()
                / s.pcm.len() as f64;
            assert!(rough < 8000.0, "{name}: {} roughness {rough:.0}", s.name);
        }
        assert_eq!(
            (compressed, sixteen),
            (want_compressed, want_16bit),
            "{name}"
        );
        compressed_total += compressed;

        // The whole song plays one finite, audible pass.
        let mut player = ItPlayerState::new(module, 44_100);
        let mut buf = vec![0i16; 8192 * 2];
        let mut frames = 0usize;
        let mut energy = 0f64;
        loop {
            let n = player.render(&mut buf);
            if n == 0 {
                break;
            }
            frames += n;
            energy += buf[..n * 2]
                .iter()
                .map(|&v| (v as f64).powi(2))
                .sum::<f64>();
            assert!(frames < 44_100 * 1200, "{name}: the pass must be finite");
        }
        let rms = (energy / (frames as f64 * 2.0)).sqrt();
        eprintln!(
            "[it] {name}: {:.1} s, rms {rms:.0}",
            frames as f64 / 44_100.0
        );
        assert!(frames > 44_100 * 30, "{name}: a real song lasts a while");
        assert!(rms > 300.0, "{name}: audible (rms {rms:.0})");
    }
    assert_eq!(compressed_total, 30);
}
