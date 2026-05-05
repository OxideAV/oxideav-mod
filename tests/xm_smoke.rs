//! End-to-end smoke test for FastTracker 2 (`.xm`) structural support.
//!
//! Exercises:
//!
//! - `probe_xm` via the container registry: the 17-byte
//!   `"Extended Module: "` banner must dominate the STM and MOD probes.
//! - `open_xm` → demuxer metadata + single-packet emission.
//! - The stub XM decoder: parses/validates, then returns `unsupported`
//!   rather than silent zeros (matches the STM decoder contract).
//! - `xm::parse_header` / `parse_patterns` / `parse_instruments` /
//!   `extract_sample_bodies` on the packet payload emitted by the
//!   demuxer — the route structural callers use today.

use std::io::Cursor;

use oxideav_core::ContainerRegistry;
use oxideav_core::{CodecId, CodecParameters, Error, Packet, TimeBase};
use oxideav_core::{CodecRegistry, Decoder};
use oxideav_mod::{
    container::OUTPUT_SAMPLE_RATE, register_codecs, register_containers, xm, CODEC_ID_XM_STR,
};

/// Build a minimal XM file with:
///  - 4 channels, 1 pattern (4 rows), 1 instrument (1 sample, 4-byte body).
///  - A small packed pattern that exercises the mask byte (note+volume only).
fn build_minimal_xm() -> Vec<u8> {
    let mut out = vec![0u8; xm::XM_MIN_HEADER_LEN];
    out[0..17].copy_from_slice(xm::XM_BANNER);
    let name = b"smoke-xm            ";
    out[17..37].copy_from_slice(name);
    out[xm::XM_ID_BYTE_OFFSET] = 0x1A;
    let tracker = b"oxideav             ";
    out[38..58].copy_from_slice(tracker);
    out[58..60].copy_from_slice(&xm::XM_VERSION_0104.to_le_bytes());
    out[60..64].copy_from_slice(&0x114u32.to_le_bytes());
    out[64..66].copy_from_slice(&1u16.to_le_bytes()); // song_length
    out[66..68].copy_from_slice(&0u16.to_le_bytes()); // restart
    out[68..70].copy_from_slice(&4u16.to_le_bytes()); // num_channels
    out[70..72].copy_from_slice(&1u16.to_le_bytes()); // num_patterns
    out[72..74].copy_from_slice(&1u16.to_le_bytes()); // num_instruments
    out[74..76].copy_from_slice(&1u16.to_le_bytes()); // flags: linear
    out[76..78].copy_from_slice(&6u16.to_le_bytes()); // tempo
    out[78..80].copy_from_slice(&125u16.to_le_bytes()); // bpm
                                                        // order[0] = 0, rest 0xFF
    for i in 1..xm::XM_ORDER_TABLE_SIZE {
        out[xm::XM_ORDER_TABLE_OFFSET + i] = 0xFF;
    }

    // --- Pattern #0 header + packed data ---
    //
    // 4 rows × 4 channels. Row 0 / channel 0 carries a packed cell with
    // note + volume only (mask 0x05). All other cells are empty packed
    // (single 0x80 byte).
    let mut packed: Vec<u8> = Vec::new();
    for row in 0..4usize {
        for ch in 0..4usize {
            if row == 0 && ch == 0 {
                packed.push(0x80 | 0x05); // mask: note + volume
                packed.push(49); // note (C-4)
                packed.push(0x40); // volume = 0x40 → "set volume 48"
            } else {
                packed.push(0x80); // empty
            }
        }
    }
    out.extend_from_slice(&9u32.to_le_bytes()); // pattern header length
    out.push(0); // packing type
    out.extend_from_slice(&4u16.to_le_bytes()); // 4 rows
    out.extend_from_slice(&(packed.len() as u16).to_le_bytes());
    out.extend(packed);

    // --- Instrument #0: one 8-bit sample, 4-byte body ---
    const HSIZE: u32 = 0x107;
    let inst_start = out.len();
    out.extend_from_slice(&HSIZE.to_le_bytes());
    let mut nbuf = [0u8; 22];
    nbuf[..4].copy_from_slice(b"bass");
    out.extend_from_slice(&nbuf);
    out.push(0); // instrument type
    out.extend_from_slice(&1u16.to_le_bytes()); // num_samples = 1

    // Extended instrument block (file-offset +29 from inst_start):
    out.extend_from_slice(&xm::XM_SAMPLE_HEADER_SIZE.to_le_bytes()); // sample_header_size
                                                                     // sample_map (96 bytes of zero).
    out.extend(vec![0u8; 96]);
    // Volume envelope: (0,0) + (32,64) — 2 points.
    let mut vol_env = [0u8; 48];
    vol_env[0..2].copy_from_slice(&0u16.to_le_bytes());
    vol_env[2..4].copy_from_slice(&0u16.to_le_bytes());
    vol_env[4..6].copy_from_slice(&32u16.to_le_bytes());
    vol_env[6..8].copy_from_slice(&64u16.to_le_bytes());
    out.extend_from_slice(&vol_env);
    out.extend_from_slice(&[0u8; 48]); // panning envelope
    out.push(2); // num_vol_points
    out.push(0); // num_pan_points
    out.push(0); // vol sustain
    out.push(0); // vol loop start
    out.push(0); // vol loop end
    out.push(0); // pan sustain
    out.push(0); // pan loop start
    out.push(0); // pan loop end
    out.push(0x01); // vol type: On
    out.push(0); // pan type
    out.push(0); // vibrato type
    out.push(0); // vibrato sweep
    out.push(0); // vibrato depth
    out.push(0); // vibrato rate
    out.extend_from_slice(&512u16.to_le_bytes()); // fadeout
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    while out.len() - inst_start < HSIZE as usize {
        out.push(0); // pad to HSIZE
    }

    // Sample header (0x28 bytes).
    let body = [1i8, 2, 3, 4]; // delta stream: decodes to [1,3,6,10]
    out.extend_from_slice(&(body.len() as u32).to_le_bytes()); // length
    out.extend_from_slice(&0u32.to_le_bytes()); // loop_start
    out.extend_from_slice(&0u32.to_le_bytes()); // loop_length
    out.push(0x40); // volume
    out.push(0); // finetune
    out.push(0); // type: 8-bit, no loop
    out.push(128); // panning
    out.push(0); // relative_note
    out.push(0); // reserved
    let mut sname = [0u8; 22];
    sname[..3].copy_from_slice(b"bb1");
    out.extend_from_slice(&sname);

    // Sample body.
    for &b in &body {
        out.push(b as u8);
    }

    out
}

