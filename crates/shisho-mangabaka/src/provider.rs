//! The backbone facade: negative cache → offline dump store → API
//! fallback, plus `refresh_cache()` orchestrating the dump pipeline with
//! an atomic promote.
//!
//! Lookup chain for `get`, `resolve_by_foreign_id`, and `search`:
//!
//! 1. **Negative cache** (in-memory). Recent known-misses short-circuit
//!    without touching the offline store or the network.
//! 2. **Offline store** (if a dump is loaded). Backed by the extracted
//!    MangaBaka dump under `<storage dir>/mangabaka/series.sqlite`.
//! 3. **Live API fallback** (only when `api_fallback` and an API key are
//!    configured). A confirmed miss records a negative entry.
//!
//! `refresh_cache` is serialized: a fire that overlaps a running refresh
//! is skipped with a warning rather than queued (the next scheduled fire
//! picks up the new dump anyway).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use shisho_ids::Provider;
use tokio::sync::{Mutex, RwLock};

use crate::PROVIDER_ID;
use crate::client::MangabakaClient;
use crate::error::MangabakaError;
use crate::mapping::{provider_to_mb_source, series_to_metadata, series_to_search_hit};
use crate::metrics::instruments;
use crate::negative_cache::NegativeCache;
use crate::offline::{self, OfflineStore};
use crate::types::{RefreshOutcome, RefreshSummary, SearchHit, SeriesMetadata};
use shisho_config::MangabakaConfig;

/// Negative-cache capacity: bounded so a pathological caller cannot grow
/// it without limit; eviction drops the oldest half at the cap.
const NEGATIVE_CACHE_MAX_ENTRIES: usize = 10_000;

/// Wall-clock budget for one dump download + ingest (the dump is ~500 MB
/// compressed; minutes on a slow link).
const REFRESH_TIMEOUT: Duration = Duration::from_secs(60 * 30);

/// The loaded dump and the row count taken at open time (so status
/// surfaces don't re-count on every probe).
struct LoadedDump {
    store: Arc<OfflineStore>,
    series_rows: u64,
}

/// What the last refresh attempt did, for status surfaces.
#[derive(Debug, Clone)]
pub struct RefreshAttempt {
    pub at: DateTime<Utc>,
    /// `Ok(rows)` on success, `Err(message)` on failure.
    pub outcome: Result<u64, String>,
}

/// Backbone state for `status()`, `/healthz`, and the Status page.
#[derive(Debug, Clone)]
pub struct BackboneStatus {
    pub dump_loaded: bool,
    pub series_rows: Option<u64>,
    /// Modification time of the live dump file, if one exists on disk.
    pub dump_modified_at: Option<DateTime<Utc>>,
    pub last_refresh: Option<RefreshAttempt>,
    /// A wholesale refresh is running right now (download + ingest).
    pub refresh_running: bool,
    pub negative_cache_size: usize,
    pub api_fallback: bool,
}

pub struct MangabakaProvider {
    client: MangabakaClient,
    /// True iff `api_fallback` is configured *and* an API key exists
    /// (validated at boot; double-checked here so a permissive test
    /// construction degrades to dump-only instead of 401-ing).
    api_fallback: bool,
    /// Loaded by [`Self::load_existing_dump`] or after a successful
    /// `refresh_cache`. `None` = no offline path available;
    /// reads fall through to the API (or return `Ok(None)`).
    offline: RwLock<Option<LoadedDump>>,
    /// `<storage dir>/mangabaka/`. Owned by this provider.
    dump_dir: PathBuf,
    /// `None` = API-only mode; `refresh_cache` reports `NotConfigured`.
    dump_url: Option<String>,
    negative_cache: NegativeCache,
    http_for_refresh: reqwest::Client,
    refresh_lock: Mutex<()>,
    last_refresh: RwLock<Option<RefreshAttempt>>,
}

