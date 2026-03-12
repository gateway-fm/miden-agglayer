use alloy::primitives::U256;
use parking_lot::RwLock;
use std::collections::HashSet;
use std::path::PathBuf;

pub struct ClaimTracker {
    claimed: RwLock<HashSet<U256>>,
    persistence_path: Option<PathBuf>,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<ClaimTracker>();

impl ClaimTracker {
    pub fn new(persistence_path: Option<PathBuf>) -> anyhow::Result<Self> {
        let claimed = if let Some(ref path) = persistence_path {
            if path.exists() {
                let data = std::fs::read_to_string(path)?;
                let indices: Vec<String> = serde_json::from_str(&data)?;
                let set: HashSet<U256> = indices
                    .into_iter()
                    .map(|s| U256::from_str_radix(s.trim_start_matches("0x"), 16))
                    .collect::<Result<_, _>>()?;
                set
            } else {
                HashSet::new()
            }
        } else {
            HashSet::new()
        };
        Ok(Self {
            claimed: RwLock::new(claimed),
            persistence_path,
        })
    }

    /// Atomically insert a global index, failing if already claimed. Persists on success.
    pub fn try_claim(&self, global_index: U256) -> anyhow::Result<()> {
        let mut claimed = self.claimed.write();
        if !claimed.insert(global_index) {
            anyhow::bail!("claim already submitted for global_index {global_index}");
        }
        drop(claimed);
        self.persist();
        Ok(())
    }

    /// Rollback a claim on submission failure. Persists after removal.
    pub fn unclaim(&self, global_index: &U256) {
        let mut claimed = self.claimed.write();
        claimed.remove(global_index);
        drop(claimed);
        self.persist();
    }

    pub fn is_claimed(&self, global_index: &U256) -> bool {
        self.claimed.read().contains(global_index)
    }

    fn persist(&self) {
        let Some(ref path) = self.persistence_path else {
            return;
        };
        let claimed = self.claimed.read();
        let indices: Vec<String> = claimed.iter().map(|i| format!("{i:#x}")).collect();
        let Ok(data) = serde_json::to_string_pretty(&indices) else {
            tracing::error!("ClaimTracker: failed to serialize claimed indices");
            return;
        };
        drop(claimed);

        let tmp_path = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp_path, &data) {
            tracing::error!("ClaimTracker: failed to write {}: {e}", tmp_path.display());
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, path) {
            tracing::error!("ClaimTracker: failed to rename to {}: {e}", path.display());
        }
    }
}

impl Default for ClaimTracker {
    fn default() -> Self {
        Self::new(None).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claim_and_reject_duplicate() {
        let tracker = ClaimTracker::new(None).unwrap();
        let idx = U256::from(42u64);
        assert!(!tracker.is_claimed(&idx));
        tracker.try_claim(idx).unwrap();
        assert!(tracker.is_claimed(&idx));
        assert!(tracker.try_claim(idx).is_err());
    }

    #[test]
    fn test_unclaim_allows_reclaim() {
        let tracker = ClaimTracker::new(None).unwrap();
        let idx = U256::from(99u64);
        tracker.try_claim(idx).unwrap();
        tracker.unclaim(&idx);
        assert!(!tracker.is_claimed(&idx));
        tracker.try_claim(idx).unwrap();
    }

    #[test]
    fn test_concurrent_claims() {
        use std::sync::Arc;
        let tracker = Arc::new(ClaimTracker::new(None).unwrap());
        let mut handles = vec![];
        for i in 0..100u64 {
            let t = tracker.clone();
            handles.push(std::thread::spawn(move || t.try_claim(U256::from(i))));
        }
        for h in handles {
            h.join().unwrap().unwrap();
        }
        // All 100 should be claimed
        for i in 0..100u64 {
            assert!(tracker.is_claimed(&U256::from(i)));
        }
    }

    #[test]
    fn test_persistence_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "claim_tracker_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("claimed_indices.json");

        {
            let tracker = ClaimTracker::new(Some(path.clone())).unwrap();
            tracker.try_claim(U256::from(1u64)).unwrap();
            tracker.try_claim(U256::from(2u64)).unwrap();
        }

        // Load from persisted file
        let tracker = ClaimTracker::new(Some(path.clone())).unwrap();
        assert!(tracker.is_claimed(&U256::from(1u64)));
        assert!(tracker.is_claimed(&U256::from(2u64)));
        assert!(!tracker.is_claimed(&U256::from(3u64)));

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
