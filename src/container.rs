//! MOD as a container format.
//!
//! MOD files are self-contained and don't have a natural packetisation,
//! so the container here is a thin shim: it reads the whole file into
//! memory, parses the header to populate the stream's `CodecParameters`
//! (channel count, sample rate, sample format), then delivers the entire
//! file as a single packet to the codec.

use std::io::Read;

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Error, MediaType, Packet, Result, SampleFormat,
    StreamInfo, TimeBase,
};
use oxideav_core::{ContainerRegistry, Demuxer, ReadSeek};

use crate::header::parse_header;
use crate::it;
use crate::stm;
use crate::xm;

/// Output sample rate used by the decoder. 44.1 kHz is a common choice
/// that matches most "modern" MOD players; the Amiga Paula chip ran at
/// 7093789.2 Hz / divider so there's no "native" rate.
pub const OUTPUT_SAMPLE_RATE: u32 = 44_100;

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("mod", open);
    reg.register_extension("mod", "mod");
    reg.register_probe("mod", probe);

    // Scream Tracker v1 (.stm) registration — handled by a separate
    // demuxer / probe that emits a single packet under the "stm" codec.
    reg.register_demuxer("stm", open_stm);
    reg.register_extension("stm", "stm");
    reg.register_probe("stm", probe_stm);

    // FastTracker 2 (.xm) registration — single-packet demuxer guarded
    // by the 17-byte "Extended Module: " banner probe. The codec id is
    // wired to a full playback decoder (`make_xm_decoder` in the
    // decoder module); structural-only callers can still drive the
    // `xm::parse_*` helpers directly off the demuxed packet payload.
    reg.register_demuxer("xm", open_xm);
    reg.register_extension("xm", "xm");
    reg.register_probe("xm", probe_xm);

    // Impulse Tracker (.it) registration — single-packet demuxer
    // guarded by the 4-byte `IMPM` magic at offset 0. The codec id is
    // wired to the full playback decoder (`make_it_decoder`).
    reg.register_demuxer("it", open_it);
    reg.register_extension("it", "it");
    reg.register_probe("it", probe_it);
}

/// ProTracker / Soundtracker family signature at offset 1080 — a 4-byte
/// magic identifying the channel layout. If the file is too short to
/// reach offset 1084, fall back to extension confirmation.
fn probe(p: &oxideav_core::ProbeData) -> u8 {
    if p.buf.len() < 1084 {
        if p.ext == Some("mod") {
            return 25;
        }
        return 0;
    }
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&p.buf[1080..1084]);
    // Single source of truth for the tag catalogue: a tag the parser can
    // resolve to a channel count is a tag we probe positively. This keeps
    // probe and parse acceptance from ever drifting apart.
    if crate::header::is_known_signature(&magic) {
        return 100;
    }
    // SoundTracker 2.6 / IceTracker keeps its magic at +1464 instead
    // (`Soundtracker-v2.6-IceTracker-st26.txt`); `parse_header`
    // dispatches on the same check, keeping probe and parse aligned.
    if crate::header::is_st26_magic(p.buf) {
        return 100;
    }
    0
}

fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut blob = Vec::new();
    input.read_to_end(&mut blob)?;
    if blob.len() < crate::header::HEADER_FIXED_SIZE {
        return Err(Error::invalid("MOD: file shorter than 1084-byte header"));
    }
    let header = parse_header(&blob)?;

    let mut params = CodecParameters::audio(CodecId::new(crate::CODEC_ID_STR));
    params.media_type = MediaType::Audio;
    params.channels = Some(2); // mixed stereo output
    params.sample_rate = Some(OUTPUT_SAMPLE_RATE);
    params.sample_format = Some(SampleFormat::S16);
    params.extradata = blob.clone();

    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64),
        duration: None, // computed lazily by the decoder
        start_time: Some(0),
        params,
    };

    let metadata = build_metadata(&header);
    // Upper-bound duration estimate at the ProTracker default tempo
    // (speed=6 ticks/row, BPM=125 → 50 ticks/sec). Real songs commonly
    // change tempo via Fxx effects so this is typically a loose upper
    // bound; a true value needs a full playback simulation. Formula:
    //   song_length * 64 rows * 6 ticks / 50 tps.
    let duration_micros: i64 = (header.song_length as i64).saturating_mul(64 * 6 * 1_000_000) / 50;

    Ok(Box::new(ModDemuxer {
        streams: vec![stream],
        blob,
        consumed: false,
        metadata,
        duration_micros,
        _header: header,
    }))
}

