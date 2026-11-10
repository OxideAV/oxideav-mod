//! One-finite-pass detection shared by the MOD / XM / IT players.
//!
//! Tracker songs routinely end by jumping back into themselves, and the
//! original trackers then play forever. A decoder has to stop, so each
//! player records the *flow state* it is in whenever it arrives on a
//! new row — the order position, the row and whatever else decides
//! where the song goes next (pattern-loop start rows and counters) —
//! and ends the song the first time a state comes round again. Keying
//! on the full flow state rather than on `(order, row)` alone is what
//! lets pattern loops revisit their rows without ending the song, and
//! still catches a loop that can never exit.
//!
//! States are stored as 128-bit digests. The number of distinct states
//! in one pass is capped; reaching the cap counts as a repeat, so even
//! a module built to nest loops astronomically deep yields a finite
//! pass.

use std::collections::HashSet;
use std::hash::Hasher;

/// Most distinct flow states one pass may hold (four times the largest
/// loop-free XM song: 256 orders of 256 rows).
pub const MAX_PASS_STATES: usize = 1 << 18;

/// Set of flow states visited in the current pass.
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct PassTracker {
    seen: HashSet<u128>,
    key: Vec<u8>,
}

impl PassTracker {
    /// Begin describing the state at `(order, row)`.
    pub fn start(&mut self, order: usize, row: usize) {
        self.key.clear();
        self.key.extend_from_slice(&(order as u32).to_le_bytes());
        self.key.extend_from_slice(&(row as u32).to_le_bytes());
    }

    /// Append further flow-relevant bytes to the state being described.
    pub fn push(&mut self, bytes: &[u8]) {
        self.key.extend_from_slice(bytes);
    }

    /// Record the described state. Returns `true` when it was already
    /// visited in this pass (or the pass has grown past
    /// [`MAX_PASS_STATES`]).
    pub fn finish(&mut self) -> bool {
        if self.seen.len() >= MAX_PASS_STATES {
            return true;
        }
        let mut a = std::collections::hash_map::DefaultHasher::new();
        a.write_u8(0x5A);
        a.write(&self.key);
        let mut b = std::collections::hash_map::DefaultHasher::new();
        b.write_u8(0xA5);
        b.write(&self.key);
        b.write_u64(self.key.len() as u64);
        let digest = ((a.finish() as u128) << 64) | b.finish() as u128;
        !self.seen.insert(digest)
    }

    /// Forget every visited state (a new pass begins).
    pub fn clear(&mut self) {
        self.seen.clear();
    }

    /// Number of states recorded in the current pass.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// True when no state has been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_repeats_only_when_every_byte_matches() {
        let mut t = PassTracker::default();
        t.start(0, 0);
        t.push(&[1, 2]);
        assert!(!t.finish());
        t.start(0, 0);
        t.push(&[1, 3]);
        assert!(!t.finish(), "different loop state: not a repeat");
        t.start(0, 1);
        t.push(&[1, 2]);
        assert!(!t.finish());
        t.start(0, 0);
        t.push(&[1, 2]);
        assert!(t.finish(), "same order, row and loop state");
        assert_eq!(t.len(), 3);
        t.clear();
        assert!(t.is_empty());
        t.start(0, 0);
        t.push(&[1, 2]);
        assert!(!t.finish(), "a cleared tracker starts a new pass");
    }
}