impl MangabakaProvider {
    /// Construct from the typed config block and the dump directory
    /// (`StorageConfig::mangabaka_dir()`). Deliberately permissive about
    /// config combinations — [`MangabakaConfig::validate`] is the boot
    /// gate — but `api_fallback` is only honored when a key exists.
    ///
    /// The store starts unloaded; call [`Self::load_existing_dump`] to
    /// pick up a dump already on disk (boot does this before serving).
    pub fn new(cfg: &MangabakaConfig, dump_dir: PathBuf) -> Result<Self, MangabakaError> {
        let api_key = (!cfg.api_key.is_empty()).then(|| cfg.api_key.expose().to_owned());
        let api_fallback = cfg.api_fallback && api_key.is_some();
        let client = MangabakaClient::new(cfg.api_base_url.clone(), api_key, cfg.timeout())?;

        // Long timeout for the refresh client: the dump download can take
        // several minutes on a slow link.
        let http_for_refresh = reqwest::Client::builder()
            .user_agent(concat!("shisho/", env!("CARGO_PKG_VERSION")))
            .timeout(REFRESH_TIMEOUT)
            .build()
            .map_err(|e| MangabakaError::NotConfigured {
                message: format!("building refresh http client: {e}"),
            })?;

        Ok(Self {
            client,
            api_fallback,
            offline: RwLock::new(None),
            dump_dir,
            dump_url: cfg.dump_url().map(str::to_owned),
            negative_cache: NegativeCache::new(
                cfg.negative_cache_ttl(),
                NEGATIVE_CACHE_MAX_ENTRIES,
            ),
            http_for_refresh,
            refresh_lock: Mutex::new(()),
            last_refresh: RwLock::new(None),
        })
    }

    /// Open the dump already on disk, if any. `true` when a dump is
    /// serving afterwards. A missing file is normal (fresh deployment:
    /// API-only until the first refresh); an unreadable one is logged and
    /// retried on the next refresh.
    pub async fn load_existing_dump(&self) -> bool {
        let dump = offline::dump_path(&self.dump_dir);
        if !dump.exists() {
            return false;
        }
        match open_dump(&dump).await {
            Ok(loaded) => {
                tracing::info!(
                    path = %dump.display(),
                    series_rows = loaded.series_rows,
                    "loaded MangaBaka offline dump"
                );
                instruments()
                    .dump_series_rows
                    .record(loaded.series_rows, &[]);
                *self.offline.write().await = Some(loaded);
                true
            }
            Err(e) => {
                tracing::warn!(error = ?e, path = %dump.display(),
                    "failed to open existing dump; will retry on next refresh");
                false
            }
        }
    }

    /// Lookup by native MangaBaka id (the string form of the numeric id).
    #[tracing::instrument(skip(self), fields(provider = PROVIDER_ID))]
    pub async fn get(&self, mangabaka_id: &str) -> Result<Option<SeriesMetadata>, MangabakaError> {
        if self
            .negative_cache
            .is_known_miss(PROVIDER_ID, mangabaka_id)
            .await
        {
            instruments().negative_cache_hits.add(1, &[]);
            return Ok(None);
        }
        if let Some(store) = self.current_store().await
            && let Some(hit) = store
                .find_by_id(mangabaka_id)
                .await
                .map_err(MangabakaError::Store)?
        {
            return Ok(Some(hit));
        }
        if !self.api_fallback {
            self.negative_cache
                .record_miss(PROVIDER_ID, mangabaka_id)
                .await;
            return Ok(None);
        }
        instruments().api_fallback_hits.add(1, &[]);
        let api_hit = self
            .client
            .get_series(mangabaka_id)
            .await?
            .map(series_to_metadata);
        if api_hit.is_none() {
            self.negative_cache
                .record_miss(PROVIDER_ID, mangabaka_id)
                .await;
        }
        Ok(api_hit)
    }