fn build_metadata(h: &crate::header::ModHeader) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if !h.title.is_empty() {
        out.push(("title".into(), h.title.clone()));
    }
    // Live +951 restart position (NoiseTracker / FastTracker lineage —
    // see `header::ModHeader::restart_position`). Only emitted when the
    // byte is a real restart order: the ProTracker `$7F` / SoundTracker
    // `$78` fillers and out-of-range values carry no information.
    if let Some(restart) = h.restart_position() {
        out.push(("restart_position".into(), restart.to_string()));
    }
    // Emit the same key for every sample name so CLI continuation
    // formatting collapses the run into one block under a single label.
    for s in h.samples.iter() {
        if !s.name.is_empty() {
            out.push(("sample".into(), s.name.clone()));
        }
    }
    let n_nonempty_samples = h.samples.iter().filter(|s| s.length > 0).count();
    out.push((
        "extra_info".into(),
        format!(
            "{} patterns, {} channels, {}/{} samples",
            h.n_patterns,
            h.channels,
            n_nonempty_samples,
            h.samples.len()
        ),
    ));
    out
}

struct ModDemuxer {
    streams: Vec<StreamInfo>,
    blob: Vec<u8>,
    consumed: bool,
    metadata: Vec<(String, String)>,
    duration_micros: i64,
    _header: crate::header::ModHeader,
}

impl Demuxer for ModDemuxer {
    fn format_name(&self) -> &str {
        "mod"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if self.consumed {
            return Err(Error::Eof);
        }
        self.consumed = true;
        let data = std::mem::take(&mut self.blob);
        let stream = &self.streams[0];
        let mut pkt = Packet::new(0, stream.time_base, data);
        pkt.pts = Some(0);
        pkt.dts = Some(0);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        if self.duration_micros > 0 {
            Some(self.duration_micros)
        } else {
            None
        }
    }
}

/// Scream Tracker v1 probe: the only rock-solid byte-level signature is
/// `0x1A` at offset `0x1C` combined with a plausible file-type byte
/// (1 = song, 2 = module) at `0x1D`. The tracker-name field at
/// `0x14..0x1C` is informational but commonly `"!Scream!"`; we require
/// it to be printable ASCII in the probe to avoid false positives, but
/// do NOT hard-code any specific string so we don't miss Scream Tracker
/// clones or rewrites.
fn probe_stm(p: &oxideav_core::ProbeData) -> u8 {
    if stm::is_stm(p.buf) {
        // Bonus points when the tracker-name field matches common values;
        // otherwise still return a solid-but-not-perfect score so the MOD
        // probe (which requires a 1084-byte header) clearly wins on MOD
        // files and STM wins on STM files.
        if p.buf.len() >= 0x1C {
            let name = &p.buf[0x14..0x1C];
            if name.starts_with(b"!Scream!") || name.starts_with(b"!Scrn") {
                return 100;
            }
        }
        return 80;
    }
    if p.ext == Some("stm") && p.buf.len() >= stm::HEADER_PREFIX_SIZE {
        return 25;
    }
    0
}

fn open_stm(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut blob = Vec::new();
    input.read_to_end(&mut blob)?;
    if blob.len() < stm::ORDER_TABLE_OFFSET + stm::ORDER_TABLE_SIZE {
        return Err(Error::invalid(
            "STM: file shorter than 0x410-byte header+order block",
        ));
    }
    let header = stm::parse_header(&blob)?;

    let mut params = CodecParameters::audio(CodecId::new(crate::CODEC_ID_STM_STR));
    params.media_type = MediaType::Audio;
    params.channels = Some(2); // mixed stereo output (once playback lands)
    params.sample_rate = Some(OUTPUT_SAMPLE_RATE);
    params.sample_format = Some(SampleFormat::S16);
    params.extradata = blob.clone();

    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64),
        duration: None,
        start_time: Some(0),
        params,
    };

    let metadata = build_stm_metadata(&header);
    let duration_micros = stm::estimate_duration_micros(&header);

    Ok(Box::new(StmDemuxer {
        streams: vec![stream],
        blob,
        consumed: false,
        metadata,
        duration_micros,
        _header: header,
    }))
}

