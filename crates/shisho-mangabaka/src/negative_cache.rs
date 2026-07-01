//! In-memory negative cache for "known-miss" lookups.
//!
//! `(scope, id)` pairs that resolved to `Ok(None)` are recorded here so
//! subsequent calls within `ttl` skip the offline store and the network.
//! The cache is bounded: at `max_entries` the oldest half is dropped
//! rather than wandering off into unbounded growth.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

pub struct NegativeCache {
    inner: RwLock<HashMap<(String, String), Instant>>,
    ttl: Duration,
    max_entries: usize,
}

impl NegativeCache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            ttl,
            max_entries,
        }
    }

    /// `true` if a non-expired miss is recorded for `(scope, id)`.
    /// Returns `false` if missing or expired (and removes the expired entry).
    pub async fn is_known_miss(&self, scope: &str, id: &str) -> bool {
        let key = (scope.to_string(), id.to_string());
        {
            let guard = self.inner.read().await;
            if let Some(recorded_at) = guard.get(&key)
                && recorded_at.elapsed() < self.ttl
            {
                return true;
            }
        }
        // Either no entry or it's stale; drop the stale one if present.
        let mut guard = self.inner.write().await;
        if let Some(recorded_at) = guard.get(&key)
            && recorded_at.elapsed() >= self.ttl
        {
            guard.remove(&key);
        }
        false
    }

    /// Record a miss. If we'd exceed `max_entries`, evict the oldest half.
    pub async fn record_miss(&self, scope: &str, id: &str) {
        let mut guard = self.inner.write().await;
        if guard.len() >= self.max_entries {
            // Simple bounded eviction: keep the newest half. Costs O(n log n)
            // but only runs at the cap boundary.
            let mut entries: Vec<((String, String), Instant)> = guard.drain().collect();
            entries.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
            entries.truncate(self.max_entries / 2);
            for (k, v) in entries {
                guard.insert(k, v);
            }
        }
        guard.insert((scope.to_string(), id.to_string()), Instant::now());
    }

    /// Remove an entry, e.g. after a refresh discovers a series that was
    /// previously cached as a miss.
    pub async fn forget(&self, scope: &str, id: &str) {
        let mut guard = self.inner.write().await;
        guard.remove(&(scope.to_string(), id.to_string()));
    }

    /// Clear every entry (a fresh dump invalidates all recorded misses).
    pub async fn clear(&self) {
        self.inner.write().await.clear();
    }

    /// Current entry count, for status surfaces.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn records_and_returns_hit() {
        let cache = NegativeCache::new(Duration::from_secs(60), 100);
        assert!(!cache.is_known_miss("mangabaka", "1").await);
        cache.record_miss("mangabaka", "1").await;
        assert!(cache.is_known_miss("mangabaka", "1").await);
        assert_eq!(cache.len().await, 1);
    }

    #[tokio::test]
    async fn expired_entries_are_purged_on_lookup() {
        let cache = NegativeCache::new(Duration::from_millis(20), 100);
        cache.record_miss("mangabaka", "1").await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(!cache.is_known_miss("mangabaka", "1").await);
        // And the underlying entry is gone.
        assert!(cache.is_empty().await);
    }

    #[tokio::test]
    async fn forget_removes_specific_entry() {
        let cache = NegativeCache::new(Duration::from_secs(60), 100);
        cache.record_miss("mangabaka", "1").await;
        cache.record_miss("mangabaka", "2").await;
        cache.forget("mangabaka", "1").await;
        assert!(!cache.is_known_miss("mangabaka", "1").await);
        assert!(cache.is_known_miss("mangabaka", "2").await);
    }

    #[tokio::test]
    async fn clear_empties_the_cache() {
        let cache = NegativeCache::new(Duration::from_secs(60), 100);
        cache.record_miss("mangabaka", "1").await;
        cache.record_miss("mangabaka", "mal:2").await;
        cache.clear().await;
        assert!(cache.is_empty().await);
        assert!(!cache.is_known_miss("mangabaka", "1").await);
    }

    #[tokio::test]
    async fn bounded_growth_evicts_oldest_half_at_cap() {
        let cache = NegativeCache::new(Duration::from_secs(60), 4);
        cache.record_miss("p", "1").await;
        cache.record_miss("p", "2").await;
        cache.record_miss("p", "3").await;
        cache.record_miss("p", "4").await;
        // The fifth insert triggers eviction down to max/2 = 2 entries.
        cache.record_miss("p", "5").await;
        let len = cache.len().await;
        assert!(len <= 4, "len after eviction should be <= max, got {len}");
        assert!(cache.is_known_miss("p", "5").await, "newest entry survives");
    }
}
