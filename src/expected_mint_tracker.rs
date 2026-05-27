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
//! ## RD-913 — two bugs fixed here
//!
//! **Bug A (in-memory only).** Pre-fix this tracker was `RwLock<HashMap<...>>`.
//! A graceful shutdown during a pending claim's staleness window lost
//! the entry — the restart had no record that the CLAIM was still in
//! flight, so a never-landing MINT never escalated. Recoverable for
//! in-flight claims (the next `record_expected` after restart will be
//! made by the resubmission path), but the staleness window restarted
//! from zero. Post-fix the source of truth is `monitor_expected_mints`.
//!
//! **Bug B (StaleAlert fires forever).** Pre-fix `tick()` looped over
//! the map, pushed `Landed` entries to `to_remove`, but NEVER pushed
//! `StaleAlert` entries. The entry persisted in the map, so every
//! subsequent tick (every sync, ≈ every 6s) re-evaluated the same
//! entry past threshold and emitted another `bridge_expected_mint_stale_total`
//! increment. After 24h of pager fatigue on a single stuck claim, the
//! counter would be ~14400 and on-call would have learned to ignore the
//! metric — exactly the failure mode the metric exists to prevent.
//!
//! Post-fix: **one-shot StaleAlert.** When `ticks_pending >= stale_threshold_ticks`
//! the entry fires `StaleAlert` ONCE, the `alerted` flag is set, and the
//! entry is removed from the live map. The DB row is also deleted (so
//! restart doesn't replay the alert). This matches operator intent —
//! once on-call is paged, the entry's job is done; the operator either
//! resubmits (which creates a fresh tracker entry) or manually clears the
//! claim. Continued spamming serves nobody.

use lru::LruCache;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::store::Store;

/// 32-byte MINT NoteId.
pub type MintNoteId = [u8; 32];

/// 32-byte global index (claim identifier on the L1 side).
pub type GlobalIndex = [u8; 32];

/// Default in-memory cache capacity (entries).
/// Small (10k) because in-flight claims at any moment are O(hundreds);
/// the cache exists primarily to avoid hitting the DB on every tick.
pub const DEFAULT_CACHE_CAPACITY: usize = 10_000;

/// Tracks the expected MINT NoteId for each submitted claim and how
/// many sync ticks have elapsed since submission.
pub struct ExpectedMintTracker {
    cache: Mutex<LruCache<GlobalIndex, Entry>>,
    store: Arc<dyn Store>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    expected_mint: MintNoteId,
    /// Number of sync ticks elapsed since the claim was submitted but
    /// the expected MINT hasn't been observed on-chain yet.
    ticks_pending: u32,
    /// One-shot guard (RD-913 Bug B). Once a StaleAlert has fired for
    /// this entry, the next tick treats it as already-alerted and skips
    /// re-firing. The DB row is also deleted at the same time; this
    /// flag is the in-cache mirror used for crash-recovery: if we crash
    /// between firing the metric and deleting the row, the next load
    /// sees `alerted=true` and stays quiet.
    alerted: bool,
}

/// Verdict on a single claim's expected-MINT status, used to drive retry
/// or alert decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MintStatus {
    /// Expected MINT was observed on-chain. Drop the entry.
    Landed,
    /// Still within retry window — increment ticks and wait.
    Pending { ticks_pending: u32 },
    /// Exceeded retry threshold — page on-call. **Fires ONCE per entry**
    /// (RD-913 Bug B fix). After this, the entry is removed and no
    /// further alerts fire for this `global_index` until the next
    /// `record_expected` call.
    StaleAlert { ticks_pending: u32 },
}

impl ExpectedMintTracker {
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

    /// Register a claim's expected MINT NoteId. Called immediately after
    /// a successful CLAIM submission. Upserts: re-registering the same
    /// global_index resets the staleness window.
    pub async fn record_expected(
        &self,
        global_index: GlobalIndex,
        expected_mint: MintNoteId,
    ) -> anyhow::Result<()> {
        self.store
            .expected_mint_record(&global_index, &expected_mint)
            .await?;
        self.cache.lock().put(
            global_index,
            Entry {
                expected_mint,
                ticks_pending: 0,
                alerted: false,
            },
        );
        Ok(())
    }