fn build_stm_metadata(h: &stm::StmHeader) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if !h.title.is_empty() {
        out.push(("title".into(), h.title.clone()));
    }
    if !h.tracker_name.is_empty() {
        out.push(("tracker".into(), h.tracker_name.clone()));
    }
    for inst in h.instruments.iter() {
        if !inst.name.is_empty() {
            out.push(("sample".into(), inst.name.clone()));
        }
    }
    let n_nonempty_insts = h.instruments.iter().filter(|i| i.length > 0).count();
    out.push((
        "extra_info".into(),
        format!(
            "{} patterns, {} channels (fixed), {}/{} instruments, tempo=0x{:02X}",
            h.n_patterns,
            stm::STM_CHANNELS,
            n_nonempty_insts,
            h.instruments.len(),
            h.tempo,
        ),
    ));
    out
}

struct StmDemuxer {
    streams: Vec<StreamInfo>,
    blob: Vec<u8>,
    consumed: bool,
    metadata: Vec<(String, String)>,
    duration_micros: i64,
    _header: stm::StmHeader,
}

impl Demuxer for StmDemuxer {
    fn format_name(&self) -> &str {
        "stm"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if self.consumed {
            return Err(Error::Eof);
        }
        self.consumed = true;
        let data = std::mem::take(&mut self.blob);
        let stream = &self.streams[0];
        let mut pkt = Packet::new(0, stream.time_base, data);
        pkt.pts = Some(0);
        pkt.dts = Some(0);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        if self.duration_micros > 0 {
            Some(self.duration_micros)
        } else {
            None
        }
    }
}

/// FastTracker 2 (.xm) probe: the canonical signature is the 17-byte
/// `"Extended Module: "` ASCII banner at offset 0 (capital M, trailing
/// colon+space — per `FastTracker-2-xm-alt.txt`, lowercase is rejected
/// by FT2 itself). Extension-only match is a weak fallback.
fn probe_xm(p: &oxideav_core::ProbeData) -> u8 {
    if xm::is_xm(p.buf) {
        return 100;
    }
    if p.ext == Some("xm") && p.buf.len() >= xm::XM_MIN_HEADER_LEN {
        return 25;
    }
    0
}

fn open_xm(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut blob = Vec::new();
    input.read_to_end(&mut blob)?;
    if blob.len() < xm::XM_MIN_HEADER_LEN {
        return Err(Error::invalid(
            "XM: file shorter than 336-byte banner+header+order block",
        ));
    }
    let header = xm::parse_header(&blob)?;

    let mut params = CodecParameters::audio(CodecId::new(crate::CODEC_ID_XM_STR));
    params.media_type = MediaType::Audio;
    params.channels = Some(2); // mixed stereo output (once playback lands)
    params.sample_rate = Some(OUTPUT_SAMPLE_RATE);
    params.sample_format = Some(SampleFormat::S16);
    params.extradata = blob.clone();

    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64),
        duration: None,
        start_time: Some(0),
        params,
    };

    // Duration estimate is best-effort: walk patterns once (cheaply) to
    // get row counts. If pattern parsing hiccups we just leave the
    // duration unset rather than error out of container open.
    let duration_micros = match xm::parse_patterns(&header, &blob) {
        Ok((pats, _)) => xm::estimate_duration_micros(&header, &pats),
        Err(_) => 0,
    };

    let metadata = build_xm_metadata(&header);

    Ok(Box::new(XmDemuxer {
        streams: vec![stream],
        blob,
        consumed: false,
        metadata,
        duration_micros,
        _header: header,
    }))
}