    /// Foreign-ID pivot: resolve another provider's id to MangaBaka
    /// metadata. `Provider::Mangabaka` delegates to [`Self::get`];
    /// `Provider::Other` (and any source MangaBaka cannot cross-resolve)
    /// returns `Ok(None)` — rejected from resolution, never a panic.
    #[tracing::instrument(skip(self), fields(provider = PROVIDER_ID, foreign = %foreign_provider))]
    pub async fn resolve_by_foreign_id(
        &self,
        foreign_provider: &Provider,
        foreign_id: &str,
    ) -> Result<Option<SeriesMetadata>, MangabakaError> {
        if *foreign_provider == Provider::Mangabaka {
            return self.get(foreign_id).await;
        }
        let Some(mb_source) = provider_to_mb_source(foreign_provider) else {
            return Ok(None);
        };
        let neg_key = format!("{foreign_provider}:{foreign_id}");
        if self
            .negative_cache
            .is_known_miss(PROVIDER_ID, &neg_key)
            .await
        {
            instruments().negative_cache_hits.add(1, &[]);
            return Ok(None);
        }
        if let Some(store) = self.current_store().await
            && let Some(hit) = store
                .find_by_source_id(mb_source, foreign_id)
                .await
                .map_err(MangabakaError::Store)?
        {
            return Ok(Some(hit));
        }
        if !self.api_fallback {
            self.negative_cache.record_miss(PROVIDER_ID, &neg_key).await;
            return Ok(None);
        }
        instruments().api_fallback_hits.add(1, &[]);
        let api_hit = self
            .client
            .get_by_source_id(mb_source, foreign_id)
            .await?
            .map(series_to_metadata);
        if api_hit.is_none() {
            self.negative_cache.record_miss(PROVIDER_ID, &neg_key).await;
        }
        Ok(api_hit)
    }

    /// Title search: dump FTS when loaded (Dice re-ranked), else API
    /// search when the fallback is configured, else empty.
    #[tracing::instrument(skip(self), fields(provider = PROVIDER_ID))]
    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<SearchHit>, MangabakaError> {
        if let Some(store) = self.current_store().await {
            return store
                .search_fts(query, limit.max(1))
                .await
                .map_err(MangabakaError::Store);
        }
        if !self.api_fallback {
            return Ok(Vec::new());
        }
        instruments().api_fallback_hits.add(1, &[]);
        let rows = self.client.search(query, limit.max(1)).await?;
        Ok(rows.iter().map(series_to_search_hit).collect())
    }

    /// Taste-weighted discovery over the offline dump. `None` when no dump is
    /// loaded: discovery has no API equivalent.
    pub async fn discover(
        &self,
        query: &crate::offline::DiscoverQuery<'_>,
    ) -> Result<Option<Vec<SeriesMetadata>>, MangabakaError> {
        let Some(store) = self.current_store().await else {
            return Ok(None);
        };
        store
            .discover(query)
            .await
            .map(Some)
            .map_err(MangabakaError::Store)
    }

