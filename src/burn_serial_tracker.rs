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
//! serial, and on duplicate, page critical immediately.
//!
//! ## RD-913: persistence + bounded cache
//!
//! Pre-fix this tracker held a pure `HashSet<[u8;32]>` behind an
//! `RwLock`. A process restart cleared every previously-observed serial,
//! so the Cantina #5 detector reset to zero — a colliding second BURN
//! after restart looked fresh and was accepted. The set also grew without
//! bound: no cap, no eviction, no TTL.
//!
//! Post-fix the source of truth is the `monitor_burn_serials` postgres
//! table (see `migrations/006_monitor_state_persistence.sql`). The
//! `Store::burn_serial_observe` call returns `true` only on a NEW row, so
//! collisions survive restart: the second BURN with a previously-seen
//! serial hits the row and returns `false` (Duplicate).
//!
//! The in-process `LruCache` is a hot-path optimisation: most observations
//! are NEW (not the attack), and we want to skip the DB roundtrip for
//! serials we've already inserted in this lifetime. Cache misses fall
//! through to the store, which is authoritative. Eviction is safe — an
//! evicted entry still exists in the DB, so a later collision is still
//! detected. Default capacity is 100k entries; a 32-byte key + LRU
//! bookkeeping ≈ 80B per entry → ~8MB working set, well within any
//! reasonable pod memory budget.

use lru::LruCache;
use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::store::Store;

/// Default in-memory cache capacity (entries). Picked at 100k because:
/// 1. The DB is the source of truth — evictions don't lose data.
/// 2. At 80B/entry ≈ 8MB working set; cheap by container standards.
/// 3. The hot path (consecutive new observations during sync ticks)
///    rarely re-checks the same serial twice, so cache effectiveness
///    plateaus quickly. Going higher doesn't pay off.
pub const DEFAULT_CACHE_CAPACITY: usize = 100_000;

/// Tracks observed BURN note serial numbers and reports collisions.
/// See module docs for the persistence + caching design.
pub struct BurnSerialTracker {
    /// LRU cache of (serial → ()) — presence means "we've observed and
    /// persisted this serial in this process's lifetime". Wrapped in a
    /// `Mutex` because `LruCache::get` mutates the LRU order.
    cache: Mutex<LruCache<[u8; 32], ()>>,
    store: Arc<dyn Store>,
}

/// Outcome of a `record` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Serial was never observed before; tracker has stored it.
    New,
    /// Serial was already in the store — Cantina #5 collision signature.
    Duplicate,
}