fn build_xm_metadata(h: &xm::XmHeader) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if !h.module_name.is_empty() {
        out.push(("title".into(), h.module_name.clone()));
    }
    if !h.tracker_name.is_empty() {
        out.push(("tracker".into(), h.tracker_name.clone()));
    }
    let freq = match h.frequency_table {
        xm::XmFrequencyTable::Amiga => "amiga",
        xm::XmFrequencyTable::Linear => "linear",
    };
    out.push((
        "extra_info".into(),
        format!(
            "{} channels, {} patterns, {} instruments, song_len={}, restart={}, tempo={}, bpm={}, freq={}, version=0x{:04X}",
            h.num_channels,
            h.num_patterns,
            h.num_instruments,
            h.song_length,
            h.restart_position,
            h.default_tempo,
            h.default_bpm,
            freq,
            h.version,
        ),
    ));
    out
}

struct XmDemuxer {
    streams: Vec<StreamInfo>,
    blob: Vec<u8>,
    consumed: bool,
    metadata: Vec<(String, String)>,
    duration_micros: i64,
    _header: xm::XmHeader,
}

impl Demuxer for XmDemuxer {
    fn format_name(&self) -> &str {
        "xm"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if self.consumed {
            return Err(Error::Eof);
        }
        self.consumed = true;
        let data = std::mem::take(&mut self.blob);
        let stream = &self.streams[0];
        let mut pkt = Packet::new(0, stream.time_base, data);
        pkt.pts = Some(0);
        pkt.dts = Some(0);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        if self.duration_micros > 0 {
            Some(self.duration_micros)
        } else {
            None
        }
    }
}

/// Impulse Tracker probe: the `IMPM` magic at offset 0
/// (`ImpulseTracker-it.txt` §"Impulse Header Layout"). Extension-only
/// match is a weak fallback.
fn probe_it(p: &oxideav_core::ProbeData) -> u8 {
    if it::is_it(p.buf) {
        return 100;
    }
    if p.ext == Some("it") && p.buf.len() >= it::IT_HEADER_FIXED_SIZE {
        return 25;
    }
    0
}

fn open_it(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut blob = Vec::new();
    input.read_to_end(&mut blob)?;
    if blob.len() < it::IT_HEADER_FIXED_SIZE {
        return Err(Error::invalid("IT: file shorter than the 0xC0-byte header"));
    }
    let header = it::parse_header(&blob)?;

    let mut params = CodecParameters::audio(CodecId::new(crate::CODEC_ID_IT_STR));
    params.media_type = MediaType::Audio;
    params.channels = Some(2);
    params.sample_rate = Some(OUTPUT_SAMPLE_RATE);
    params.sample_format = Some(SampleFormat::S16);
    params.extradata = blob.clone();

    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64),
        duration: None,
        start_time: Some(0),
        params,
    };

    let patterns = it::parse_patterns(&header, &blob);
    let duration_micros = it::estimate_duration_micros(&header, &patterns);
    let num_channels = patterns
        .iter()
        .map(|p| p.num_channels)
        .max()
        .unwrap_or(0)
        .max(1);
    let metadata = build_it_metadata(&header, &blob, num_channels);

    Ok(Box::new(ItDemuxer {
        streams: vec![stream],
        blob,
        consumed: false,
        metadata,
        duration_micros,
        _header: header,
    }))
}

fn build_it_metadata(h: &it::ItHeader, blob: &[u8], num_channels: u8) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if !h.song_name.is_empty() {
        out.push(("title".into(), h.song_name.clone()));
    }
    if let Some(msg) = it::extract_message(h, blob) {
        if !msg.trim().is_empty() {
            out.push(("comment".into(), msg));
        }
    }
    let (maj, min) = h.created_with_version();
    out.push((
        "tracker".into(),
        format!(
            "Impulse Tracker {maj}.{min:02X} (cmwt {:#06X})",
            h.compatible_with
        ),
    ));
    for &off in &h.sample_offsets {
        if let Ok(s) = it::parse_sample_header(blob, off as usize) {
            if !s.name.is_empty() {
                out.push(("sample".into(), s.name));
            }
        }
    }
    out.push((
        "extra_info".into(),
        format!(
            "{} channels, {} patterns, {} instruments, {} samples, {} orders, speed={}, tempo={}, {}, {} slides, gv={}, mv={}",
            num_channels,
            h.pattern_offsets.len(),
            h.instrument_offsets.len(),
            h.sample_offsets.len(),
            h.playable_order_count(),
            h.initial_speed,
            h.initial_tempo,
            if h.uses_instruments() { "instrument mode" } else { "sample mode" },
            if h.linear_slides() { "linear" } else { "amiga" },
            h.global_volume,
            h.mix_volume,
        ),
    ));
    out
}

