//! Per-user library snapshot — the universal ownership gate.
//!
//! The caller's full Codex series list (paginated, with their credential)
//! is folded into the normalized owned `(Provider, id)` set, the read-state
//! map, the caller's ratings, and the Codex series UUIDs. Cached per `user_sub`
//! with SWR revalidation (~15 min): the owned set is slow-moving; read
//! state moves fast mid-binge but its staleness is TTL-bounded and reported
//! via `as_of`.
//!
//! **Invariant (PRD): ownership is always per-user and always
//! Codex-authoritative.** Every candidate in every pool is anti-joined
//! against this snapshot's owned set; nothing else may claim ownership.
//!
//! Unmappable external IDs are counted and retained for `status()` —
//! never patched locally (the fix belongs to Codex's metadata-match flow).
//!
//! Upgrade path if the TTL bound ever bites: subscribe to Codex
//! `/api/v1/events/stream` (SSE entity-change feed) and invalidate per-user
//! entries on events.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::read_state::ReadState;
use shisho_cache::{Cached, SwrCache, SwrConfig, SwrError};
use shisho_codex::{CodexClient, CodexCredential, CodexError, UserSeriesRating};
use shisho_ids::{ExternalId, Provider};

/// Codex caps `pageSize` at 100; ask for the cap to minimize round-trips.
const PAGE_SIZE: u64 = 100;

/// One series of the caller's library, as the candidate pools consume it.
/// Serializable so a library can be saved as an offline evaluation fixture.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSeries {
    /// Codex series UUID.
    pub id: String,
    /// The Codex library this series shelves in. The snapshot always spans
    /// every library (the owned set must stay global); per-library scoping
    /// is a query-time filter.
    pub library_id: String,
    pub library_name: String,
    pub title: String,
    pub summary: Option<String>,
    pub status: Option<String>,
    pub year: Option<i32>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    /// Credited people, deduplicated (a writer-artist appears once).
    #[serde(default)]
    pub authors: Vec<String>,
    pub book_count: i64,
    pub unread_count: Option<i64>,
    pub read_state: ReadState,
    /// Normalized known-provider IDs (unknown providers are excluded from
    /// joins and tracked in [`LibrarySnapshot::unresolvable`]).
    #[serde(with = "canonical_ids")]
    pub external_ids: Vec<ExternalId>,
    /// The caller's Codex rating, 1-100 (Codex displays it as 1-10).
    pub rating: Option<i32>,
    /// The caller's free-text notes on that rating.
    pub rating_notes: Option<String>,
    /// When the caller last set or changed the rating.
    #[serde(default)]
    pub rating_updated_at: Option<DateTime<Utc>>,
}

/// External ids as their canonical `<token>:<id>` strings on the wire.
mod canonical_ids {
    use serde::{Deserialize, Deserializer, Serializer};
    use shisho_ids::ExternalId;

    pub fn serialize<S: Serializer>(ids: &[ExternalId], ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_seq(ids.iter().map(ExternalId::canonical_key))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<ExternalId>, D::Error> {
        Vec::<String>::deserialize(de)?
            .iter()
            .map(|raw| ExternalId::from_tsundoku(raw).map_err(serde::de::Error::custom))
            .collect()
    }
}

/// A series that cannot participate in any Tsundoku join: none of its
/// external IDs map to a known provider (or it has none at all). Surfaced
/// on `status()` with a pointer to Codex's metadata-match flow; never
/// patched here.
#[derive(Debug, Clone)]
pub struct UnresolvableSeries {
    pub series_id: String,
    pub title: String,
    /// The raw `source:externalId` strings that failed to map, verbatim.
    pub raw_sources: Vec<String>,
}

/// The caller's library, folded for joining and filtering.
#[derive(Debug, Clone)]
pub struct LibrarySnapshot {
    pub series: Vec<SnapshotSeries>,
    /// The normalized owned set — the anti-join key for every pool.
    owned: HashSet<(Provider, String)>,
    pub unresolvable: Vec<UnresolvableSeries>,
}

impl LibrarySnapshot {
    /// Fold a plain series list into a snapshot, deriving the owned set
    /// from the series' external IDs. Seed/test helper: the production
    /// build path ([`build`]) also tracks unresolvable raw sources.
    pub fn from_series(series: Vec<SnapshotSeries>) -> Self {
        let owned = series
            .iter()
            .flat_map(|s| s.external_ids.iter())
            .map(|id| (id.provider.clone(), id.id.clone()))
            .collect();
        Self {
            series,
            owned,
            unresolvable: Vec::new(),
        }
    }

