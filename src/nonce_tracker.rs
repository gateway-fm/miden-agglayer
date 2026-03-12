use parking_lot::RwLock;
use std::collections::HashMap;

pub struct NonceTracker {
    nonces: RwLock<HashMap<String, u64>>,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<NonceTracker>();

impl NonceTracker {
    pub fn new() -> Self {
        Self {
            nonces: RwLock::new(HashMap::new()),
        }
    }

    pub fn get(&self, address: &str) -> u64 {
        *self.nonces.read().get(&address.to_lowercase()).unwrap_or(&0)
    }

    /// Increment nonce, returning the value before increment.
    pub fn increment(&self, address: &str) -> u64 {
        let key = address.to_lowercase();
        let mut nonces = self.nonces.write();
        let nonce = nonces.entry(key).or_insert(0);
        let prev = *nonce;
        *nonce += 1;
        prev
    }
}

impl Default for NonceTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_default() {
        let tracker = NonceTracker::new();
        assert_eq!(tracker.get("0xabc"), 0);
    }

    #[test]
    fn test_increment_returns_previous() {
        let tracker = NonceTracker::new();
        assert_eq!(tracker.increment("0xABC"), 0);
        assert_eq!(tracker.increment("0xABC"), 1);
        assert_eq!(tracker.increment("0xABC"), 2);
        assert_eq!(tracker.get("0xabc"), 3);
    }

    #[test]
    fn test_case_insensitive() {
        let tracker = NonceTracker::new();
        tracker.increment("0xABC");
        assert_eq!(tracker.get("0xabc"), 1);
        assert_eq!(tracker.get("0xABC"), 1);
    }

    #[test]
    fn test_independent_addresses() {
        let tracker = NonceTracker::new();
        tracker.increment("0xaaa");
        tracker.increment("0xaaa");
        tracker.increment("0xbbb");
        assert_eq!(tracker.get("0xaaa"), 2);
        assert_eq!(tracker.get("0xbbb"), 1);
    }
}
