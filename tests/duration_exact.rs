//! The demuxers report the exact length of one pass: the engine is run
//! without mixing at open time, so speed / tempo changes, jumps, breaks,
//! loops and delays are all followed. For every format, the reported
//! duration must equal the number of frames the decoder then emits.

use std::io::Cursor;

use oxideav_core::{CodecId, CodecParameters, CodecRegistry, ContainerRegistry, Error, Frame};
use oxideav_mod::it_writer::{ItWriter, ItWriterPattern, ItWriterSample};
use oxideav_mod::stm::StmCell;
use oxideav_mod::stm_player::command;
use oxideav_mod::stm_writer::{StmWriter, StmWriterInstrument, StmWriterPattern};
use oxideav_mod::xm_writer::{XmWriter, XmWriterPattern, single_sample_instrument, square_sample};
use oxideav_mod::{register_codecs, register_containers};

/// Open `bytes` as `format`; returns (reported duration, decoded frames).
fn duration_and_frames(format: &str, bytes: Vec<u8>) -> (i64, u64) {
    let mut containers = ContainerRegistry::new();
    register_containers(&mut containers);
    let mut codecs = CodecRegistry::new();
    register_codecs(&mut codecs);
    let mut demux = containers
        .open_demuxer(format, Box::new(Cursor::new(bytes)), &codecs)
        .expect("demuxer");
    let reported = demux.duration_micros().expect("duration");
    let params = CodecParameters::audio(CodecId::new(format));
    let mut dec = codecs.first_decoder(&params).expect("decoder");
    dec.send_packet(&demux.next_packet().expect("packet"))
        .expect("send_packet");
    let mut frames = 0u64;
    loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(a)) => frames += a.samples as u64,
            Ok(_) => panic!("audio expected"),
            Err(Error::Eof) => break,
            Err(e) => panic!("{e:?}"),
        }
        assert!(frames < 44_100 * 3600, "{format}: the pass must be finite");
    }
    (reported, frames)
}

fn micros(frames: u64) -> i64 {
    (frames as i128 * 1_000_000 / 44_100) as i64
}

#[test]
fn mod_duration_follows_speed_tempo_breaks_and_the_loop_back() {
    // Two patterns: F03 (speed 3) on row 0, F96 (150 BPM) on row 8, a
    // D00 break on row 31 of pattern 0, EE2 on row 4 of pattern 1 and a
    // B00 loop-back on its row 15.
    let mut bytes = vec![0u8; 1084];
    bytes[20 + 22..20 + 24].copy_from_slice(&32u16.to_be_bytes());
    bytes[20 + 25] = 64;
    bytes[20 + 28..20 + 30].copy_from_slice(&1u16.to_be_bytes());
    bytes[950] = 2;
    bytes[951] = 0x7F;
    bytes[952] = 0;
    bytes[953] = 1;
    bytes[1080..1084].copy_from_slice(b"M.K.");
    let mut patterns = vec![0u8; 2 * 64 * 4 * 4];
    let mut put = |pat: usize, row: usize, ch: usize, fx: u8, param: u8| {
        let off = pat * 1024 + (row * 4 + ch) * 4;
        patterns[off + 2] = fx;
        patterns[off + 3] = param;
    };
    put(0, 0, 0, 0xF, 0x03);
    put(0, 8, 1, 0xF, 0x96);
    put(0, 31, 2, 0xD, 0x00);
    put(1, 4, 0, 0xE, 0xE2);
    put(1, 15, 3, 0xB, 0x00);
    // A note so there is something to play.
    patterns[0] = 0x01;
    patterns[1] = 0xAC;
    patterns[2] |= 0x10;
    bytes.extend(patterns);
    bytes.extend(std::iter::repeat_n(0x40u8, 64));
    let (reported, frames) = duration_and_frames("mod", bytes);
    assert_eq!(reported, micros(frames));
    // 8 rows at 125 BPM (882 frames / tick), then 24 + 16 rows plus the
    // two delayed row-times at 150 BPM (735 frames / tick), speed 3.
    assert_eq!(frames, 8 * 3 * 882 + (24 + 16 + 2) * 3 * 735);
}

#[test]
fn stm_duration_follows_tempo_and_the_deferred_jump() {
    let mut w = StmWriter::default();
    w.instruments.push(StmWriterInstrument {
        pcm: vec![0xC0; 64],
        ..StmWriterInstrument::default()
    });
    for _ in 0..3 {
        w.patterns.push(StmWriterPattern::new());
    }
    w.orders = vec![0, 1, 2];
    let fx = |cmd: u8, info: u8| StmCell {
        command: cmd,
        command_param: info,
        ..StmCell::default()
    };
    w.patterns[0].put(0, 0, fx(command::TEMPO, 0x30));
    w.patterns[0].put(4, 1, fx(command::JUMP, 2)); // deferred to the end
    w.patterns[2].put(9, 2, fx(command::BREAK, 0)); // order 3 unused: end
    let (reported, frames) = duration_and_frames("stm", w.build());
    assert_eq!(reported, micros(frames));
    assert_eq!(frames, (64 + 10) * 3 * 882);
}

#[test]
fn xm_duration_follows_speed_breaks_and_the_restart() {
    let mut w = XmWriter {
        num_channels: 2,
        default_tempo: 4,
        default_bpm: 125,
        ..XmWriter::default()
    };
    w.instruments
        .push(single_sample_instrument(square_sample(64, 16, 8000)));
    let mut p0 = XmWriterPattern::new(16);
    p0.note(0, 0, 49, 1);
    p0.effect(7, 1, 0x0D, 0x02); // break to row 2 of pattern 1
    let mut p1 = XmWriterPattern::new(12);
    p1.effect(5, 0, 0x0F, 0x02); // speed 2
    w.patterns = vec![p0, p1];
    w.orders = vec![0, 1];
    let (reported, frames) = duration_and_frames("xm", w.build());
    assert_eq!(reported, micros(frames));
    // Rows 0..=7 at speed 4, rows 2..=4 at speed 4, rows 5..=11 at 2.
    assert_eq!(frames, (8 * 4 + 3 * 4 + 7 * 2) * 882);
}

#[test]
fn it_duration_follows_speed_tempo_and_pattern_loops() {
    let mut w = ItWriter::default();
    w.samples.push(ItWriterSample {
        pcm: vec![8000; 64],
        ..ItWriterSample::default()
    });
    let mut p = ItWriterPattern::new(16);
    p.note(0, 0, 60, 1);
    p.effect(1, 0, 'A', 0x03); // speed 3
    p.effect(2, 0, 'S', 0xB0); // loop start
    p.effect(4, 0, 'S', 0xB2); // rows 2..=4 three times
    p.effect(6, 0, 'T', 0x96); // 150 BPM
    p.effect(9, 0, 'C', 0x00); // break: next order is the end
    w.patterns.push(p);
    w.orders = vec![0, 255];
    let (reported, frames) = duration_and_frames("it", w.build());
    assert_eq!(reported, micros(frames));
    let speed0 = w.initial_speed as u64;
    // Row 0 at the initial speed; rows 1..=5 with the loop body twice
    // more (5 + 6 rows) at speed 3 / 125 BPM; rows 6..=9 at 150 BPM.
    assert_eq!(frames, speed0 * 882 + (5 + 6) * 3 * 882 + 4 * 3 * 735);
}
