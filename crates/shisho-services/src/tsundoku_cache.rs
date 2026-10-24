//! Tsundoku query cache with feed-cursor invalidation.
//!
//! List and detail queries are SWR-cached (~5 min revalidate) to bound
//! Tsundoku load. The series feed is used as a **change signal only** (PRD
//! Part 4: a feed-built metadata mirror is explicitly out of scope): a
//! periodic poll reads the cursor persisted in `feed_cursor` — Shisho's
//! only cross-system state — and
//!
//! - an **empty page** confirms nothing changed: all entry lifetimes are
//!   extended, nothing is invalidated;
//! - a **non-empty page** invalidates affected detail entries by series ID,
//!   re-hydrates the ones that were warm via the detail endpoint, and
//!   conservatively invalidates every cached list (any release can change
//!   list membership and rating/recency ordering; lists rebuild lazily on
//!   the next demand);
//!
//! then the cursor advance is committed, so a restart resumes where
//! processing last completed. The very first poll (no stored cursor) walks
//! the feed from the beginning once to establish the cursor; with cold
//! caches that walk is invalidation-free and costs only the page reads.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, DatabaseConnection, EntityTrait};

use shisho_cache::{Cached, SwrCache, SwrConfig, SwrError};
use shisho_db::entities::feed_cursor;
use shisho_tsundoku::{
    CoverageSpan, SeriesDetail, SeriesListPage, SeriesListQuery, TsundokuClient, TsundokuError,
};

/// The `feed_cursor` row key for the Tsundoku series feed.
const FEED_KEY: &str = "tsundoku";
/// Feed page size for one poll pass.
const FEED_PAGE_LIMIT: u32 = 100;
/// Feed page size for batched external-ID resolution.
const RESOLUTION_PAGE_LIMIT: u32 = 100;

/// What one external id resolved to in Tsundoku's catalog.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub series_id: i32,
    pub volume_coverage: Vec<CoverageSpan>,
    pub chapter_coverage: Vec<CoverageSpan>,
}

#[derive(Debug, thiserror::Error)]
pub enum FeedPollError {
    #[error(transparent)]
    Tsundoku(#[from] TsundokuError),
    #[error("feed cursor persistence failed")]
    Db(#[from] sea_orm::DbErr),
}

/// What one poll pass did (for logs and tests).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FeedPoll {
    /// Feed items seen across all pages of this pass.
    pub changed: usize,
    /// Warm detail entries refreshed via the detail endpoint.
    pub rehydrated: usize,
    /// True when the pass saw only an empty page (lifetimes extended).
    pub unchanged: bool,
}

/// SWR-cached Tsundoku queries + the feed poller.
#[derive(Clone)]
pub struct TsundokuCache {
    client: TsundokuClient,
    db: DatabaseConnection,
    lists: SwrCache<String, Arc<SeriesListPage>>,
    details: SwrCache<i32, Arc<SeriesDetail>>,
}

impl TsundokuCache {
    pub fn new(client: TsundokuClient, db: DatabaseConnection, cfg: SwrConfig) -> Self {
        Self {
            client,
            db,
            lists: SwrCache::new(cfg),
            details: SwrCache::new(cfg),
        }
    }

    pub fn client(&self) -> &TsundokuClient {
        &self.client
    }

    /// Cached `GET /series` list query.
    pub async fn list(
        &self,
        query: &SeriesListQuery,
    ) -> Result<Cached<Arc<SeriesListPage>>, SwrError<TsundokuError>> {
        let client = self.client.clone();
        let query = query.clone();
        self.lists
            .get_or_load(query.cache_key(), move || async move {
                client.list_series(&query).await.map(Arc::new)
            })
            .await
    }

    /// Cached `GET /series/{id}` detail query.
    pub async fn detail(
        &self,
        id: i32,
    ) -> Result<Cached<Arc<SeriesDetail>>, SwrError<TsundokuError>> {
        let client = self.client.clone();
        self.details
            .get_or_load(id, move || async move {
                client.series_detail(id).await.map(Arc::new)
            })
            .await
    }

