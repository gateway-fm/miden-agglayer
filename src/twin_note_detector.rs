//! Twin-NoteId detector — Cantina #6 monitor.
//!
//! Cantina #6 reports that B2AGG reclaim authorises against
//! `NoteMetadata.sender`, but `NoteId` and `Nullifier` ignore metadata.
//! An attacker can clone a victim's public B2AGG note: keep the recipient
//! digest and assets identical (same NoteId, same nullifier), substitute
//! their own `metadata.sender`, and submit it. When the attacker reclaims
//! the twin, the shared nullifier is consumed and the victim's original
//! B2AGG can no longer be reclaimed OR consumed by the bridge — funds are
//! permanently stranded.
//!
//! The aggkit-side defense is detection: cross-index every observed note
//! by its `NoteId`, and on a second observation with a DIFFERENT
//! `note.commitment()` (i.e. different metadata), page critical and
//! freeze claim/bridge-out processing for the affected asset.
//!
//! ## RD-913: persistence + bounded cache
//!
//! Pre-fix this detector held a pure `HashMap<NoteIdBytes, Vec<NoteCommitment>>`
//! behind an `RwLock`. A process restart cleared every observation, so the
//! twin signature was undetectable across restart (the attacker simply
//! waits for the proxy to restart, then submits the twin).
//!
//! Post-fix the source of truth is the `monitor_twin_notes` postgres table
//! keyed by `(note_id, commitment)`. The `Store::twin_note_commitments`
//! lookup returns every commitment ever seen for a NoteId. The in-process
//! `LruCache` keyed on NoteId holds the recent observation set; on cache
//! miss we re-load from the store.
//!
//! Default cache capacity is 10k NoteIds (smaller than burn-serial's 100k
//! because each entry is a `Vec<[u8;32]>`, typically size 1, occasionally
//! 2 — total working set still under ~5MB).

use lru::LruCache;
use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::store::Store;

/// 32-byte NoteId (stable across the same recipient+asset_commitment pair).
pub type NoteIdBytes = [u8; 32];
/// 32-byte full note commitment (changes when metadata changes).
pub type NoteCommitment = [u8; 32];

/// Default in-memory cache capacity (entries, keyed by NoteId).
/// 10k chosen as a balance: legitimate distinct NoteIds per sync cycle
/// is small (~hundreds), the cache exists primarily to skip repeat
/// observations of the SAME NoteId during a sync rather than to hold
/// the whole observation history.
pub const DEFAULT_CACHE_CAPACITY: usize = 10_000;

/// Cross-index of observed notes by `NoteId` → list of distinct commitments.
pub struct TwinNoteDetector {
    cache: Mutex<LruCache<NoteIdBytes, Vec<NoteCommitment>>>,
    store: Arc<dyn Store>,
}

/// Outcome of a `record` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// First time this NoteId has been observed.
    New,
    /// Same NoteId, same commitment as a previous record — legitimate
    /// duplicate (e.g. re-sync, replay during restore). Not an alert.
    LegitimateDuplicate,
    /// Same NoteId but a NEW commitment — Cantina #6 twin signature.
    /// `prior_commitments` returns every previously-seen commitment for
    /// this NoteId so the operator can attribute the original vs the
    /// twin.
    TwinDetected {
        prior_commitments: Vec<NoteCommitment>,
    },
}