#[test]
fn probe_identifies_xm_from_banner() {
    let mut reg = ContainerRegistry::new();
    register_containers(&mut reg);
    let bytes = build_minimal_xm();
    let mut input = Cursor::new(bytes);
    let name = reg
        .probe_input(&mut input, Some("xm"))
        .expect("probe_input must classify valid XM");
    assert_eq!(name, "xm");
}

#[test]
fn open_xm_populates_metadata_and_emits_one_packet() {
    let mut reg = ContainerRegistry::new();
    register_containers(&mut reg);
    let bytes = build_minimal_xm();
    let cursor = Cursor::new(bytes.clone());

    let codec_reg = oxideav_core::CodecRegistry::new();
    let mut demux = reg
        .open_demuxer("xm", Box::new(cursor), &codec_reg)
        .expect("xm demuxer must be registered");

    assert_eq!(demux.format_name(), "xm");
    let streams: Vec<_> = demux.streams().to_vec();
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].params.sample_rate, Some(OUTPUT_SAMPLE_RATE));

    let md = demux.metadata();
    assert!(md.iter().any(|(k, v)| k == "title" && v == "smoke-xm"));
    assert!(md.iter().any(|(k, v)| k == "tracker" && v == "oxideav"));
    assert!(md.iter().any(|(k, _)| k == "extra_info"));

    // Duration estimate is present (best-effort; just check > 0).
    assert!(demux.duration_micros().map(|d| d > 0).unwrap_or(false));

    // First packet carries the entire file.
    let pkt = demux.next_packet().expect("first packet available");
    assert_eq!(pkt.data, bytes);

    // Second call drains with Eof.
    match demux.next_packet() {
        Err(Error::Eof) => {}
        other => panic!("expected Eof on second next_packet, got {other:?}"),
    }
}

