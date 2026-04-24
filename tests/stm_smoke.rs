//! End-to-end smoke test for Scream Tracker v1 (`.stm`) support.
//!
//! Exercises:
//!
//! - `probe_stm` via the container registry, including the boost for
//!   the `!Scream!` tracker-name banner.
//! - `open_stm` → demuxer metadata + single-packet emission.
//! - The stub STM decoder: parses/validates, then returns an explicit
//!   `unsupported` error rather than silent zeros.
//! - `stm::parse_patterns` + `stm::extract_samples` on the packet
//!   payload emitted by the demuxer, the route callers use today for
//!   structural STM inspection.

use std::io::Cursor;

use oxideav_codec::{CodecRegistry, Decoder};
use oxideav_container::ContainerRegistry;
use oxideav_core::{CodecId, CodecParameters, Error, Packet, TimeBase};
use oxideav_mod::{
    container::OUTPUT_SAMPLE_RATE, register_codecs, register_containers, stm, CODEC_ID_STM_STR,
};

/// Build a minimal valid STM file with one pattern (mostly zero) and one
/// instrument carrying a 4-byte body.
fn build_minimal_stm() -> Vec<u8> {
    const HEADER_PREFIX: usize = 0x30;
    const INST_REC: usize = 32;
    const INST_COUNT: usize = 31;
    const ORDER_OFF: usize = 0x3D0;
    const ORDER_SIZE: usize = 64;
    const PATTERN_OFF: usize = 0x410;
    const BYTES_PER_PATTERN: usize = 64 * 4 * 4;

    let n_patterns = 1u8;
    let mut out = vec![0u8; PATTERN_OFF];
    out[0..4].copy_from_slice(b"song");
    out[0x14..0x1C].copy_from_slice(b"!Scream!");
    out[0x1C] = 0x1A;
    out[0x1D] = 2; // module
    out[0x1E] = 2;
    out[0x1F] = 0;
    out[0x20] = 0x60; // tempo
    out[0x21] = n_patterns;
    out[0x22] = 64;

    // Instrument 0 at offset HEADER_PREFIX.
    let inst_off = HEADER_PREFIX;
    out[inst_off..inst_off + 4].copy_from_slice(b"kick");
    out[inst_off + 16..inst_off + 18].copy_from_slice(&4u16.to_le_bytes());
    out[inst_off + 22] = 64;
    out[inst_off + 24..inst_off + 26].copy_from_slice(&8363u16.to_le_bytes());

    // Order table: pattern 0 at index 0, 255 elsewhere.
    for i in 0..ORDER_SIZE {
        out[ORDER_OFF + i] = if i == 0 { 0 } else { 255 };
    }

    // Compile-time sanity — layout values must match crate constants.
    assert_eq!(stm::HEADER_PREFIX_SIZE, HEADER_PREFIX);
    assert_eq!(stm::INSTRUMENT_RECORD_SIZE, INST_REC);
    assert_eq!(stm::INSTRUMENT_COUNT, INST_COUNT);
    assert_eq!(stm::ORDER_TABLE_OFFSET, ORDER_OFF);
    assert_eq!(stm::ORDER_TABLE_SIZE, ORDER_SIZE);
    assert_eq!(stm::PATTERN_DATA_OFFSET, PATTERN_OFF);
    assert_eq!(stm::BYTES_PER_PATTERN, BYTES_PER_PATTERN);

    // Pattern 0 — mostly zeros, but put a distinctive cell at row 0 / ch 0
    // so parse_patterns has something interesting to decode.
    let mut pattern = vec![0u8; BYTES_PER_PATTERN];
    pattern[0] = 0x40; // octave 4, semitone 0
    pattern[1] = (1 << 3) | 2; // instrument 1, vol_lo 2
    pattern[2] = (1 << 4) | 3; // vol_hi 1, command 3
    pattern[3] = 0x0C;
    out.extend(pattern);

    // Instrument 0 body.
    out.extend([0x10, 0xF0, 0x40, 0xC0]);
    out
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

    let codec_reg = oxideav_codec::CodecRegistry::new();
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
    assert!(md.iter().any(|(k, _)| k == "sample"));
    assert!(md.iter().any(|(k, _)| k == "extra_info"));

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
fn stm_decoder_rejects_with_unsupported_but_is_constructible() {
    // The decoder is a stub — it validates the STM header then returns
    // `unsupported`. That's the contract: parsing infrastructure is
    // wired, playback isn't. Later work can turn this into real audio.
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    let params = CodecParameters::audio(CodecId::new(CODEC_ID_STM_STR));
    let mut dec: Box<dyn Decoder> = reg
        .make_decoder(&params)
        .expect("stm decoder constructible");

    let bytes = build_minimal_stm();
    let pkt = Packet::new(0, TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64), bytes);
    match dec.send_packet(&pkt) {
        Err(Error::Unsupported(msg)) => {
            assert!(
                msg.contains("STM"),
                "error message should mention STM, got: {msg}"
            );
        }
        other => panic!("expected Unsupported on send_packet, got {other:?}"),
    }
}

#[test]
fn stm_decoder_rejects_non_stm_blob_as_invalid() {
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    let params = CodecParameters::audio(CodecId::new(CODEC_ID_STM_STR));
    let mut dec: Box<dyn Decoder> = reg
        .make_decoder(&params)
        .expect("stm decoder constructible");

    // Zero blob — no 0x1A id byte, so is_stm returns false.
    let bytes = vec![0u8; 0x410];
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

    let codec_reg = oxideav_codec::CodecRegistry::new();
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
    assert_eq!(cell.command, 3);
    assert_eq!(cell.command_param, 0x0C);

    let samples = stm::extract_samples(&hdr, &pkt.data);
    assert_eq!(samples.len(), 31);
    assert_eq!(samples[0].pcm.len(), 4);
    assert_eq!(samples[0].c3_hz, 8363);
}
