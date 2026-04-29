//! Expected-MINT-NoteId tracker — Cantina #7 monitor.
//!
//! Cantina #7 reports that the MINT note's NoteId is fully derivable from
//! public claim data (the bridge_in MASM derives serial from
//! `CLAIM_PROOF_DATA_KEY`, which is itself public). An attacker watching
//! pending claims can pre-compute the expected MINT NoteId and submit a
//! metadata-distinct twin first; batch dedup (keyed on NoteId) discards
//! the legitimate claim's MINT, censoring the user.
//!
//! The aggkit-side defense (per CANTINA_FIXES.md) is to:
//! 1. Track each submitted claim's expected MINT NoteId locally.
//! 2. Periodically check that the expected MINT lands on-chain.
//! 3. If it doesn't appear within N blocks, retry with backoff; after K
//!    retries, page on-call.
//!
//! This module exposes the in-memory tracker and the staleness predicate.
//! The wiring (deriving expected NoteIds from CLAIM data + checking the
//! Miden node for the note + retry orchestration) is a separate commit.

use std::collections::HashMap;
use std::sync::RwLock;

/// 32-byte MINT NoteId.
pub type MintNoteId = [u8; 32];

/// 32-byte global index (claim identifier on the L1 side).
pub type GlobalIndex = [u8; 32];

/// Tracks the expected MINT NoteId for each submitted claim and how
/// many sync ticks have elapsed since submission.
pub struct ExpectedMintTracker {
    inner: RwLock<HashMap<GlobalIndex, Entry>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    expected_mint: MintNoteId,
    /// Number of sync ticks elapsed since the claim was submitted but
    /// the expected MINT hasn't been observed on-chain yet.
    ticks_pending: u32,
}

/// Verdict on a single claim's expected-MINT status, used to drive retry
/// or alert decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MintStatus {
    /// Expected MINT was observed on-chain. Drop the entry.
    Landed,
    /// Still within retry window — increment ticks and wait.
    Pending { ticks_pending: u32 },
    /// Exceeded retry threshold — page on-call.
    StaleAlert { ticks_pending: u32 },
}

impl Default for ExpectedMintTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ExpectedMintTracker {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Register a claim's expected MINT NoteId. Called immediately after
    /// a successful CLAIM submission.
    pub fn record_expected(&self, global_index: GlobalIndex, expected_mint: MintNoteId) {
        let mut map = self.inner.write().expect("ExpectedMintTracker lock poisoned");
        map.insert(
            global_index,
            Entry {
                expected_mint,
                ticks_pending: 0,
            },
        );
    }

    /// Update the tracker on each sync tick. `landed_mint_ids` is the set
    /// of MINT NoteIds observed on-chain since the previous call. For
    /// each tracked claim:
    /// - if its expected MINT is in `landed_mint_ids`: drop (Landed)
    /// - else increment `ticks_pending`; if over `stale_threshold_ticks`,
    ///   return StaleAlert; otherwise Pending.
    ///
    /// Returns a vector of `(global_index, status)` for every tracked
    /// claim, in stable order, so the caller can drive retry / alert.
    pub fn tick(
        &self,
        landed_mint_ids: &std::collections::HashSet<MintNoteId>,
        stale_threshold_ticks: u32,
    ) -> Vec<(GlobalIndex, MintStatus)> {
        let mut map = self.inner.write().expect("ExpectedMintTracker lock poisoned");
        let mut results = Vec::with_capacity(map.len());
        let mut to_remove = Vec::new();

        for (gi, entry) in map.iter_mut() {
            if landed_mint_ids.contains(&entry.expected_mint) {
                results.push((*gi, MintStatus::Landed));
                to_remove.push(*gi);
            } else {
                entry.ticks_pending = entry.ticks_pending.saturating_add(1);
                if entry.ticks_pending >= stale_threshold_ticks {
                    results.push((*gi, MintStatus::StaleAlert {
                        ticks_pending: entry.ticks_pending,
                    }));
                } else {
                    results.push((*gi, MintStatus::Pending {
                        ticks_pending: entry.ticks_pending,
                    }));
                }
            }
        }

        for gi in to_remove {
            map.remove(&gi);
        }

        // Stable order for deterministic test assertions.
        results.sort_by_key(|(gi, _)| *gi);
        results
    }

