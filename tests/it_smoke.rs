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
use oxideav_mod::it_writer::{square_sample, ItWriter, ItWriterPattern};
use oxideav_mod::{
    container::OUTPUT_SAMPLE_RATE, it, register_codecs, register_containers, CODEC_ID_IT_STR,
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
    assert!(dm
        .metadata()
        .iter()
        .any(|(k, v)| k == "title" && v == "smoke-it"));
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
                for chunk in a.data[0].chunks_exact(2) {
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
