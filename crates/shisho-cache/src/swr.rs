//! Stale-while-revalidate cache primitive (PRD Principle 2: query live,
//! cache briefly).
//!
//! Semantics, exactly:
//! - **fresh** (younger than the revalidate window): served as-is.
//! - **stale**: served immediately while a single background refresh runs
//!   (single-flight — concurrent stale hits never stack refreshes).
//! - **absent**: built inline, bounded by an overall budget (per-page
//!   timeouts are the loader's job, e.g. the HTTP client timeout). On
//!   inline-build failure the typed error is returned — fail loud, never
//!   guess.
//!
//! Every hit carries the wall-clock `as_of` of the data it returns, so tool
//! responses can phrase staleness honestly. Entries live in memory only;
//! a process restart costs a warm-up build, nothing more.

use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// Knobs for one cache, sourced from `[cache]` config.
#[derive(Debug, Clone, Copy)]
pub struct SwrConfig {
    /// Entries older than this are stale: still served, but revalidated in
    /// the background.
    pub revalidate_after: Duration,
    /// Overall wall-clock budget for one build (inline or background).
    pub build_budget: Duration,
}

/// How fresh the returned value was at serve time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    /// Served from a stale entry; a background refresh is running (or just
    /// failed and will be retried on the next stale hit).
    Stale,
}

/// A cache hit: the value plus the wall-clock time it was loaded.
#[derive(Debug, Clone)]
pub struct Cached<V> {
    pub value: V,
    pub as_of: DateTime<Utc>,
    pub freshness: Freshness,
}