impl TwinNoteDetector {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self::with_capacity(store, DEFAULT_CACHE_CAPACITY)
    }

    pub fn with_capacity(store: Arc<dyn Store>, capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("non-zero by construction");
        Self {
            cache: Mutex::new(LruCache::new(cap)),
            store,
        }
    }

    /// Record an observed `(NoteId, commitment)` pair. Returns the outcome.
    ///
    /// Authoritative state lives in the store; the cache short-circuits
    /// hot-path repeat observations of the SAME NoteId. On a cache miss we
    /// load the full commitment list from the store (a NoteId we've never
    /// seen this lifetime might still have prior commitments from before
    /// the restart). The classification then runs against the materialized
    /// list, and the store is updated with the new pair (atomic ON
    /// CONFLICT DO NOTHING insert).
    pub async fn record(
        &self,
        note_id: NoteIdBytes,
        commitment: NoteCommitment,
    ) -> anyhow::Result<Outcome> {
        // Get the current commitment list — from cache if present, else
        // from the store.
        let prior: Vec<NoteCommitment> = {
            let mut cache = self.cache.lock();
            if let Some(existing) = cache.get(&note_id) {
                existing.clone()
            } else {
                // Cache miss → fall through to the store (released the
                // lock so the await doesn't hold a parking_lot guard).
                Vec::new()
            }
        };

        let prior = if prior.is_empty() {
            // Either truly novel OR cache-evicted; defer to the store.
            self.store.twin_note_commitments(&note_id).await?
        } else {
            prior
        };

        let outcome = if prior.is_empty() {
            Outcome::New
        } else if prior.contains(&commitment) {
            Outcome::LegitimateDuplicate
        } else {
            Outcome::TwinDetected {
                prior_commitments: prior.clone(),
            }
        };

        // Update the store. ON CONFLICT DO NOTHING handles the
        // LegitimateDuplicate case (no row inserted, returns false) and
        // the race where two workers race the same novel insertion.
        let inserted = self.store.twin_note_observe(&note_id, &commitment).await?;

        // Cache write-back: if we inserted (or already-known), keep the
        // cache view of `prior` consistent with the store.
        if inserted || !prior.contains(&commitment) {
            let mut next = prior.clone();
            if !next.contains(&commitment) {
                next.push(commitment);
            }
            self.cache.lock().put(note_id, next);
        } else {
            // Legitimate duplicate: ensure the cache contains the prior
            // list so subsequent same-NoteId observations are cheap.
            self.cache.lock().put(note_id, prior);
        }

        Ok(outcome)
    }

    /// Distinct NoteIds in the in-memory cache (NOT the authoritative
    /// count; that lives in the DB). Kept for test sanity-checks.
    pub fn cache_size(&self) -> usize {
        self.cache.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::InMemoryStore;

    fn store() -> Arc<dyn Store> {
        Arc::new(InMemoryStore::new())
    }

    /// Cantina #6 — repro+regression. The detector must distinguish three
    /// states cleanly:
    /// - First observation → `New`
    /// - Same NoteId + same commitment → `LegitimateDuplicate` (no alert)
    /// - Same NoteId + different commitment → `TwinDetected` with prior
    ///   commitments listed for attribution
    #[tokio::test]
    async fn cantina_6_twin_detector_three_states() {
        let d = TwinNoteDetector::new(store());
        let id_a = [0xAAu8; 32];
        let id_b = [0xBBu8; 32];
        let c1 = [0x11u8; 32];
        let c2 = [0x22u8; 32];

        // First observation of (id_a, c1) — New.
        assert_eq!(d.record(id_a, c1).await.unwrap(), Outcome::New);
        assert_eq!(d.cache_size(), 1);

        // Same (id_a, c1) again — LegitimateDuplicate (not a twin).
        assert_eq!(
            d.record(id_a, c1).await.unwrap(),
            Outcome::LegitimateDuplicate
        );
        assert_eq!(d.cache_size(), 1);

        // Different NoteId — independent New, no false positive.
        assert_eq!(d.record(id_b, c1).await.unwrap(), Outcome::New);

        // Same NoteId (id_a) + DIFFERENT commitment (c2) — TwinDetected.
        match d.record(id_a, c2).await.unwrap() {
            Outcome::TwinDetected { prior_commitments } => {
                assert_eq!(prior_commitments, vec![c1]);
            }
            other => panic!("expected TwinDetected, got {other:?}"),
        }
        assert_eq!(d.cache_size(), 2);

        // Re-observing the twin commitment c2 is now a LegitimateDuplicate
        // (we've seen it once already). The alert fires only on first sight.
        assert_eq!(
            d.record(id_a, c2).await.unwrap(),
            Outcome::LegitimateDuplicate
        );

        // A THIRD distinct commitment for id_a fires another TwinDetected
        // and reports BOTH prior commitments. Useful for attribution when
        // the attacker repeatedly clones.
        let c3 = [0x33u8; 32];
        match d.record(id_a, c3).await.unwrap() {
            Outcome::TwinDetected { prior_commitments } => {
                assert!(prior_commitments.contains(&c1));
                assert!(prior_commitments.contains(&c2));
                assert_eq!(prior_commitments.len(), 2);
            }
            other => panic!("expected TwinDetected with 2 priors, got {other:?}"),
        }
    }

    /// The detector must not panic on edge inputs.
    #[tokio::test]
    async fn cantina_6_zero_inputs_not_special() {
        let d = TwinNoteDetector::new(store());
        assert_eq!(d.record([0u8; 32], [0u8; 32]).await.unwrap(), Outcome::New);
        match d.record([0u8; 32], [1u8; 32]).await.unwrap() {
            Outcome::TwinDetected { .. } => {}
            other => panic!("expected TwinDetected, got {other:?}"),
        }
    }

    /// RD-913 Bug A — restart simulation. The detector observes a (NoteId,
    /// commitment) pair, is dropped, a new detector is constructed against
    /// the SAME store, and the previously-seen NoteId with a DIFFERENT
    /// commitment must be reported as `TwinDetected`. Pre-fix this returned
    /// `New` because the in-memory cross-index was cleared on restart.
    #[tokio::test]
    async fn rd913_restart_survives_observation() {
        let store: Arc<dyn Store> = store();
        let note_id = [0x42u8; 32];
        let commitment_a = [0x01u8; 32];
        let commitment_b = [0x02u8; 32];

        // Pre-restart detector observes the original pair.
        let d1 = TwinNoteDetector::new(store.clone());
        assert_eq!(
            d1.record(note_id, commitment_a).await.unwrap(),
            Outcome::New
        );
        drop(d1);

        // Restart: brand-new detector. The store still has the row; a
        // DIFFERENT commitment for the same NoteId must trigger the
        // Cantina #6 twin alert.
        let d2 = TwinNoteDetector::new(store.clone());
        assert_eq!(d2.cache_size(), 0);
        match d2.record(note_id, commitment_b).await.unwrap() {
            Outcome::TwinDetected { prior_commitments } => {
                assert_eq!(prior_commitments, vec![commitment_a]);
            }
            other => panic!("expected TwinDetected, got {other:?}"),
        }
    }

    /// RD-913 — cache eviction safety. With capacity 1, observing a second
    /// NoteId evicts the first from the cache. Re-observing the first with
    /// a different commitment must still fire `TwinDetected` because the
    /// store reload picks up the persisted commitment.
    #[tokio::test]
    async fn rd913_eviction_does_not_lose_twin_signal() {
        let store: Arc<dyn Store> = store();
        let d = TwinNoteDetector::with_capacity(store, 1);
        let id_a = [0xAAu8; 32];
        let id_b = [0xBBu8; 32];
        let c1 = [0x11u8; 32];
        let c2 = [0x22u8; 32];

        assert_eq!(d.record(id_a, c1).await.unwrap(), Outcome::New);
        assert_eq!(d.record(id_b, c1).await.unwrap(), Outcome::New);
        // id_a should be evicted from the cache by now.
        match d.record(id_a, c2).await.unwrap() {
            Outcome::TwinDetected { prior_commitments } => {
                assert_eq!(prior_commitments, vec![c1]);
            }
            other => panic!("expected TwinDetected after eviction, got {other:?}"),
        }
    }
}
