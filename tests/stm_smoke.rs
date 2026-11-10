//! End-to-end smoke test for Scream Tracker 2 (`.stm`) support.
//!
//! Exercises:
//!
//! - `probe_stm` via the container registry, including the boost for
//!   the `!Scream!` tracker-name banner.
//! - `open_stm` → demuxer metadata + single-packet emission.
//! - The full STM decoder: parses/validates the packet then emits one
//!   or more interleaved S16 stereo `AudioFrame`s via
//!   `StmPlayerState::render`.
//! - `stm::parse_patterns` + `stm::extract_samples` on the packet
//!   payload emitted by the demuxer, the route callers use for
//!   structural STM inspection.

use std::io::Cursor;

use oxideav_core::ContainerRegistry;
use oxideav_core::{CodecId, CodecParameters, Error, Frame, Packet, TimeBase};
use oxideav_core::{CodecRegistry, Decoder};
use oxideav_mod::stm_writer::{StmWriter, StmWriterInstrument, StmWriterPattern};
use oxideav_mod::{
    CODEC_ID_STM_STR, container::OUTPUT_SAMPLE_RATE, register_codecs, register_containers, stm,
};

/// A minimal valid module: one pattern with one distinctive cell and
/// one instrument carrying a 4-byte body, in the layout Scream Tracker
/// 2.21 writes.
fn build_minimal_stm() -> Vec<u8> {
    let mut w = StmWriter {
        title: "song".into(),
        orders: vec![0],
        ..StmWriter::default()
    };
    w.instruments.push(StmWriterInstrument {
        name: "kick".into(),
        pcm: vec![0x90, 0x70, 0xC0, 0x40],
        ..StmWriterInstrument::default()
    });
    let mut p = StmWriterPattern::new();
    p.put(
        0,
        0,
        stm::StmCell {
            note_raw: 0x40, // octave 4, C
            instrument: 1,
            volume: 10,
            command: 4, // D
            command_param: 0x0C,
        },
    );
    w.patterns.push(p);
    w.build()
}

#[test]
fn probe_identifies_stm_from_scream_banner() {
    let mut reg = ContainerRegistry::new();
    register_containers(&mut reg);
    let bytes = build_minimal_stm();
    let mut input = Cursor::new(bytes);
    let name = reg
        .probe_input(&mut input, Some("stm"))
        .expect("probe_input must classify valid STM");
    assert_eq!(name, "stm");
}

#[test]
fn open_stm_populates_metadata_and_emits_one_packet() {
    let mut reg = ContainerRegistry::new();
    register_containers(&mut reg);
    let bytes = build_minimal_stm();
    let cursor = Cursor::new(bytes.clone());

    let codec_reg = oxideav_core::CodecRegistry::new();
    let mut demux = reg
        .open_demuxer("stm", Box::new(cursor), &codec_reg)
        .expect("stm demuxer must be registered");

    assert_eq!(demux.format_name(), "stm");
    let streams: Vec<_> = demux.streams().to_vec();
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].params.sample_rate, Some(OUTPUT_SAMPLE_RATE));

    let md = demux.metadata();
    assert!(md.iter().any(|(k, v)| k == "title" && v == "song"));
    assert!(md.iter().any(|(k, v)| k == "tracker" && v == "!Scream!"));
    assert!(
        md.iter()
            .any(|(k, v)| k == "tracker_version" && v == "2.21")
    );
    assert!(md.iter().any(|(k, _)| k == "sample"));
    assert!(
        md.iter()
            .any(|(k, v)| k == "extra_info" && v.starts_with("module, 1 orders"))
    );

    // First packet carries the whole file.
    let pkt = demux.next_packet().expect("first packet available");
    assert_eq!(pkt.data, bytes);

    // Second call drains with Eof.
    match demux.next_packet() {
        Err(Error::Eof) => {}
        other => panic!("expected Eof on second next_packet, got {other:?}"),
    }
}

#[test]
fn stm_decoder_emits_pcm_through_codec_pipeline() {
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    let params = CodecParameters::audio(CodecId::new(CODEC_ID_STM_STR));
    let mut dec: Box<dyn Decoder> = reg
        .first_decoder(&params)
        .expect("stm decoder constructible");

    let bytes = build_minimal_stm();
    let pkt = Packet::new(0, TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64), bytes);
    dec.send_packet(&pkt).expect("stm send_packet must succeed");

    // The whole song is one 64-row pattern at tempo 0x60: the decoder
    // must emit exactly 7.68 s and then report Eof on its own.
    let mut total = 0u64;
    loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(a)) => {
                assert_eq!(a.data.len(), 1, "stm decoder emits interleaved stereo");
                assert_eq!(
                    a.data[0].len(),
                    a.samples as usize * 4,
                    "S16 stereo plane must be `samples * 4` bytes"
                );
                total += a.samples as u64;
                assert!(total < 44_100 * 60, "the decoder must terminate");
            }
            Ok(other) => panic!("expected Audio frame, got {other:?}"),
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected decode error: {e:?}"),
        }
    }
    assert_eq!(total, 64 * 6 * 882);
}

#[test]
fn stm_decoder_rejects_non_stm_blob_as_invalid() {
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    let params = CodecParameters::audio(CodecId::new(CODEC_ID_STM_STR));
    let mut dec: Box<dyn Decoder> = reg
        .first_decoder(&params)
        .expect("stm decoder constructible");

    // Zero blob — no 0x1A id byte, so is_stm returns false.
    let bytes = vec![0u8; 0x490];
    let pkt = Packet::new(0, TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64), bytes);
    match dec.send_packet(&pkt) {
        Err(Error::InvalidData(_)) => {}
        other => panic!("expected InvalidData on non-STM blob, got {other:?}"),
    }
}

#[test]
fn parse_patterns_and_samples_off_demuxed_packet() {
    let mut reg = ContainerRegistry::new();
    register_containers(&mut reg);
    let bytes = build_minimal_stm();
    let cursor = Cursor::new(bytes);

    let codec_reg = oxideav_core::CodecRegistry::new();
    let mut demux = reg
        .open_demuxer("stm", Box::new(cursor), &codec_reg)
        .expect("stm demuxer");
    let pkt = demux.next_packet().expect("packet");

    let hdr = stm::parse_header(&pkt.data).expect("header");
    assert_eq!(hdr.title, "song");
    assert_eq!(hdr.n_patterns, 1);
    assert_eq!(hdr.tempo, 0x60);

    let patterns = stm::parse_patterns(&hdr, &pkt.data);
    assert_eq!(patterns.len(), 1);
    let cell = patterns[0].rows[0][0];
    assert_eq!(
        cell.kind(),
        stm::StmNoteKind::Note {
            octave: 4,
            semitone: 0
        },
    );
    assert_eq!(cell.instrument, 1);
    assert_eq!(cell.volume, 10);
    assert_eq!(cell.command, 4);
    assert_eq!(cell.command_param, 0x0C);

    let samples = stm::extract_samples(&hdr, &pkt.data);
    assert_eq!(samples.len(), 31);
    assert_eq!(samples[0].pcm.len(), 4);
    assert_eq!(samples[0].c3_hz, 8363);
}