struct ItDemuxer {
    streams: Vec<StreamInfo>,
    blob: Vec<u8>,
    consumed: bool,
    metadata: Vec<(String, String)>,
    duration_micros: i64,
    _header: it::ItHeader,
}

impl Demuxer for ItDemuxer {
    fn format_name(&self) -> &str {
        "it"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if self.consumed {
            return Err(Error::Eof);
        }
        self.consumed = true;
        let data = std::mem::take(&mut self.blob);
        let stream = &self.streams[0];
        let mut pkt = Packet::new(0, stream.time_base, data);
        pkt.pts = Some(0);
        pkt.dts = Some(0);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        if self.duration_micros > 0 {
            Some(self.duration_micros)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_probe_and_metadata() {
        use crate::it_writer::{square_sample, ItWriter, ItWriterPattern};
        let mut w = ItWriter {
            song_name: "probe me".into(),
            message: Some("msg".into()),
            ..ItWriter::default()
        };
        w.orders = vec![0, 0, 255];
        w.samples.push(square_sample(32, 4, 10000));
        let mut p = ItWriterPattern::new(8);
        p.note(0, 3, 60, 1);
        w.patterns.push(p);
        let bytes = w.build();
        let pd = oxideav_core::ProbeData {
            buf: &bytes,
            ext: None,
        };
        assert_eq!(probe_it(&pd), 100);
        assert_eq!(probe_stm(&pd), 0);
        assert_eq!(probe_xm(&pd), 0);
        let none = oxideav_core::ProbeData {
            buf: &[0u8; 300],
            ext: Some("it"),
        };
        assert_eq!(probe_it(&none), 25);
        let dm = open_it(
            Box::new(std::io::Cursor::new(bytes.clone())),
            &oxideav_core::CodecRegistry::new(),
        )
        .unwrap();
        assert_eq!(dm.format_name(), "it");
        let md = dm.metadata();
        assert!(md.iter().any(|(k, v)| k == "title" && v == "probe me"));
        assert!(md.iter().any(|(k, v)| k == "comment" && v == "msg"));
        assert!(md.iter().any(|(k, v)| k == "sample" && v == "square"));
        assert!(md
            .iter()
            .any(|(k, v)| k == "extra_info" && v.starts_with("4 channels, 1 patterns")));
        // 16 rows × 6 × 2.5/125 = 1.92 s.
        assert_eq!(dm.duration_micros(), Some(1_920_000));
    }

    #[test]
    fn metadata_surfaces_live_restart_position_only() {
        use crate::player::tests::synth_mod_with_order_table_and_restart;
        // A live +951 byte (below the song length, not a $7F/$78
        // filler) is real information for players and metadata
        // reporters; the filler conventions must stay silent so
        // ordinary ProTracker-lineage modules don't grow a spurious
        // key.
        let live = synth_mod_with_order_table_and_restart(3, 1, &[0, 1, 2], &[]);
        let h = crate::header::parse_header(&live).unwrap();
        let md = build_metadata(&h);
        assert!(
            md.iter().any(|(k, v)| k == "restart_position" && v == "1"),
            "a live restart byte must surface as restart_position=1"
        );

        let filler = synth_mod_with_order_table_and_restart(3, 0x7F, &[0, 1, 2], &[]);
        let h = crate::header::parse_header(&filler).unwrap();
        let md = build_metadata(&h);
        assert!(
            !md.iter().any(|(k, _)| k == "restart_position"),
            "the $7F filler must not emit a restart_position key"
        );
    }
}