impl BurnSerialTracker {
    /// Construct with the default cache capacity.
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self::with_capacity(store, DEFAULT_CACHE_CAPACITY)
    }

    /// Construct with an explicit cache capacity (`>= 1`).
    pub fn with_capacity(store: Arc<dyn Store>, capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("non-zero by construction");
        Self {
            cache: Mutex::new(LruCache::new(cap)),
            store,
        }
    }

    /// Record an observed BURN serial. Returns `Outcome::Duplicate` on a
    /// collision (caller alerts), `Outcome::New` on first observation.
    ///
    /// The DB row is the source of truth. The cache is consulted ONLY to
    /// short-circuit known-new serials we've already observed this run —
    /// a cache hit means "we previously inserted this and the DB has the
    /// row", so the next observation of the same serial is a Duplicate.
    /// A cache miss falls through to the store, which either inserts
    /// (returns `true` → New, populate cache) or hits the existing row
    /// (returns `false` → Duplicate).
    pub async fn record(&self, serial: [u8; 32]) -> anyhow::Result<Outcome> {
        // Cache hit: serial is already persisted from a previous call in
        // this process; this is the second observation → Duplicate.
        // `LruCache::get` updates recency, hence the Mutex.
        {
            let mut cache = self.cache.lock();
            if cache.get(&serial).is_some() {
                return Ok(Outcome::Duplicate);
            }
        }

        // Cache miss: ask the store. The store INSERT … ON CONFLICT path
        // is atomic; concurrent first observations from two threads have
        // exactly one INSERT win.
        let inserted = self.store.burn_serial_observe(&serial).await?;
        if inserted {
            // Newly inserted: cache it so subsequent observations short-
            // circuit to the cache-hit branch above.
            self.cache.lock().put(serial, ());
            Ok(Outcome::New)
        } else {
            // The row existed before our INSERT (either a previous-life
            // observation that survived restart, or a concurrent insert
            // from another worker). Either way: Duplicate.
            // Populate the cache so the next observation is a cheap hit.
            self.cache.lock().put(serial, ());
            Ok(Outcome::Duplicate)
        }
    }

    /// Distinct serials observed since startup (cache-resident only).
    /// Pre-RD-913 this returned the full set size; post-RD-913 the
    /// authoritative count lives in the DB (`SELECT COUNT(*) FROM
    /// monitor_burn_serials`). This method preserves the rough metric
    /// for sanity-checking in tests without an extra roundtrip.
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

    /// Cantina #5 — repro+regression. A reused B2AGG serial + same-asset
    /// produces two BURN notes with the same NoteId AND same nullifier.
    /// The tracker must:
    /// - return `New` on first observation
    /// - return `Duplicate` on the EXACT same serial seen again
    /// - keep distinct serials independent (no false-positive collisions)
    #[tokio::test]
    async fn cantina_5_burn_serial_tracker_detects_duplicate() {
        let t = BurnSerialTracker::new(store());
        let s1 = [0xAAu8; 32];
        let s2 = [0xBBu8; 32];

        assert_eq!(t.record(s1).await.unwrap(), Outcome::New);
        assert_eq!(t.cache_size(), 1);

        // Distinct serial — independent, also New.
        assert_eq!(t.record(s2).await.unwrap(), Outcome::New);
        assert_eq!(t.cache_size(), 2);

        // Re-observe s1 — Cantina #5 collision signature.
        assert_eq!(t.record(s1).await.unwrap(), Outcome::Duplicate);
        assert_eq!(t.cache_size(), 2);

        // Re-re-observe s1 — still Duplicate.
        assert_eq!(t.record(s1).await.unwrap(), Outcome::Duplicate);
        assert_eq!(t.cache_size(), 2);
    }

    /// Boundary: zero-bytes is a legitimate serial.
    #[tokio::test]
    async fn cantina_5_zero_serial_treated_like_any_other() {
        let t = BurnSerialTracker::new(store());
        assert_eq!(t.record([0u8; 32]).await.unwrap(), Outcome::New);
        assert_eq!(t.record([0u8; 32]).await.unwrap(), Outcome::Duplicate);
    }

    /// RD-913 Bug A — restart simulation. The tracker observes a serial,
    /// is dropped (the process exits), a NEW tracker is constructed against
    /// the SAME store (the pod restarted, postgres survives), and the
    /// previously-observed serial must be reported as Duplicate on its next
    /// observation. Pre-fix this returned `New` and silently let the
    /// Cantina #5 collision through.
    #[tokio::test]
    async fn rd913_restart_survives_observation() {
        let store: Arc<dyn Store> = store();
        let serial = [0x42u8; 32];

        // Pre-restart tracker observes the serial.
        let t1 = BurnSerialTracker::new(store.clone());
        assert_eq!(t1.record(serial).await.unwrap(), Outcome::New);
        drop(t1);

        // Restart: brand-new tracker, no warm cache. The store still has
        // the row from the previous tracker, so the next observation must
        // be Duplicate.
        let t2 = BurnSerialTracker::new(store.clone());
        // Empty cache by construction.
        assert_eq!(t2.cache_size(), 0);
        assert_eq!(t2.record(serial).await.unwrap(), Outcome::Duplicate);
        // After the store roundtrip the cache is populated.
        assert_eq!(t2.cache_size(), 1);
    }

    /// RD-913 — cache eviction safety. With a tiny capacity (1), the
    /// eviction policy must NOT cause a false `New` on re-observation:
    /// the evicted entry is still in the store, so the store roundtrip
    /// returns `false` (already exists) → Duplicate.
    #[tokio::test]
    async fn rd913_eviction_does_not_lose_collisions() {
        let store: Arc<dyn Store> = store();
        let t = BurnSerialTracker::with_capacity(store, 1);
        let s1 = [0x11u8; 32];
        let s2 = [0x22u8; 32];

        assert_eq!(t.record(s1).await.unwrap(), Outcome::New);
        // Observing s2 evicts s1 from the cache (capacity 1).
        assert_eq!(t.record(s2).await.unwrap(), Outcome::New);
        assert_eq!(t.cache_size(), 1);

        // Re-observe s1 — cache miss, but the DB row is still there.
        // MUST be Duplicate. A bug where the cache shadowed the store
        // would return New here and Cantina #5 would slip through.
        assert_eq!(t.record(s1).await.unwrap(), Outcome::Duplicate);
    }

    /// Concurrent observation of the same serial from many tasks must
    /// produce exactly one `New` and `n-1` `Duplicate`s — the store's
    /// INSERT … ON CONFLICT atomicity guarantees this regardless of cache
    /// race conditions.
    #[tokio::test]
    async fn cantina_5_tracker_serialises_concurrent_inserts() {
        let store: Arc<dyn Store> = store();
        let t = Arc::new(BurnSerialTracker::new(store));
        let n_tasks = 16;
        let serial = [0x42u8; 32];

        let handles: Vec<_> = (0..n_tasks)
            .map(|_| {
                let t = t.clone();
                tokio::spawn(async move { t.record(serial).await.unwrap() })
            })
            .collect();
        let mut outcomes = Vec::new();
        for h in handles {
            outcomes.push(h.await.unwrap());
        }

        let new_count = outcomes.iter().filter(|o| **o == Outcome::New).count();
        let dup_count = outcomes
            .iter()
            .filter(|o| **o == Outcome::Duplicate)
            .count();
        assert_eq!(new_count, 1, "exactly one task must record New");
        assert_eq!(dup_count, n_tasks - 1, "all others must observe Duplicate");
    }
}