    /// Update the tracker on each sync tick. `landed_mint_ids` is the set
    /// of CLAIM/MINT IDs observed consumed since the previous call. For
    /// each tracked claim:
    /// - if its expected MINT is in `landed_mint_ids`: drop (Landed)
    /// - else increment `ticks_pending`; if `>= stale_threshold_ticks`
    ///   AND we haven't already alerted, fire StaleAlert ONCE and drop
    ///   the entry; otherwise Pending.
    ///
    /// Returns a vector of `(global_index, status)` for every tracked
    /// claim, in stable order. Entries reported as `Landed` or
    /// `StaleAlert` are removed both from the cache and from the
    /// persistent store — `StaleAlert` is one-shot per `record_expected`.
    pub async fn tick(
        &self,
        landed_mint_ids: &HashSet<MintNoteId>,
        stale_threshold_ticks: u32,
    ) -> anyhow::Result<Vec<(GlobalIndex, MintStatus)>> {
        // Source of truth: load every live entry from the store. The
        // cache mirrors this, but we reload on every tick to handle:
        //   - cache evictions (a long-lived stale entry could have been
        //     pushed out of the cache by burstier observations),
        //   - restart recovery (cache starts empty on boot),
        //   - cross-process consistency (only one proxy is expected, but
        //     defensive).
        let rows = self.store.expected_mint_load_all().await?;

        let mut results = Vec::with_capacity(rows.len());
        let mut to_remove: Vec<GlobalIndex> = Vec::new();

        for (gi, expected_mint, ticks_pending, alerted) in rows {
            if landed_mint_ids.contains(&expected_mint) {
                results.push((gi, MintStatus::Landed));
                to_remove.push(gi);
                continue;
            }

            let next_ticks = ticks_pending.saturating_add(1);
            if next_ticks >= stale_threshold_ticks && !alerted {
                // RD-913 Bug B fix: fire StaleAlert ONCE per entry.
                // Push to to_remove AND mark alerted so the row is gone
                // before the next tick; the in-cache `alerted=true`
                // covers the crash-window between fire-and-delete.
                results.push((
                    gi,
                    MintStatus::StaleAlert {
                        ticks_pending: next_ticks,
                    },
                ));
                to_remove.push(gi);
            } else if alerted {
                // Already alerted previously (crash recovery path).
                // Don't re-fire; let the deletion below clear it.
                to_remove.push(gi);
            } else {
                // Still within retry window.
                self.store
                    .expected_mint_update_tick(&gi, next_ticks, false)
                    .await?;
                {
                    let mut cache = self.cache.lock();
                    cache.put(
                        gi,
                        Entry {
                            expected_mint,
                            ticks_pending: next_ticks,
                            alerted: false,
                        },
                    );
                }
                results.push((
                    gi,
                    MintStatus::Pending {
                        ticks_pending: next_ticks,
                    },
                ));
            }
        }

        for gi in &to_remove {
            self.store.expected_mint_remove(gi).await?;
            self.cache.lock().pop(gi);
        }

        // Stable order for deterministic test assertions.
        results.sort_by_key(|(gi, _)| *gi);
        Ok(results)
    }

    /// Count of currently-tracked (i.e. live in the store) entries.
    /// Reads from the store, not the cache — the store is authoritative.
    pub async fn pending_count(&self) -> anyhow::Result<usize> {
        Ok(self.store.expected_mint_load_all().await?.len())
    }