    pub fn pending_count(&self) -> usize {
        self.inner.read().map(|m| m.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Cantina #7 — repro+regression. The tracker drives the retry/alert
    /// state machine for expected MINTs. This test exercises the full
    /// lifecycle of two claims: one lands quickly (Landed), one censored
    /// past the threshold (StaleAlert).
    #[test]
    fn cantina_7_expected_mint_tracker_lifecycle() {
        let t = ExpectedMintTracker::new();
        let gi_a: GlobalIndex = [0xAAu8; 32];
        let gi_b: GlobalIndex = [0xBBu8; 32];
        let mint_a: MintNoteId = [0x11u8; 32];
        let mint_b: MintNoteId = [0x22u8; 32];

        t.record_expected(gi_a, mint_a);
        t.record_expected(gi_b, mint_b);
        assert_eq!(t.pending_count(), 2);

        // Tick 1: nothing landed yet.
        let landed: HashSet<MintNoteId> = HashSet::new();
        let r = t.tick(&landed, 3);
        assert_eq!(r.len(), 2);
        for (_, status) in &r {
            assert!(matches!(status, MintStatus::Pending { ticks_pending: 1 }));
        }

        // Tick 2: A's mint lands; B still pending.
        let mut landed = HashSet::new();
        landed.insert(mint_a);
        let r = t.tick(&landed, 3);
        // Stable order: gi_a (0xAA) < gi_b (0xBB).
        assert_eq!(r[0], (gi_a, MintStatus::Landed));
        assert!(matches!(r[1], (g, MintStatus::Pending { ticks_pending: 2 }) if g == gi_b));
        // A is dropped from the map.
        assert_eq!(t.pending_count(), 1);

        // Tick 3: B still pending, ticks_pending=3 hits threshold → StaleAlert.
        let r = t.tick(&HashSet::new(), 3);
        assert_eq!(r.len(), 1);
        assert!(matches!(r[0], (g, MintStatus::StaleAlert { ticks_pending: 3 }) if g == gi_b));
        // B remains in map (we stay pinging until landed or operator acks).
        assert_eq!(t.pending_count(), 1);

        // Tick 4: B finally lands.
        let mut landed = HashSet::new();
        landed.insert(mint_b);
        let r = t.tick(&landed, 3);
        assert_eq!(r[0], (gi_b, MintStatus::Landed));
        assert_eq!(t.pending_count(), 0);
    }

    /// A claim whose MINT lands on the FIRST tick (zero censorship) is
    /// reported as Landed, not Pending. This rules out off-by-one in the
    /// landed-set check.
    #[test]
    fn cantina_7_first_tick_landing() {
        let t = ExpectedMintTracker::new();
        let gi: GlobalIndex = [0x42u8; 32];
        let mint: MintNoteId = [0x99u8; 32];
        t.record_expected(gi, mint);

        let mut landed = HashSet::new();
        landed.insert(mint);
        let r = t.tick(&landed, 5);
        assert_eq!(r, vec![(gi, MintStatus::Landed)]);
        assert_eq!(t.pending_count(), 0);
    }

    /// A claim that's never re-observed but the threshold is `u32::MAX`
    /// stays Pending forever — no spurious StaleAlert.
    #[test]
    fn cantina_7_high_threshold_stays_pending() {
        let t = ExpectedMintTracker::new();
        let gi: GlobalIndex = [0x42u8; 32];
        let mint: MintNoteId = [0x99u8; 32];
        t.record_expected(gi, mint);

        for i in 1..10 {
            let r = t.tick(&HashSet::new(), u32::MAX);
            assert!(matches!(r[0], (g, MintStatus::Pending { ticks_pending }) if g == gi && ticks_pending == i));
        }
    }
}