    /// Map external ids (Tsundoku `<token>:<id>` form) to the Tsundoku series
    /// carrying them, with coverage, via the filtered feed: the only
    /// by-external-ID query Tsundoku offers. Every external id of a matched
    /// series is a key, so callers can look up by any identity they hold.
    /// Empty when Tsundoku is unconfigured or there is nothing to resolve.
    pub async fn resolve_external_ids(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, Resolved>, TsundokuError> {
        let mut map = HashMap::new();
        if ids.is_empty() || !self.client.is_configured() {
            return Ok(map);
        }
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .client
                .feed_filtered(cursor.as_deref(), Some(RESOLUTION_PAGE_LIMIT), ids)
                .await?;
            for item in page.items {
                let resolved = Resolved {
                    series_id: item.series_id,
                    volume_coverage: item.volume_coverage,
                    chapter_coverage: item.chapter_coverage,
                };
                for eid in &item.external_ids {
                    map.insert(eid.normalized().canonical_key(), resolved.clone());
                }
            }
            cursor = page.next_cursor;
            if !page.has_more || cursor.is_none() {
                return Ok(map);
            }
        }
    }

    /// Most recent successful Tsundoku load (list or detail), for
    /// `/healthz` and `status()`.
    pub fn last_warm(&self) -> Option<DateTime<Utc>> {
        self.lists.last_warm().max(self.details.last_warm())
    }

    /// One feed poll pass: read the stored cursor, page through the feed,
    /// apply invalidation, persist the cursor after each processed page.
    pub async fn poll_feed_once(&self) -> Result<FeedPoll, FeedPollError> {
        let mut cursor = self.stored_cursor().await?;
        let mut outcome = FeedPoll::default();

        loop {
            let page = self
                .client
                .feed(cursor.as_deref(), Some(FEED_PAGE_LIMIT))
                .await?;

            if page.items.is_empty() {
                // Nothing changed since the cursor: every cached entry is
                // still accurate, so extend lifetimes instead of letting
                // them go stale.
                self.lists.extend_all();
                self.details.extend_all();
                outcome.unchanged = outcome.changed == 0;
                if let Some(next) = page.next_cursor {
                    self.store_cursor(&next).await?;
                }
                return Ok(outcome);
            }

            outcome.changed += page.items.len();

            // Any release can change membership or ordering of any list
            // query; rebuilding lazily on demand is cheaper than guessing
            // which lists were affected.
            for key in self.lists.keys() {
                self.lists.invalidate(&key);
            }

            let warm: HashSet<i32> = self.details.keys().into_iter().collect();
            for item in &page.items {
                if warm.contains(&item.series_id) {
                    match self.client.series_detail(item.series_id).await {
                        Ok(detail) => {
                            self.details.insert(item.series_id, Arc::new(detail));
                            outcome.rehydrated += 1;
                        }
                        Err(err) => {
                            tracing::warn!(series_id = item.series_id, error = %err,
                                "feed re-hydration failed; entry invalidated for lazy rebuild");
                            self.details.invalidate(&item.series_id);
                        }
                    }
                } else {
                    self.details.invalidate(&item.series_id);
                }
            }

            // Commit the advance only after the page was fully processed:
            // a crash re-processes this page instead of skipping it.
            match page.next_cursor {
                Some(next) => {
                    self.store_cursor(&next).await?;
                    cursor = Some(next);
                }
                None => return Ok(outcome),
            }
            if !page.has_more {
                return Ok(outcome);
            }
        }
    }

    /// Spawn the periodic poll loop (from `serve`). The first tick fires
    /// immediately; failures are logged and retried on the next tick.
    pub fn spawn_feed_poller(&self, every: Duration) -> tokio::task::JoinHandle<()> {
        let cache = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(every);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match cache.poll_feed_once().await {
                    Ok(outcome) => {
                        tracing::debug!(?outcome, "tsundoku feed poll complete");
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "tsundoku feed poll failed; retrying next tick");
                    }
                }
            }
        })
    }

    async fn stored_cursor(&self) -> Result<Option<String>, sea_orm::DbErr> {
        Ok(feed_cursor::Entity::find_by_id(FEED_KEY)
            .one(&self.db)
            .await?
            .map(|row| row.cursor))
    }

    async fn store_cursor(&self, cursor: &str) -> Result<(), sea_orm::DbErr> {
        let row = feed_cursor::ActiveModel {
            id: Set(FEED_KEY.to_owned()),
            cursor: Set(cursor.to_owned()),
            updated_at: Set(Utc::now().into()),
        };
        feed_cursor::Entity::insert(row)
            .on_conflict(
                OnConflict::column(feed_cursor::Column::Id)
                    .update_columns([feed_cursor::Column::Cursor, feed_cursor::Column::UpdatedAt])
                    .to_owned(),
            )
            .exec(&self.db)
            .await?;
        Ok(())
    }
}
