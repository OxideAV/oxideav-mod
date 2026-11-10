//! Scream Tracker 2 module / song writer — a hidden test helper.
//!
//! Emits the layout measured on Scream Tracker 2.21 files
//! (`docs/audio/trackers/stm/stm-pattern-packing-and-order-table.md`):
//! 48-byte header, 31 instrument records, a 128-byte order table padded
//! with 99, then the patterns. A **module** (file type 2) stores every
//! cell in four bytes (empty = `FF 01 80 00`) and appends the 8-bit
//! unsigned sample bodies, each padded to 16 bytes except the last; a
//! **song** (file type 1) stores an empty cell as the single byte 252
//! and carries no sample bodies. Used by the unit / integration tests
//! and the steered fuzz target; not part of the supported API.

use crate::stm::{
    INSTRUMENT_COUNT, INSTRUMENT_RECORD_SIZE, ORDER_TABLE_OFFSET, ORDER_TABLE_SIZE, ORDER_UNUSED,
    PATTERN_DATA_OFFSET, PATTERN_ROWS, SAMPLE_ALIGN, STM_CHANNELS, StmCell,
};

/// One instrument record plus its body.
#[derive(Clone, Debug)]
pub struct StmWriterInstrument {
    pub name: String,
    /// On-disk body: 8-bit unsigned.
    pub pcm: Vec<u8>,
    pub loop_start: u16,
    pub loop_end: u16,
    pub volume: u8,
    pub c2spd: u16,
}

impl Default for StmWriterInstrument {
    fn default() -> Self {
        StmWriterInstrument {
            name: String::new(),
            pcm: Vec::new(),
            loop_start: 0,
            loop_end: 0xFFFF,
            volume: 64,
            c2spd: 8363,
        }
    }
}

/// One 64-row, 4-channel pattern.
#[derive(Clone, Debug)]
pub struct StmWriterPattern {
    pub cells: Vec<StmCell>,
}

impl Default for StmWriterPattern {
    fn default() -> Self {
        Self::new()
    }
}

impl StmWriterPattern {
    pub fn new() -> Self {
        StmWriterPattern {
            cells: vec![StmCell::default(); PATTERN_ROWS * STM_CHANNELS],
        }
    }

    /// Place a cell; out-of-range coordinates are ignored.
    pub fn put(&mut self, row: usize, ch: usize, cell: StmCell) {
        if row < PATTERN_ROWS && ch < STM_CHANNELS {
            self.cells[row * STM_CHANNELS + ch] = cell;
        }
    }
}

/// Module / song description.
#[derive(Clone, Debug)]
pub struct StmWriter {
    pub title: String,
    /// File type 1 (song: packed cells, no sample bodies) instead of 2.
    pub song: bool,
    pub version: (u8, u8),
    pub tempo: u8,
    pub global_volume: u8,
    pub instruments: Vec<StmWriterInstrument>,
    pub orders: Vec<u8>,
    pub patterns: Vec<StmWriterPattern>,
}

impl Default for StmWriter {
    fn default() -> Self {
        StmWriter {
            title: String::new(),
            song: false,
            version: (2, 21),
            tempo: 0x60,
            global_volume: 64,
            instruments: Vec::new(),
            orders: Vec::new(),
            patterns: Vec::new(),
        }
    }
}

