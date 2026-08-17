//! Sample-body extraction for MOD files.
//!
//! After the header + pattern data block, the remainder of the file is a
//! concatenation of raw signed-8-bit sample bodies, in the order samples
//! appear in the header. The header tells us each body's length in bytes.
//! Some files are truncated (the last sample's declared length exceeds
//! the file) — we clamp rather than error.

use crate::header::ModHeader;

/// Per-sample decoded body plus the loop metadata needed by the mixer.
#[derive(Clone, Debug, Default)]
pub struct SampleBody {
    /// Raw signed 8-bit PCM. Empty if the header declared zero length.
    pub pcm: Vec<i8>,
    /// Loop start in samples (0 if sample does not loop).
    pub loop_start: u32,
    /// Loop length in samples (0 if sample does not loop — spec says
    /// repeat length of 2 also means "no loop").
    pub loop_length: u32,
    /// Default volume 0..=64.
    pub volume: u8,
    /// Finetune -8..=7.
    pub finetune: i8,
}

impl SampleBody {
    /// True if this sample has a valid loop region.
    pub fn is_looped(&self) -> bool {
        self.loop_length > 2
    }
}

impl crate::mixer::SampleSource for SampleBody {
    fn len(&self) -> usize {
        self.pcm.len()
    }
    fn loop_start(&self) -> usize {
        if self.is_looped() {
            self.loop_start as usize
        } else {
            0
        }
    }
    fn loop_end(&self) -> usize {
        if self.is_looped() {
            (self.loop_start + self.loop_length) as usize
        } else {
            self.pcm.len()
        }
    }
    fn loop_kind(&self) -> crate::mixer::LoopKind {
        if self.is_looped() {
            crate::mixer::LoopKind::Forward
        } else {
            crate::mixer::LoopKind::None
        }
    }
    fn at(&self, idx: usize) -> f32 {
        self.pcm.get(idx).copied().unwrap_or(0) as f32 / 128.0
    }
}