    /// Franchise links (`kind`, related MangaBaka id) per series, from the
    /// offline dump. Empty without a dump: links are a dump-only signal.
    pub async fn relations(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<(String, i64)>>, MangabakaError> {
        let Some(store) = self.current_store().await else {
            return Ok(Default::default());
        };
        store
            .relations_for_many(ids)
            .await
            .map_err(MangabakaError::Store)
    }

    pub async fn offline_loaded(&self) -> bool {
        self.offline.read().await.is_some()
    }

    /// True when the dump file on disk is older than `max_age`. A missing
    /// dump reports `false` (absence is its own, separately handled state),
    /// as does a future mtime (clock skew counts as fresh).
    pub async fn dump_older_than(&self, max_age: std::time::Duration) -> bool {
        let Ok(meta) = tokio::fs::metadata(offline::dump_path(&self.dump_dir)).await else {
            return false;
        };
        let Ok(modified) = meta.modified() else {
            return false;
        };
        modified.elapsed().map(|age| age > max_age).unwrap_or(false)
    }

    /// Wholesale dump refresh: download → extract → validate → prepare →
    /// atomic promote → re-open. Serialized; an overlapping fire is
    /// skipped with a warning. A failure leaves the previous dump serving
    /// (the store is only swapped after the promote succeeds).
    pub async fn refresh_cache(&self) -> Result<RefreshOutcome, MangabakaError> {
        let Some(dump_url) = self.dump_url.as_deref() else {
            return Ok(RefreshOutcome::NotConfigured);
        };
        let Ok(_guard) = self.refresh_lock.try_lock() else {
            tracing::warn!("MangaBaka refresh already running; skipping this fire");
            return Ok(RefreshOutcome::SkippedAlreadyRunning);
        };

        let started_at = Utc::now();
        let timer = std::time::Instant::now();
        let result = self.refresh_inner(dump_url).await;
        instruments()
            .refresh_duration
            .record(timer.elapsed().as_secs_f64(), &[]);

        let attempt = RefreshAttempt {
            at: started_at,
            outcome: match &result {
                Ok((_, rows)) => Ok(*rows),
                Err(e) => Err(e.to_string()),
            },
        };
        *self.last_refresh.write().await = Some(attempt);

        let (artifacts, series_rows) = result?;
        Ok(RefreshOutcome::Refreshed(RefreshSummary {
            started_at,
            finished_at: Utc::now(),
            bytes_downloaded: artifacts.bytes_downloaded,
            series_rows,
            sha1: artifacts.sha1,
        }))
    }

    async fn refresh_inner(
        &self,
        dump_url: &str,
    ) -> Result<(offline::RefreshArtifacts, u64), MangabakaError> {
        // Close the current store before promoting: Windows can't rename
        // over an open file, and even on POSIX it keeps the reader from
        // seeing torn state. The previous file stays on disk untouched
        // until the validated temp copy is renamed over it, so a failure
        // below re-opens the old dump.
        {
            let mut guard = self.offline.write().await;
            *guard = None;
        }

        let pipeline = offline::refresh(&self.http_for_refresh, dump_url, &self.dump_dir).await;

        let dump = offline::dump_path(&self.dump_dir);
        match pipeline {
            Ok(artifacts) => {
                let loaded = open_dump(&dump).await.map_err(MangabakaError::Store)?;
                let rows = loaded.series_rows;
                instruments().dump_series_rows.record(rows, &[]);
                *self.offline.write().await = Some(loaded);
                // A fresh dump invalidates every recorded miss.
                self.negative_cache.clear().await;
                tracing::info!(series_rows = rows, "MangaBaka dump refreshed");
                Ok((artifacts, rows))
            }
            Err(err) => {
                // The promote never happened; re-open the previous dump (if
                // any) so reads keep serving the old data.
                if dump.exists() {
                    match open_dump(&dump).await {
                        Ok(loaded) => *self.offline.write().await = Some(loaded),
                        Err(reopen) => {
                            tracing::error!(error = ?reopen, "could not re-open previous dump after failed refresh");
                        }
                    }
                }
                Err(MangabakaError::Store(err))
            }
        }
    }

    /// Backbone state for the status surfaces.
    pub async fn status(&self) -> BackboneStatus {
        let (dump_loaded, series_rows) = match self.offline.read().await.as_ref() {
            Some(loaded) => (true, Some(loaded.series_rows)),
            None => (false, None),
        };
        let dump_modified_at = tokio::fs::metadata(offline::dump_path(&self.dump_dir))
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .map(DateTime::<Utc>::from);
        BackboneStatus {
            dump_loaded,
            series_rows,
            dump_modified_at,
            last_refresh: self.last_refresh.read().await.clone(),
            // The refresh holds its lock for the whole pipeline, so a
            // failed try_lock means one is in flight.
            refresh_running: self.refresh_lock.try_lock().is_err(),
            negative_cache_size: self.negative_cache.len().await,
            api_fallback: self.api_fallback,
        }
    }

    /// Snapshot the current store under a short-lived read lock so the
    /// lock is not held across the actual query's await.
    async fn current_store(&self) -> Option<Arc<OfflineStore>> {
        self.offline
            .read()
            .await
            .as_ref()
            .map(|loaded| Arc::clone(&loaded.store))
    }
}

async fn open_dump(path: &std::path::Path) -> anyhow::Result<LoadedDump> {
    let store = OfflineStore::open_ro(path).await?;
    let series_rows = store.count_active_rows().await?;
    Ok(LoadedDump {
        store: Arc::new(store),
        series_rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shisho_config::Secret;
    use tempfile::TempDir;

    fn dump_only_cfg() -> MangabakaConfig {
        MangabakaConfig {
            api_fallback: false,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn api_fallback_without_key_degrades_to_dump_only() {
        let dir = TempDir::new().unwrap();
        let cfg = MangabakaConfig::default(); // api_fallback=true, no key
        let provider = MangabakaProvider::new(&cfg, dir.path().to_path_buf()).unwrap();
        assert!(!provider.api_fallback, "no key means no API fallback");
    }

    #[tokio::test]
    async fn api_fallback_with_key_is_honored() {
        let dir = TempDir::new().unwrap();
        let cfg = MangabakaConfig {
            api_key: Secret::new("k"),
            ..Default::default()
        };
        let provider = MangabakaProvider::new(&cfg, dir.path().to_path_buf()).unwrap();
        assert!(provider.api_fallback);
    }

    #[tokio::test]
    async fn offline_loaded_reports_false_when_no_dump_on_disk() {
        let dir = TempDir::new().unwrap();
        let provider = MangabakaProvider::new(&dump_only_cfg(), dir.path().to_path_buf()).unwrap();
        assert!(!provider.offline_loaded().await);
        let status = provider.status().await;
        assert!(!status.dump_loaded);
        assert_eq!(status.series_rows, None);
        assert!(status.last_refresh.is_none());
    }

    #[tokio::test]
    async fn dump_older_than_reflects_the_on_disk_file_age() {
        let dir = TempDir::new().unwrap();
        let provider = MangabakaProvider::new(&dump_only_cfg(), dir.path().to_path_buf()).unwrap();
        // Missing dump is its own state, never "too old".
        assert!(!provider.dump_older_than(std::time::Duration::ZERO).await);

        std::fs::write(offline::dump_path(dir.path()), b"stub").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            provider
                .dump_older_than(std::time::Duration::from_millis(1))
                .await
        );
        assert!(
            !provider
                .dump_older_than(std::time::Duration::from_secs(24 * 60 * 60))
                .await
        );
    }

    #[tokio::test]
    async fn refresh_cache_reports_not_configured_without_dump_url() {
        let dir = TempDir::new().unwrap();
        let cfg = MangabakaConfig {
            offline_dump_url: None,
            api_fallback: false,
            ..Default::default()
        };
        let provider = MangabakaProvider::new(&cfg, dir.path().to_path_buf()).unwrap();
        let outcome = provider.refresh_cache().await.unwrap();
        assert!(matches!(outcome, RefreshOutcome::NotConfigured));
    }

    #[tokio::test]
    async fn dump_only_miss_returns_none_and_records_negative_entry() {
        let dir = TempDir::new().unwrap();
        let provider = MangabakaProvider::new(&dump_only_cfg(), dir.path().to_path_buf()).unwrap();
        assert!(provider.get("123").await.unwrap().is_none());
        assert_eq!(provider.status().await.negative_cache_size, 1);
        // Second call short-circuits on the negative cache (still None).
        assert!(provider.get("123").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn other_provider_is_rejected_from_resolution_not_a_panic() {
        let dir = TempDir::new().unwrap();
        let provider = MangabakaProvider::new(&dump_only_cfg(), dir.path().to_path_buf()).unwrap();
        let hit = provider
            .resolve_by_foreign_id(&Provider::Other("fandom".into()), "1")
            .await
            .unwrap();
        assert!(hit.is_none());
        // Unknown providers are not even recorded as misses.
        assert_eq!(provider.status().await.negative_cache_size, 0);
    }
}