#[test]
fn xm_decoder_rejects_with_unsupported_but_is_constructible() {
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    let params = CodecParameters::audio(CodecId::new(CODEC_ID_XM_STR));
    let mut dec: Box<dyn Decoder> = reg.first_decoder(&params).expect("xm decoder constructible");

    let bytes = build_minimal_xm();
    let pkt = Packet::new(0, TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64), bytes);
    match dec.send_packet(&pkt) {
        Err(Error::Unsupported(msg)) => {
            assert!(
                msg.contains("XM"),
                "error message should mention XM, got: {msg}"
            );
        }
        other => panic!("expected Unsupported on send_packet, got {other:?}"),
    }
}

#[test]
fn xm_decoder_rejects_non_xm_blob_as_invalid() {
    let mut reg = CodecRegistry::new();
    register_codecs(&mut reg);
    let params = CodecParameters::audio(CodecId::new(CODEC_ID_XM_STR));
    let mut dec: Box<dyn Decoder> = reg.first_decoder(&params).expect("xm decoder constructible");

    let bytes = vec![0u8; xm::XM_MIN_HEADER_LEN];
    let pkt = Packet::new(0, TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64), bytes);
    match dec.send_packet(&pkt) {
        Err(Error::InvalidData(_)) => {}
        other => panic!("expected InvalidData on non-XM blob, got {other:?}"),
    }
}

#[test]
fn parse_patterns_and_instruments_off_demuxed_packet() {
    let mut reg = ContainerRegistry::new();
    register_containers(&mut reg);
    let bytes = build_minimal_xm();
    let cursor = Cursor::new(bytes);

    let codec_reg = oxideav_core::CodecRegistry::new();
    let mut demux = reg
        .open_demuxer("xm", Box::new(cursor), &codec_reg)
        .expect("xm demuxer");
    let pkt = demux.next_packet().expect("packet");

    // Header.
    let hdr = xm::parse_header(&pkt.data).expect("header");
    assert_eq!(hdr.module_name, "smoke-xm");
    assert_eq!(hdr.num_channels, 4);
    assert_eq!(hdr.num_patterns, 1);
    assert_eq!(hdr.num_instruments, 1);
    assert_eq!(hdr.frequency_table, xm::XmFrequencyTable::Linear);

    // Patterns.
    let (patterns, after_patterns) = xm::parse_patterns(&hdr, &pkt.data).expect("patterns");
    assert_eq!(patterns.len(), 1);
    let p0 = &patterns[0];
    assert_eq!(p0.num_rows, 4);
    assert_eq!(p0.rows.len(), 4);
    let cell = p0.rows[0][0];
    assert_eq!(cell.note, 49);
    assert_eq!(cell.volume, 0x40);
    assert_eq!(cell.volume_kind(), xm::XmVolume::SetVolume(0x30));
    // Remaining cells in row 0 are empty.
    for ch in 1..4 {
        assert_eq!(p0.rows[0][ch], xm::XmCell::default());
    }
    // Later rows are all empty.
    for row in 1..4 {
        for ch in 0..4 {
            assert_eq!(p0.rows[row][ch], xm::XmCell::default());
        }
    }

    // Instruments.
    let mut instruments =
        xm::parse_instruments(&hdr, &pkt.data, after_patterns).expect("instruments");
    assert_eq!(instruments.len(), 1);
    let inst = &instruments[0];
    assert_eq!(inst.name, "bass");
    assert_eq!(inst.num_samples, 1);
    assert_eq!(inst.sample_header_size, xm::XM_SAMPLE_HEADER_SIZE);
    assert_eq!(inst.volume_envelope.points.len(), 2);
    assert_eq!(inst.volume_envelope.points[1], (32, 64));
    assert!(inst.volume_envelope.is_on());
    let s = &inst.samples[0];
    assert_eq!(s.name, "bb1");
    assert_eq!(s.length, 4);
    assert_eq!(s.volume, 0x40);

    // Decode sample delta stream.
    xm::extract_sample_bodies(&mut instruments, &pkt.data);
    assert_eq!(instruments[0].samples[0].pcm8, vec![1, 3, 6, 10]);
}
