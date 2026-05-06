//! BURN serial collision tracker — Cantina #5 monitor.
//!
//! Cantina #5 reports that the upstream `compute_burn_note_serial_num`
//! procedure derives the BURN note's serial from `(B2AGG_SERIAL_NUM,
//! ASSET_KEY)` only, omitting the leaf data. A caller that constructs a
//! valid B2AGG note directly with a reused upstream serial for the same
//! asset amount produces distinct exit leaves whose BURN notes COLLIDE
//! on `NoteId` and `nullifier`. Only the first BURN can be finalised; the
//! second wraps trapped wrapped tokens that the faucet never burns,
//! eventually exhausting `mint_and_send`'s `token_supply` headroom and
//! locking out all bridge-ins for that asset.
//!
//! The aggkit-side defense is detection: track every observed BURN note
//! serial, and on duplicate, page critical immediately. This module
//! provides the in-memory tracker (a `HashSet<[u8; 32]>` is the load-
//! bearing data structure) and the predicate that returns `true` on a
//! freshly-observed collision so the caller can branch on metrics + log.

use std::collections::HashSet;
use std::sync::RwLock;

/// Tracks observed BURN note serial numbers and reports collisions.
///
/// Self-review (Cantina #5 monitor) — on each sync tick, every BURN note
/// the bridge consumed is forwarded here via `record(serial)`. The first
/// time a serial is seen, the tracker stores it and returns `Outcome::New`.
/// On the second occurrence (the Cantina #5 collision signature), the
/// tracker returns `Outcome::Duplicate` and the caller MUST emit
/// `bridge_burn_serial_collision_total` and freeze further bridge-in
/// processing for the affected asset.
pub struct BurnSerialTracker {
    seen: RwLock<HashSet<[u8; 32]>>,
}

/// Outcome of a `record` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Serial was never observed before; tracker has stored it.
    New,
    /// Serial was already in the set — Cantina #5 collision signature.
    Duplicate,
}

impl Default for BurnSerialTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl BurnSerialTracker {
    pub fn new() -> Self {
        Self {
            seen: RwLock::new(HashSet::new()),
        }
    }

    /// Record an observed BURN serial. Returns `Outcome::Duplicate` on a
    /// collision (caller alerts), `Outcome::New` on first observation.
    pub fn record(&self, serial: [u8; 32]) -> Outcome {
        let mut set = self.seen.write().expect("BurnSerialTracker lock poisoned");
        if set.insert(serial) {
            Outcome::New
        } else {
            Outcome::Duplicate
        }
    }

    /// Total distinct serials observed since startup.
    pub fn distinct_count(&self) -> usize {
        self.seen.read().map(|s| s.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cantina #5 — repro+regression. A reused B2AGG serial + same-asset
    /// produces two BURN notes with the same NoteId AND same nullifier.
    /// The tracker must:
    /// - return `New` on first observation
    /// - return `Duplicate` on the EXACT same serial seen again (even
    ///   bytes-identical to a previously-seen entry)
    /// - keep distinct serials independent (no false-positive collisions)
    /// - survive concurrent writes without panicking (the RwLock alone is
    ///   not enough to guarantee this — `insert`'s contract is to short-
    ///   circuit so the hash lookup is the only race surface)
    #[test]
    fn cantina_5_burn_serial_tracker_detects_duplicate() {
        let t = BurnSerialTracker::new();
        let s1 = [0xAAu8; 32];
        let s2 = [0xBBu8; 32];

        assert_eq!(t.record(s1), Outcome::New);
        assert_eq!(t.distinct_count(), 1);

        // Distinct serial — independent, also New.
        assert_eq!(t.record(s2), Outcome::New);
        assert_eq!(t.distinct_count(), 2);

        // Re-observe s1 — Cantina #5 collision signature.
        assert_eq!(t.record(s1), Outcome::Duplicate);
        // Distinct count does NOT grow on duplicate.
        assert_eq!(t.distinct_count(), 2);

        // Re-re-observe s1 — still Duplicate.
        assert_eq!(t.record(s1), Outcome::Duplicate);
        assert_eq!(t.distinct_count(), 2);
    }

    /// Boundary: zero-bytes is a legitimate serial (it's just an opaque
    /// 32-byte value as far as our tracker is concerned), so observing
    /// `[0; 32]` twice must report a duplicate, not silently ignore.
    #[test]
    fn cantina_5_zero_serial_treated_like_any_other() {
        let t = BurnSerialTracker::new();
        assert_eq!(t.record([0u8; 32]), Outcome::New);
        assert_eq!(t.record([0u8; 32]), Outcome::Duplicate);
    }

    /// Concurrent observation of the same serial from two threads must
    /// produce exactly one `New` and at least one `Duplicate` — never two
    /// `New`s (which would indicate a TOCTOU race in the insert).
    #[test]
    fn cantina_5_tracker_serialises_concurrent_inserts() {
        use std::sync::Arc;
        use std::thread;

        let t = Arc::new(BurnSerialTracker::new());
        let n_threads = 16;
        let serial = [0x42u8; 32];

        let handles: Vec<_> = (0..n_threads)
            .map(|_| {
                let t = t.clone();
                thread::spawn(move || t.record(serial))
            })
            .collect();
        let outcomes: Vec<Outcome> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let new_count = outcomes.iter().filter(|o| **o == Outcome::New).count();
        let dup_count = outcomes
            .iter()
            .filter(|o| **o == Outcome::Duplicate)
            .count();
        assert_eq!(new_count, 1, "exactly one thread must record New");
        assert_eq!(
            dup_count,
            n_threads - 1,
            "all others must observe Duplicate"
        );
        assert_eq!(t.distinct_count(), 1);
    }
}