/// A cold (absent-entry) build failed. Stale entries never surface this:
/// they serve the old value and retry in the background.
#[derive(Debug, thiserror::Error)]
pub enum SwrError<E> {
    #[error("cold cache build failed")]
    Load(#[source] E),
    #[error("cold cache build exceeded its {budget:?} budget")]
    Timeout { budget: Duration },
}

struct Entry<V> {
    value: V,
    as_of: DateTime<Utc>,
    fetched_at: Instant,
    /// Single-flight guard: true while a background refresh for this key is
    /// in flight.
    refreshing: Arc<AtomicBool>,
}

struct Inner<K, V> {
    entries: DashMap<K, Entry<V>>,
    /// Per-key locks so concurrent cold builds for the same key coalesce
    /// (the loser of the race finds the winner's entry on re-check).
    build_locks: DashMap<K, Arc<Mutex<()>>>,
    cfg: SwrConfig,
    /// Most recent successful load, for `/healthz` and `status()`.
    last_warm: RwLock<Option<DateTime<Utc>>>,
}

/// The SWR cache. Cheap to clone (shared interior); background refresh
/// tasks hold a clone.
pub struct SwrCache<K, V> {
    inner: Arc<Inner<K, V>>,
}

impl<K, V> Clone for SwrCache<K, V> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Snapshot of an entry's state, taken without holding the shard guard
/// across an await.
enum Lookup<V> {
    Fresh(Cached<V>),
    /// Stale hit. `claimed` carries the refresh flag iff this caller won
    /// the single-flight race and must spawn the refresh.
    Stale {
        hit: Cached<V>,
        claimed: Option<Arc<AtomicBool>>,
    },
    Absent,
}

impl<K, V> SwrCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn new(cfg: SwrConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                entries: DashMap::new(),
                build_locks: DashMap::new(),
                cfg,
                last_warm: RwLock::new(None),
            }),
        }
    }

    /// Look up `key`, applying the SWR state machine. `load` runs at most
    /// once per call: inline when the entry is absent, in the background
    /// when it is stale, not at all when it is fresh.
    pub async fn get_or_load<F, Fut, E>(&self, key: K, load: F) -> Result<Cached<V>, SwrError<E>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<V, E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        match self.lookup(&key) {
            Lookup::Fresh(hit) => Ok(hit),
            Lookup::Stale { hit, claimed } => {
                if let Some(refreshing) = claimed {
                    self.spawn_refresh(key, load, refreshing);
                }
                Ok(hit)
            }
            Lookup::Absent => self.build_inline(key, load).await,
        }
    }

    fn lookup(&self, key: &K) -> Lookup<V> {
        let Some(entry) = self.inner.entries.get(key) else {
            return Lookup::Absent;
        };
        let hit = Cached {
            value: entry.value.clone(),
            as_of: entry.as_of,
            freshness: Freshness::Fresh,
        };
        if entry.fetched_at.elapsed() < self.inner.cfg.revalidate_after {
            return Lookup::Fresh(hit);
        }
        // Stale: the first concurrent hit claims the refresh via CAS.
        let claimed = entry
            .refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then(|| Arc::clone(&entry.refreshing));
        Lookup::Stale {
            hit: Cached {
                freshness: Freshness::Stale,
                ..hit
            },
            claimed,
        }
    }

    fn spawn_refresh<F, Fut, E>(&self, key: K, load: F, refreshing: Arc<AtomicBool>)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<V, E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        let cache = self.clone();
        tokio::spawn(async move {
            let budget = cache.inner.cfg.build_budget;
            match tokio::time::timeout(budget, load()).await {
                // insert() replaces the entry (and its refresh flag) — no
                // need to clear the claimed one.
                Ok(Ok(value)) => {
                    cache.insert(key, value);
                }
                Ok(Err(err)) => {
                    tracing::warn!(error = %err, "SWR background refresh failed; serving stale until the next hit retries");
                    refreshing.store(false, Ordering::Release);
                }
                Err(_) => {
                    tracing::warn!(
                        ?budget,
                        "SWR background refresh exceeded its budget; serving stale until the next hit retries"
                    );
                    refreshing.store(false, Ordering::Release);
                }
            }
        });
    }

    async fn build_inline<F, Fut, E>(&self, key: K, load: F) -> Result<Cached<V>, SwrError<E>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<V, E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        let lock = self
            .inner
            .build_locks
            .entry(key.clone())
            .or_default()
            .clone();
        let _guard = lock.lock().await;

        // Re-check: a concurrent caller may have built this key while we
        // waited on the lock.
        match self.lookup(&key) {
            Lookup::Fresh(hit) | Lookup::Stale { hit, claimed: None } => return Ok(hit),
            Lookup::Stale {
                hit,
                claimed: Some(refreshing),
            } => {
                // Freshly built yet already stale: only possible with a
                // near-zero revalidate window. Don't refresh again here.
                refreshing.store(false, Ordering::Release);
                return Ok(hit);
            }
            Lookup::Absent => {}
        }

        let budget = self.inner.cfg.build_budget;
        match tokio::time::timeout(budget, load()).await {
            Ok(Ok(value)) => {
                let as_of = self.insert(key, value.clone());
                Ok(Cached {
                    value,
                    as_of,
                    freshness: Freshness::Fresh,
                })
            }
            Ok(Err(err)) => Err(SwrError::Load(err)),
            Err(_) => Err(SwrError::Timeout { budget }),
        }
    }

    /// The current entry for `key`, if any — no load, no refresh claim.
    /// For read-only surfaces that cannot rebuild the entry (the web UI's
    /// status page holds no backend credential).
    pub fn peek(&self, key: &K) -> Option<Cached<V>> {
        let entry = self.inner.entries.get(key)?;
        let freshness = if entry.fetched_at.elapsed() < self.inner.cfg.revalidate_after {
            Freshness::Fresh
        } else {
            Freshness::Stale
        };
        Some(Cached {
            value: entry.value.clone(),
            as_of: entry.as_of,
            freshness,
        })
    }

    /// Insert (or replace) an entry as freshly loaded now, returning the
    /// `as_of` it was stamped with. Also the re-hydration path for
    /// feed-driven invalidation.
    pub fn insert(&self, key: K, value: V) -> DateTime<Utc> {
        let now = Utc::now();
        self.inner.entries.insert(
            key,
            Entry {
                value,
                as_of: now,
                fetched_at: Instant::now(),
                refreshing: Arc::new(AtomicBool::new(false)),
            },
        );
        *self.inner.last_warm.write().expect("last_warm lock") = Some(now);
        now
    }

    /// Drop one entry; the next hit builds inline.
    pub fn invalidate(&self, key: &K) {
        self.inner.entries.remove(key);
    }

    /// Treat every current entry as fresh again without touching `as_of`
    /// (used when the Tsundoku feed confirms nothing changed).
    pub fn extend_all(&self) {
        let now = Instant::now();
        for mut entry in self.inner.entries.iter_mut() {
            entry.fetched_at = now;
        }
    }

    /// Keys currently held (the "warm" set for feed re-hydration).
    pub fn keys(&self) -> Vec<K> {
        self.inner.entries.iter().map(|e| e.key().clone()).collect()
    }

    /// Wall-clock time of the most recent successful load, for `/healthz`
    /// and `status()`.
    pub fn last_warm(&self) -> Option<DateTime<Utc>> {
        *self.inner.last_warm.read().expect("last_warm lock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn cache(revalidate_secs: u64, budget_secs: u64) -> SwrCache<String, String> {
        SwrCache::new(SwrConfig {
            revalidate_after: Duration::from_secs(revalidate_secs),
            build_budget: Duration::from_secs(budget_secs),
        })
    }

    /// A loader that counts invocations and returns "v{n}".
    fn counting_loader(
        calls: &Arc<AtomicUsize>,
    ) -> impl FnOnce() -> std::pin::Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
    + Send
    + 'static {
        let calls = Arc::clone(calls);
        move || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            Box::pin(async move { Ok(format!("v{n}")) })
        }
    }

    /// Let spawned background tasks run to completion on the current-thread
    /// test runtime.
    async fn drain_background() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn absent_entry_builds_inline_and_fresh_hit_skips_the_loader() {
        let cache = cache(60, 30);
        let calls = Arc::new(AtomicUsize::new(0));

        let first = cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .expect("cold build");
        assert_eq!(first.value, "v1");
        assert_eq!(first.freshness, Freshness::Fresh);
        assert!(cache.last_warm().is_some());

        tokio::time::advance(Duration::from_secs(30)).await;
        let second = cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .expect("fresh hit");
        assert_eq!(second.value, "v1");
        assert_eq!(second.freshness, Freshness::Fresh);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "fresh hit must not load");
    }

    #[tokio::test(start_paused = true)]
    async fn stale_entry_served_immediately_then_refreshed_in_background() {
        let cache = cache(60, 30);
        let calls = Arc::new(AtomicUsize::new(0));

        cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(61)).await;

        let stale = cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .expect("stale hit");
        assert_eq!(stale.value, "v1", "stale value served as-is");
        assert_eq!(stale.freshness, Freshness::Stale);

        drain_background().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one background refresh");

        let refreshed = cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .unwrap();
        assert_eq!(refreshed.value, "v2");
        assert_eq!(refreshed.freshness, Freshness::Fresh);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_stale_hits_trigger_a_single_refresh() {
        let cache = cache(60, 30);
        let calls = Arc::new(AtomicUsize::new(0));

        cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(61)).await;

        // Many concurrent stale hits, each offering a loader.
        let hits = futures_util::future::join_all(
            (0..10).map(|_| cache.get_or_load("k".to_owned(), counting_loader(&calls))),
        )
        .await;
        for hit in hits {
            let hit = hit.expect("stale hit");
            assert_eq!(hit.value, "v1");
            assert_eq!(hit.freshness, Freshness::Stale);
        }

        drain_background().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "exactly one background refresh despite 10 concurrent stale hits"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn inline_build_failure_fails_loud() {
        let cache: SwrCache<String, String> = cache(60, 30);
        let result = cache
            .get_or_load("k".into(), || async { Err("codex is down".to_owned()) })
            .await;
        match result {
            Err(SwrError::Load(e)) => assert_eq!(e, "codex is down"),
            other => panic!("expected Load error, got {other:?}"),
        }
        assert!(cache.last_warm().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn inline_build_is_bounded_by_the_budget() {
        let cache: SwrCache<String, String> = cache(60, 5);
        let result = cache
            .get_or_load("k".into(), || async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok::<_, String>("too late".to_owned())
            })
            .await;
        match result {
            Err(SwrError::Timeout { budget }) => assert_eq!(budget, Duration::from_secs(5)),
            other => panic!("expected Timeout error, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn background_refresh_failure_keeps_serving_stale_and_retries() {
        let cache = cache(60, 30);
        let calls = Arc::new(AtomicUsize::new(0));

        cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(61)).await;

        // Refresh fails: the stale value must survive.
        let hit = cache
            .get_or_load("k".to_owned(), || async {
                Err::<String, _>("transient".to_owned())
            })
            .await
            .unwrap();
        assert_eq!(hit.freshness, Freshness::Stale);
        drain_background().await;

        // The flag was released, so the next stale hit retries and wins.
        let hit = cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .unwrap();
        assert_eq!(hit.value, "v1");
        assert_eq!(hit.freshness, Freshness::Stale);
        drain_background().await;

        let refreshed = cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .unwrap();
        assert_eq!(refreshed.value, "v2");
        assert_eq!(refreshed.freshness, Freshness::Fresh);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_cold_builds_coalesce_per_key() {
        let cache = cache(60, 30);
        let calls = Arc::new(AtomicUsize::new(0));

        let slow_loader = || {
            let calls = Arc::clone(&calls);
            move || {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    Ok::<_, String>(format!("v{n}"))
                })
                    as std::pin::Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
            }
        };

        let (a, b) = tokio::join!(
            cache.get_or_load("k".to_owned(), slow_loader()),
            cache.get_or_load("k".to_owned(), slow_loader()),
        );
        assert_eq!(a.unwrap().value, "v1");
        assert_eq!(
            b.unwrap().value,
            "v1",
            "loser of the race reuses the winner's build"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn invalidate_forces_an_inline_rebuild() {
        let cache = cache(60, 30);
        let calls = Arc::new(AtomicUsize::new(0));

        cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .unwrap();
        cache.invalidate(&"k".to_owned());

        let rebuilt = cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .unwrap();
        assert_eq!(rebuilt.value, "v2");
        assert_eq!(rebuilt.freshness, Freshness::Fresh);
    }

    #[tokio::test(start_paused = true)]
    async fn extend_all_makes_stale_entries_fresh_without_reloading() {
        let cache = cache(60, 30);
        let calls = Arc::new(AtomicUsize::new(0));

        cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(61)).await;
        cache.extend_all();

        let hit = cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .unwrap();
        assert_eq!(hit.freshness, Freshness::Fresh);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no reload after extend");
    }

    #[tokio::test(start_paused = true)]
    async fn peek_reads_without_loading_or_claiming_a_refresh() {
        let cache = cache(60, 30);
        let calls = Arc::new(AtomicUsize::new(0));

        assert!(cache.peek(&"k".to_owned()).is_none(), "absent stays absent");

        cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .unwrap();
        let hit = cache.peek(&"k".to_owned()).expect("cached entry");
        assert_eq!(hit.value, "v1");
        assert_eq!(hit.freshness, Freshness::Fresh);

        // Past the revalidate window: peek reports staleness but must not
        // claim the refresh slot — the next stale get_or_load still does.
        tokio::time::advance(Duration::from_secs(61)).await;
        let stale = cache.peek(&"k".to_owned()).expect("cached entry");
        assert_eq!(stale.freshness, Freshness::Stale);
        drain_background().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "peek never loads");

        cache
            .get_or_load("k".into(), counting_loader(&calls))
            .await
            .unwrap();
        drain_background().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "refresh slot was free");
    }

    #[tokio::test(start_paused = true)]
    async fn insert_rehydrates_and_keys_lists_the_warm_set() {
        let cache = cache(60, 30);
        cache.insert("a".into(), "warm".into());
        assert_eq!(cache.keys(), vec!["a".to_owned()]);

        let calls = Arc::new(AtomicUsize::new(0));
        let hit = cache
            .get_or_load("a".into(), counting_loader(&calls))
            .await
            .unwrap();
        assert_eq!(hit.value, "warm");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