    /// Mark a tracked global_index as Landed without going through the tick
    /// path. Used by the claim path once `wait_for_transaction_commit`
    /// confirms the CLAIM tx was committed: from there the bridge's MINT
    /// emission is deterministic.
    pub async fn mark_landed(&self, global_index: GlobalIndex) -> anyhow::Result<()> {
        self.store.expected_mint_remove(&global_index).await?;
        self.cache.lock().pop(&global_index);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::InMemoryStore;

    fn store() -> Arc<dyn Store> {
        Arc::new(InMemoryStore::new())
    }

    /// Cantina #7 + RD-913 Bug B — repro+regression. The lifecycle is:
    /// tick 1 → Pending(1); tick 2 lands A (Landed, dropped); tick 3 B
    /// hits threshold → StaleAlert ONCE; tick 4 must NOT re-fire (Bug B
    /// would have produced a second StaleAlert here; post-fix the entry
    /// is gone and the result list is empty).
    #[tokio::test]
    async fn cantina_7_expected_mint_tracker_lifecycle() {
        let t = ExpectedMintTracker::new(store());
        let gi_a: GlobalIndex = [0xAAu8; 32];
        let gi_b: GlobalIndex = [0xBBu8; 32];
        let mint_a: MintNoteId = [0x11u8; 32];
        let mint_b: MintNoteId = [0x22u8; 32];

        t.record_expected(gi_a, mint_a).await.unwrap();
        t.record_expected(gi_b, mint_b).await.unwrap();
        assert_eq!(t.pending_count().await.unwrap(), 2);

        // Tick 1: nothing landed yet.
        let landed: HashSet<MintNoteId> = HashSet::new();
        let r = t.tick(&landed, 3).await.unwrap();
        assert_eq!(r.len(), 2);
        for (_, status) in &r {
            assert!(matches!(status, MintStatus::Pending { ticks_pending: 1 }));
        }

        // Tick 2: A's mint lands; B still pending.
        let mut landed = HashSet::new();
        landed.insert(mint_a);
        let r = t.tick(&landed, 3).await.unwrap();
        // Stable order: gi_a (0xAA) < gi_b (0xBB).
        assert_eq!(r[0], (gi_a, MintStatus::Landed));
        assert!(matches!(r[1], (g, MintStatus::Pending { ticks_pending: 2 }) if g == gi_b));
        // A is dropped from the store.
        assert_eq!(t.pending_count().await.unwrap(), 1);

        // Tick 3: B still pending, ticks_pending=3 hits threshold →
        // StaleAlert ONCE. Bug B fix: B is now removed too.
        let r = t.tick(&HashSet::new(), 3).await.unwrap();
        assert_eq!(r.len(), 1);
        assert!(matches!(r[0], (g, MintStatus::StaleAlert { ticks_pending: 3 }) if g == gi_b));
        assert_eq!(t.pending_count().await.unwrap(), 0);

        // Tick 4 (the bug-B regression check): the map is empty, NO
        // further StaleAlert fires for gi_b.
        let r = t.tick(&HashSet::new(), 3).await.unwrap();
        assert!(
            r.is_empty(),
            "expected no alerts after the one-shot StaleAlert; got {r:?}"
        );
    }

    /// A claim whose MINT lands on the FIRST tick (zero censorship) is
    /// reported as Landed, not Pending.
    #[tokio::test]
    async fn cantina_7_first_tick_landing() {
        let t = ExpectedMintTracker::new(store());
        let gi: GlobalIndex = [0x42u8; 32];
        let mint: MintNoteId = [0x99u8; 32];
        t.record_expected(gi, mint).await.unwrap();

        let mut landed = HashSet::new();
        landed.insert(mint);
        let r = t.tick(&landed, 5).await.unwrap();
        assert_eq!(r, vec![(gi, MintStatus::Landed)]);
        assert_eq!(t.pending_count().await.unwrap(), 0);
    }

    /// A claim never re-observed but threshold is `u32::MAX` stays
    /// Pending forever — no spurious StaleAlert.
    #[tokio::test]
    async fn cantina_7_high_threshold_stays_pending() {
        let t = ExpectedMintTracker::new(store());
        let gi: GlobalIndex = [0x42u8; 32];
        let mint: MintNoteId = [0x99u8; 32];
        t.record_expected(gi, mint).await.unwrap();

        for i in 1..10u32 {
            let r = t.tick(&HashSet::new(), u32::MAX).await.unwrap();
            assert!(
                matches!(r[0], (g, MintStatus::Pending { ticks_pending }) if g == gi && ticks_pending == i)
            );
        }
    }

    /// RD-913 Bug B — explicit StaleAlert one-shot test. Threshold=2, drive
    /// past it 10 times; assert exactly ONE StaleAlert, and no entries
    /// remain after.
    #[tokio::test]
    async fn rd913_stale_alert_fires_once_only() {
        let t = ExpectedMintTracker::new(store());
        let gi: GlobalIndex = [0xCCu8; 32];
        let mint: MintNoteId = [0xDDu8; 32];
        t.record_expected(gi, mint).await.unwrap();

        let mut stale_alerts = 0;
        for _ in 0..10 {
            let r = t.tick(&HashSet::new(), 2).await.unwrap();
            for (_, status) in &r {
                if matches!(status, MintStatus::StaleAlert { .. }) {
                    stale_alerts += 1;
                }
            }
        }
        assert_eq!(
            stale_alerts, 1,
            "StaleAlert must fire exactly once per record_expected (RD-913 Bug B)"
        );
        assert_eq!(t.pending_count().await.unwrap(), 0);
    }

    /// RD-913 Bug A — restart simulation. record_expected, drop tracker,
    /// re-instantiate, tick and confirm the entry is still tracked.
    /// Pre-fix the new tracker started with zero entries (in-memory map
    /// gone), so the in-flight claim's staleness window restarted.
    #[tokio::test]
    async fn rd913_restart_preserves_pending_entries() {
        let store: Arc<dyn Store> = store();
        let gi: GlobalIndex = [0xEEu8; 32];
        let mint: MintNoteId = [0xFFu8; 32];

        let t1 = ExpectedMintTracker::new(store.clone());
        t1.record_expected(gi, mint).await.unwrap();
        // Drive one tick so ticks_pending becomes 1.
        let r = t1.tick(&HashSet::new(), 5).await.unwrap();
        assert!(matches!(r[0].1, MintStatus::Pending { ticks_pending: 1 }));
        drop(t1);

        // Restart: brand-new tracker, cache empty.
        let t2 = ExpectedMintTracker::new(store.clone());
        assert_eq!(t2.pending_count().await.unwrap(), 1);

        // Continue ticking against the SAME threshold. Pre-restart we did
        // one tick; t2 picks up ticks_pending=1 from the DB and advances
        // to 2, then 3, etc.
        let r = t2.tick(&HashSet::new(), 5).await.unwrap();
        assert!(matches!(r[0].1, MintStatus::Pending { ticks_pending: 2 }));
        let r = t2.tick(&HashSet::new(), 5).await.unwrap();
        assert!(matches!(r[0].1, MintStatus::Pending { ticks_pending: 3 }));
        let r = t2.tick(&HashSet::new(), 5).await.unwrap();
        assert!(matches!(r[0].1, MintStatus::Pending { ticks_pending: 4 }));
        let r = t2.tick(&HashSet::new(), 5).await.unwrap();
        // ticks_pending becomes 5 → >= threshold → StaleAlert fires.
        assert!(matches!(
            r[0].1,
            MintStatus::StaleAlert { ticks_pending: 5 }
        ));
        assert_eq!(t2.pending_count().await.unwrap(), 0);
    }

    /// RD-913 — re-recording the same global_index resets the staleness
    /// window AND the alerted flag. Lets operators safely resubmit a
    /// previously-stalealertd claim without it being suppressed.
    #[tokio::test]
    async fn rd913_resubmission_resets_window() {
        let t = ExpectedMintTracker::new(store());
        let gi: GlobalIndex = [0x12u8; 32];
        let mint: MintNoteId = [0x34u8; 32];

        t.record_expected(gi, mint).await.unwrap();
        // Burn through to StaleAlert in a single tick (threshold=1 fires
        // immediately when ticks_pending becomes 1).
        let r = t.tick(&HashSet::new(), 1).await.unwrap();
        assert!(matches!(r[0].1, MintStatus::StaleAlert { .. }));
        assert_eq!(t.pending_count().await.unwrap(), 0);

        // Re-register (operator resubmitted the CLAIM after triage).
        t.record_expected(gi, mint).await.unwrap();
        // First tick is Pending(1), NOT StaleAlert — the resubmission
        // reset the window.
        let r = t.tick(&HashSet::new(), 3).await.unwrap();
        assert!(matches!(r[0].1, MintStatus::Pending { ticks_pending: 1 }));
    }
}