    /// The ownership gate: does the caller own the series this external ID
    /// points at?
    pub fn owns(&self, id: &ExternalId) -> bool {
        self.owned.contains(&(id.provider.clone(), id.id.clone()))
    }

    /// Find the library series carrying the given external ID, if any.
    pub fn series_by_external_id(&self, id: &ExternalId) -> Option<&SnapshotSeries> {
        self.series
            .iter()
            .find(|s| s.external_ids.iter().any(|e| e.key() == id.key()))
    }

    /// Find a library series by its Codex UUID.
    pub fn series_by_uuid(&self, uuid: &str) -> Option<&SnapshotSeries> {
        self.series.iter().find(|s| s.id == uuid)
    }

    pub fn owned_len(&self) -> usize {
        self.owned.len()
    }

    /// Distinct library names present in the snapshot, for "did you mean"
    /// errors on bad selections.
    pub fn library_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for series in &self.series {
            if !names.contains(&series.library_name) {
                names.push(series.library_name.clone());
            }
        }
        names
    }
}

/// Does a caller-supplied library selector match this library? Names are
/// case-insensitive; IDs are exact.
pub fn selector_matches(selector: &str, library_id: &str, library_name: &str) -> bool {
    selector == library_id || selector.eq_ignore_ascii_case(library_name)
}

/// Selectors that match no library in the snapshot (typos, wrong instance).
pub fn unmatched_selectors(snapshot: &LibrarySnapshot, selectors: &[String]) -> Vec<String> {
    selectors
        .iter()
        .filter(|sel| {
            !snapshot
                .series
                .iter()
                .any(|s| selector_matches(sel, &s.library_id, &s.library_name))
        })
        .cloned()
        .collect()
}

/// Runs after every successful snapshot build (inline or background
/// refresh) with the user, the Codex client and credential the snapshot was
/// built with, and the fresh snapshot — the lazy-reconciliation trigger.
/// Must not block: spawn any real work.
pub type SnapshotBuildHook =
    Arc<dyn Fn(&str, &CodexClient, &CodexCredential, &Arc<LibrarySnapshot>) + Send + Sync>;

/// A cached local index of the caller's Codex library: canonical external id
/// (`<source>:<id>`) -> Codex series UUID. Built from the cheap `external-index`
/// endpoint so consumers resolve an external id (e.g. a MangaBaka id) to its
/// Codex series without re-scanning the full library each time.
pub type CodexIndex = HashMap<String, String>;

/// `external-index` page size (Codex caps this at 500).
const INDEX_PAGE_SIZE: u64 = 500;

/// The snapshot cache, keyed by `user_sub`.
#[derive(Clone)]
pub struct LibrarySnapshotService {
    codex: CodexClient,
    cache: SwrCache<String, Arc<LibrarySnapshot>>,
    index_cache: SwrCache<String, Arc<CodexIndex>>,
    on_build: Option<SnapshotBuildHook>,
}

impl LibrarySnapshotService {
    pub fn new(codex: CodexClient, cfg: SwrConfig) -> Self {
        Self {
            codex,
            cache: SwrCache::new(cfg),
            index_cache: SwrCache::new(cfg),
            on_build: None,
        }
    }

    /// Install the post-build hook (see [`SnapshotBuildHook`]).
    pub fn with_build_hook(mut self, hook: SnapshotBuildHook) -> Self {
        self.on_build = Some(hook);
        self
    }

    /// The caller's snapshot: fresh/stale per SWR semantics, built inline
    /// (with their credential) on a cold cache. A cold-build failure is the
    /// typed Codex error — a 401 reaches the caller verbatim.
    pub async fn get(
        &self,
        user_sub: &str,
        credential: &CodexCredential,
    ) -> Result<Cached<Arc<LibrarySnapshot>>, SwrError<CodexError>> {
        let codex = self.codex.clone();
        let credential = credential.clone();
        let hook = self.on_build.clone();
        let key = user_sub.to_owned();
        self.cache
            .get_or_load(user_sub.to_owned(), move || async move {
                let snapshot = build(&codex, &credential).await.map(Arc::new)?;
                if let Some(hook) = hook {
                    hook(&key, &codex, &credential, &snapshot);
                }
                Ok(snapshot)
            })
            .await
    }

    /// The caller's cached Codex index (external id -> series UUID), built once
    /// from the slim `external-index` endpoint and reused per SWR semantics, so
    /// repeated identity resolutions (verdict links, ownership checks) don't each
    /// re-scan Codex. Cold-build failures surface the typed Codex error.
    pub async fn external_index(
        &self,
        user_sub: &str,
        credential: &CodexCredential,
    ) -> Result<Cached<Arc<CodexIndex>>, SwrError<CodexError>> {
        let codex = self.codex.clone();
        let credential = credential.clone();
        self.index_cache
            .get_or_load(user_sub.to_owned(), move || async move {
                build_external_index(&codex, &credential)
                    .await
                    .map(Arc::new)
            })
            .await
    }