/// Extract all 31 sample bodies from the module bytes.
///
/// Samples declared longer than the remaining file are clamped to what's
/// actually there (many real-world rips are slightly truncated).
pub fn extract_samples(header: &ModHeader, bytes: &[u8]) -> Vec<SampleBody> {
    let mut out = Vec::with_capacity(header.samples.len());
    let mut cursor = header.sample_data_offset();
    let end = bytes.len();

    for sample in &header.samples {
        let declared = sample.length as usize;
        let available = end.saturating_sub(cursor);
        let take = declared.min(available);

        let pcm: Vec<i8> = if take == 0 {
            Vec::new()
        } else {
            // Reinterpret u8 as i8 (MOD samples are signed 8-bit).
            bytes[cursor..cursor + take]
                .iter()
                .map(|&b| b as i8)
                .collect()
        };

        cursor += take;

        // A loop_length of 0 or 2 means "no loop" per the ProTracker spec
        // (Protracker-effects-MODFIL12.txt §2.2 and Protracker-2.3A-misc-info.txt).
        // Real-world MOD rips occasionally have loop metadata that exceeds
        // the actual sample length; clamp to keep the mixer from reading
        // past the buffer.
        let (loop_start, loop_length) = if sample.repeat_length > 2 {
            let pcm_len = pcm.len() as u32;
            let start = sample.repeat_start.min(pcm_len);
            let len = sample.repeat_length.min(pcm_len.saturating_sub(start));
            if len > 2 {
                (start, len)
            } else {
                (0, 0)
            }
        } else {
            (0, 0)
        };

        // UST plays ONLY the loop area of a looped sample —
        // `Ultimate-Soundtracker-mod.txt` §"Notes on playing
        // repeat-samples": "Unlike PT only the loop-area is played …
        // Hence: Sample Start = Repeat Start, Sample Length = Repeat
        // Length. UST modules often (!) sound screwed if
        // repeat-samples are played incorrectly." Normalising here —
        // trimming the body to the loop region and rebasing the loop
        // to [0, len) — makes every trigger site conform at once: a
        // note-on starts the cursor at 0 (= the old repeat start) and
        // the mixer wraps at the region end, so the pre-loop head the
        // PT replayer would have played once is never emitted. The
        // 31-sample paths keep the PT "play start → repeat end, then
        // loop" behaviour untouched.
        let (pcm, loop_start, loop_length) = if header.is_ust() && loop_length > 2 {
            let s = loop_start as usize;
            let l = loop_length as usize;
            (pcm[s..s + l].to_vec(), 0, loop_length)
        } else {
            (pcm, loop_start, loop_length)
        };

        out.push(SampleBody {
            pcm,
            loop_start,
            loop_length,
            volume: sample.volume,
            finetune: sample.finetune,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::parse_header;

    fn build_minimal_mod_with_sample(pcm: &[i8]) -> Vec<u8> {
        let mut out = vec![0u8; crate::header::HEADER_FIXED_SIZE];
        // Title
        out[0..4].copy_from_slice(b"test");
        // Sample 0: length-in-words at offset 20 + 22..24.
        let len_words = (pcm.len() / 2) as u16;
        out[20 + 22..20 + 24].copy_from_slice(&len_words.to_be_bytes());
        // Volume.
        out[20 + 25] = 64;
        // Repeat start.
        out[20 + 26..20 + 28].copy_from_slice(&0u16.to_be_bytes());
        // Repeat length.
        out[20 + 28..20 + 30].copy_from_slice(&0u16.to_be_bytes());
        // Song length 1 pattern.
        out[950] = 1;
        out[951] = 0x7F;
        out[952] = 0; // order: pattern 0
        out[1080..1084].copy_from_slice(b"M.K.");
        // Pattern 0 — 64 rows × 4 channels × 4 bytes = 1024 bytes of zeros.
        out.extend(std::iter::repeat_n(0u8, 64 * 4 * 4));
        // Sample body.
        out.extend(pcm.iter().map(|&s| s as u8));
        out
    }

    #[test]
    fn extracts_signed_bytes() {
        let pcm = [10i8, -10, 40, -40, 127, -128];
        let bytes = build_minimal_mod_with_sample(&pcm);
        let header = parse_header(&bytes).unwrap();
        let samples = extract_samples(&header, &bytes);
        assert_eq!(samples.len(), 31);
        assert_eq!(samples[0].pcm, pcm);
        // Remaining samples empty.
        for s in &samples[1..] {
            assert!(s.pcm.is_empty());
        }
    }

    /// Minimal 15-sample UST module (`Ultimate-Soundtracker-mod.txt`
    /// layout): sample 1 carries `pcm` with the given repeat offset
    /// (BYTES, per the UST convention) and repeat length (words).
    fn build_minimal_ust_with_sample(
        pcm: &[i8],
        repeat_start_bytes: u16,
        repeat_len_words: u16,
    ) -> Vec<u8> {
        use crate::header::{UST_BPM_OFFSET, UST_HEADER_FIXED_SIZE, UST_SONG_LENGTH_OFFSET};
        let mut out = vec![0u8; UST_HEADER_FIXED_SIZE];
        out[0..4].copy_from_slice(b"ust!");
        let len_words = (pcm.len() / 2) as u16;
        out[20 + 22..20 + 24].copy_from_slice(&len_words.to_be_bytes());
        out[20 + 25] = 64;
        out[20 + 26..20 + 28].copy_from_slice(&repeat_start_bytes.to_be_bytes());
        out[20 + 28..20 + 30].copy_from_slice(&repeat_len_words.to_be_bytes());
        out[UST_SONG_LENGTH_OFFSET] = 1;
        out[UST_BPM_OFFSET] = 0x78;
        // order[0] = 0; pattern 0 all-zero.
        out.extend(std::iter::repeat_n(0u8, 64 * 4 * 4));
        out.extend(pcm.iter().map(|&s| s as u8));
        out
    }

    #[test]
    fn ust_looped_sample_body_is_trimmed_to_loop_area() {
        // `Ultimate-Soundtracker-mod.txt` §"Notes on playing
        // repeat-samples": "Unlike PT only the loop-area is played …
        // Sample Start = Repeat Start, Sample Length = Repeat Length."
        // Repeat offset 4 bytes, repeat length 2 words (= 4 samples):
        // the -100 head must be discarded entirely.
        let pcm = [-100i8, -100, -100, -100, 25, 26, 27, 28];
        let bytes = build_minimal_ust_with_sample(&pcm, 4, 2);
        let header = crate::header::parse_ust_header(&bytes).unwrap();
        let samples = extract_samples(&header, &bytes);
        assert_eq!(
            samples[0].pcm,
            [25, 26, 27, 28],
            "only the loop-area survives extraction for a looped UST sample"
        );
        assert_eq!(
            (samples[0].loop_start, samples[0].loop_length),
            (0, 4),
            "the loop region is rebased to cover the whole trimmed body"
        );
        assert!(samples[0].is_looped());
    }

    #[test]
    fn ust_one_shot_sample_body_is_untrimmed() {
        // Repeat length 0 words = loop off ("0=loop off, >1 loop on")
        // — a one-shot UST sample plays its whole body like PT does.
        let pcm = [1i8, 2, 3, 4, 5, 6, 7, 8];
        let bytes = build_minimal_ust_with_sample(&pcm, 0, 0);
        let header = crate::header::parse_ust_header(&bytes).unwrap();
        let samples = extract_samples(&header, &bytes);
        assert_eq!(samples[0].pcm, pcm);
        assert!(!samples[0].is_looped());
    }

    #[test]
    fn standard_mod_looped_sample_keeps_pre_loop_head() {
        // Control: the 31-sample path is PT territory — "PT plays from
        // Start to Repeat End and then loops between Repeat Start and
        // Repeat End" (same doc section) — so the head stays and the
        // loop fields remain absolute.
        let pcm = [-100i8, -100, -100, -100, 25, 26, 27, 28];
        let mut bytes = build_minimal_mod_with_sample(&pcm);
        // Repeat start 2 words (= 4 samples), repeat length 2 words.
        bytes[20 + 26..20 + 28].copy_from_slice(&2u16.to_be_bytes());
        bytes[20 + 28..20 + 30].copy_from_slice(&2u16.to_be_bytes());
        let header = parse_header(&bytes).unwrap();
        let samples = extract_samples(&header, &bytes);
        assert_eq!(samples[0].pcm, pcm, "PT keeps the one-shot head");
        assert_eq!((samples[0].loop_start, samples[0].loop_length), (4, 4));
    }

    #[test]
    fn handles_truncated_body() {
        let pcm = [1i8, 2, 3, 4];
        let mut bytes = build_minimal_mod_with_sample(&pcm);
        // Truncate by 2 bytes.
        bytes.truncate(bytes.len() - 2);
        let header = parse_header(&bytes).unwrap();
        let samples = extract_samples(&header, &bytes);
        assert_eq!(samples[0].pcm, [1, 2]);
    }
}
