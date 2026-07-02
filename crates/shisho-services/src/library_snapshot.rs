//! Per-user library snapshot — the universal ownership gate.
//!
//! The caller's full Codex series list (paginated, with their credential)
//! is folded into the normalized owned `(Provider, id)` set, the read-state
//! map, ratings presence, and the Codex series UUIDs. Cached per `user_sub`
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

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::read_state::ReadState;
use shisho_cache::{Cached, SwrCache, SwrConfig, SwrError};
use shisho_codex::{CodexClient, CodexCredential, CodexError};
use shisho_ids::{ExternalId, Provider};

/// Codex caps `pageSize` at 100; ask for the cap to minimize round-trips.
const PAGE_SIZE: u64 = 100;

/// One series of the caller's library, as the candidate pools consume it.
#[derive(Debug, Clone)]
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
    pub book_count: i64,
    pub unread_count: Option<i64>,
    pub read_state: ReadState,
    /// Normalized known-provider IDs (unknown providers are excluded from
    /// joins and tracked in [`LibrarySnapshot::unresolvable`]).
    pub external_ids: Vec<ExternalId>,
    /// The caller has a Codex rating for this series.
    pub rated: bool,
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

    /// Any of `ids` owned — the per-candidate anti-join.
    pub fn owns_any<'a>(&self, ids: impl IntoIterator<Item = &'a ExternalId>) -> bool {
        ids.into_iter().any(|id| self.owns(id))
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
/// refresh) with the user and the fresh snapshot — the lazy-reconciliation
/// trigger. Must not block: spawn any real work.
pub type SnapshotBuildHook = Arc<dyn Fn(&str, &Arc<LibrarySnapshot>) + Send + Sync>;

/// The snapshot cache, keyed by `user_sub`.
#[derive(Clone)]
pub struct LibrarySnapshotService {
    codex: CodexClient,
    cache: SwrCache<String, Arc<LibrarySnapshot>>,
    on_build: Option<SnapshotBuildHook>,
}

impl LibrarySnapshotService {
    pub fn new(codex: CodexClient, cfg: SwrConfig) -> Self {
        Self {
            codex,
            cache: SwrCache::new(cfg),
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
                    hook(&key, &snapshot);
                }
                Ok(snapshot)
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

/// Paginated fetch + normalization fold.
async fn build(
    codex: &CodexClient,
    credential: &CodexCredential,
) -> Result<LibrarySnapshot, CodexError> {
    let rated: HashSet<String> = codex
        .user_ratings(credential)
        .await?
        .ratings
        .into_iter()
        .map(|r| r.series_id)
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
                Ok(id) if !matches!(id.provider, Provider::Other(_)) => mapped.push(id),
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

        snapshot.series.push(SnapshotSeries {
            read_state: ReadState::derive(row.book_count, row.unread_count),
            rated: rated.contains(&row.id),
            id: row.id,
            library_id: row.library_id,
            library_name: row.library_name,
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