impl StmWriter {
    pub fn build(&self) -> Vec<u8> {
        let mut out = vec![0u8; PATTERN_DATA_OFFSET];
        let title = self.title.as_bytes();
        let n = title.len().min(20);
        out[..n].copy_from_slice(&title[..n]);
        out[0x14..0x1C].copy_from_slice(b"!Scream!");
        out[0x1C] = 0x1A;
        out[0x1D] = if self.song { 1 } else { 2 };
        out[0x1E] = self.version.0;
        out[0x1F] = self.version.1;
        out[0x20] = self.tempo;
        out[0x21] = self.patterns.len().min(255) as u8;
        out[0x22] = self.global_volume;

        for i in 0..INSTRUMENT_COUNT {
            let off = 0x30 + i * INSTRUMENT_RECORD_SIZE;
            let rec = &mut out[off..off + INSTRUMENT_RECORD_SIZE];
            match self.instruments.get(i) {
                Some(ins) => {
                    let name = ins.name.as_bytes();
                    let n = name.len().min(12);
                    rec[..n].copy_from_slice(&name[..n]);
                    let len = ins.pcm.len().min(0xFFFF) as u16;
                    rec[16..18].copy_from_slice(&len.to_le_bytes());
                    rec[18..20].copy_from_slice(&ins.loop_start.to_le_bytes());
                    rec[20..22].copy_from_slice(&ins.loop_end.to_le_bytes());
                    rec[22] = ins.volume;
                    rec[24..26].copy_from_slice(&ins.c2spd.to_le_bytes());
                }
                None => {
                    // The empty record ST 2.21 writes: no-loop pair and
                    // the default C2SPD.
                    rec[20..22].copy_from_slice(&0xFFFFu16.to_le_bytes());
                    rec[24..26].copy_from_slice(&8363u16.to_le_bytes());
                }
            }
        }

        for i in 0..ORDER_TABLE_SIZE {
            out[ORDER_TABLE_OFFSET + i] = self.orders.get(i).copied().unwrap_or(ORDER_UNUSED);
        }

        let empty = StmCell::default();
        for p in self.patterns.iter().take(255) {
            for cell in &p.cells {
                if self.song && *cell == empty {
                    out.push(252);
                } else {
                    out.extend_from_slice(&cell.to_bytes());
                }
            }
        }

        if !self.song {
            let last = self
                .instruments
                .iter()
                .take(INSTRUMENT_COUNT)
                .rposition(|i| !i.pcm.is_empty());
            for (i, ins) in self.instruments.iter().take(INSTRUMENT_COUNT).enumerate() {
                let len = ins.pcm.len().min(0xFFFF);
                out.extend_from_slice(&ins.pcm[..len]);
                if len > 0 && Some(i) != last {
                    let pad = len.next_multiple_of(SAMPLE_ALIGN) - len;
                    out.extend(std::iter::repeat_n(0x80u8, pad));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stm::{extract_samples, parse_header, parse_patterns};

    #[test]
    fn writer_round_trips_through_the_parser() {
        let mut w = StmWriter {
            title: "round trip".into(),
            tempo: 0x50,
            global_volume: 48,
            orders: vec![1, 0],
            ..StmWriter::default()
        };
        w.instruments.push(StmWriterInstrument {
            name: "A.SMP".into(),
            pcm: (0..40u8).collect(),
            loop_start: 4,
            loop_end: 36,
            volume: 50,
            c2spd: 9000,
        });
        w.patterns.push(StmWriterPattern::new());
        w.patterns.push(StmWriterPattern::new());
        let cell = StmCell {
            note_raw: 0x31,
            instrument: 1,
            volume: 64,
            command: 8,
            command_param: 0xA4,
        };
        w.patterns[1].put(7, 2, cell);
        let bytes = w.build();
        let h = parse_header(&bytes).unwrap();
        assert_eq!(h.title, "round trip");
        assert_eq!((h.tempo, h.global_volume, h.n_patterns), (0x50, 48, 2));
        assert_eq!(&h.order[..3], &[1, 0, ORDER_UNUSED]);
        let ins = &h.instruments[0];
        assert_eq!((ins.loop_start, ins.loop_end, ins.volume), (4, 36, 50));
        assert_eq!(ins.c3_hz, 9000);
        let pats = parse_patterns(&h, &bytes);
        assert_eq!(pats[1].rows[7][2], cell);
        let bodies = extract_samples(&h, &bytes);
        assert_eq!(bodies[0].pcm.len(), 40);
        assert_eq!(bodies[0].pcm[0], -128);
    }
}