    /// The caller's cached snapshot, if one exists — no credential, no
    /// build. The web UI's status page reads it (a UI session holds no
    /// Codex credential); an absent entry is reported, never rebuilt.
    pub fn peek(&self, user_sub: &str) -> Option<Cached<Arc<LibrarySnapshot>>> {
        self.cache.peek(&user_sub.to_owned())
    }

    /// Most recent successful snapshot build (any user), for `/healthz`.
    pub fn last_warm(&self) -> Option<DateTime<Utc>> {
        self.cache.last_warm()
    }

    /// The underlying client, for per-user calls that bypass the snapshot
    /// (e.g. a live single-series rating read).
    pub fn codex(&self) -> &CodexClient {
        &self.codex
    }
}

/// Sweep the slim `external-index` (paginated) into a canonical-id -> UUID map.
async fn build_external_index(
    codex: &CodexClient,
    credential: &CodexCredential,
) -> Result<CodexIndex, CodexError> {
    let mut map = CodexIndex::new();
    let mut page = 1u64;
    loop {
        let response = codex
            .list_series_external_index(credential, page, INDEX_PAGE_SIZE)
            .await?;
        for entry in &response.data {
            for eid in entry.normalized_external_ids() {
                map.insert(eid.canonical_key(), entry.id.clone());
            }
        }
        if response.data.is_empty() || page >= response.total_pages {
            return Ok(map);
        }
        page += 1;
    }
}

/// Paginated fetch + normalization fold.
async fn build(
    codex: &CodexClient,
    credential: &CodexCredential,
) -> Result<LibrarySnapshot, CodexError> {
    let mut ratings: HashMap<String, UserSeriesRating> = codex
        .user_ratings(credential)
        .await?
        .ratings
        .into_iter()
        .map(|r| (r.series_id.clone(), r))
        .collect();

    let mut rows = Vec::new();
    let mut page = 1;
    loop {
        let response = codex.list_series(credential, page, PAGE_SIZE).await?;
        let total_pages = response.total_pages;
        let got = response.data.len();
        rows.extend(response.data);
        if page >= total_pages || got == 0 {
            break;
        }
        page += 1;
    }

    let mut snapshot = LibrarySnapshot {
        series: Vec::with_capacity(rows.len()),
        owned: HashSet::new(),
        unresolvable: Vec::new(),
    };

    for row in rows {
        let mut mapped = Vec::new();
        let mut raw_failures = Vec::new();
        for raw in &row.external_ids {
            match ExternalId::from_codex_parts(&raw.source, &raw.external_id) {
                Ok(id) if id.is_known() => mapped.push(id),
                _ => raw_failures.push(format!("{}:{}", raw.source, raw.external_id)),
            }
        }

        for id in &mapped {
            snapshot.owned.insert((id.provider.clone(), id.id.clone()));
        }
        if mapped.is_empty() {
            snapshot.unresolvable.push(UnresolvableSeries {
                series_id: row.id.clone(),
                title: row.metadata.title.clone(),
                raw_sources: raw_failures,
            });
        }

        let rated = ratings.remove(&row.id);
        snapshot.series.push(SnapshotSeries {
            read_state: ReadState::derive(row.book_count, row.unread_count),
            rating: rated.as_ref().map(|r| r.rating),
            rating_updated_at: rated.as_ref().and_then(|r| r.updated_at),
            rating_notes: rated.and_then(|r| r.notes),
            id: row.id,
            library_id: row.library_id,
            library_name: row.library_name,
            authors: {
                let mut names: Vec<String> = Vec::new();
                for author in &row.metadata.authors {
                    if !names.contains(&author.name) {
                        names.push(author.name.clone());
                    }
                }
                names
            },
            title: row.metadata.title,
            summary: row.metadata.summary,
            status: row.metadata.status,
            year: row.metadata.year,
            genres: row.genres.into_iter().map(|g| g.name).collect(),
            tags: row.tags.into_iter().map(|t| t.name).collect(),
            book_count: row.book_count,
            unread_count: row.unread_count,
            external_ids: mapped,
        });
    }

    tracing::debug!(
        series = snapshot.series.len(),
        owned_ids = snapshot.owned.len(),
        unresolvable = snapshot.unresolvable.len(),
        "library snapshot built"
    );
    Ok(snapshot)
}
