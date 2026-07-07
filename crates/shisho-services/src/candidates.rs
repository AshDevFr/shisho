//! `get_candidates`: the two candidate pools.
//!
//! **Invariant: ownership is always per-user and always Codex-authoritative.**
//! Every candidate in every pool is anti-joined against the caller's own
//! owned-ID set from the library snapshot. Tsundoku's sweep-derived `owned`
//! flag and Codex overlay are at most server-side pre-filters (bandwidth),
//! never the gate, and overlay facts are never relayed to the caller.
//!
//! - **library** (primary): snapshot → untouched (`unread == total`) →
//!   verdict subtraction → enrich via Tsundoku where the series resolves
//!   (genres, tags, rating, description, coverage); series absent from
//!   Tsundoku are kept with `enrichment: none` (PRD Q8 — the model verifies
//!   those via web research), never silently dropped.
//! - **acquirable** (Tier 1): Tsundoku catalog (`kind=manga`,
//!   `hasReleases=true`, rating-sorted) → mandatory anti-join → verdict
//!   subtraction → coverage spans attached.
//!
//! Pools over-provision (default 40, 30-50 band) and set `truncated` when
//! more candidates existed than were returned.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::library_snapshot::{self, SnapshotSeries};
use crate::read_state::ReadState;
use crate::verdicts::VerdictIndex;
use crate::{Services, UserCtx};
use shisho_cache::SwrError;
use shisho_codex::CodexError;
use shisho_ids::{ExternalId, Provider};
use shisho_tsundoku::{CoverageSpan, Order, SeriesDetail, SeriesListQuery, TsundokuError};

/// Default over-provision target (PRD: 30-50).
const DEFAULT_LIMIT: usize = 40;
/// Hard cap on the over-provision target.
const MAX_LIMIT: usize = 100;
/// Acquirable pool: catalog page size per cached list query.
const CATALOG_PAGE_SIZE: u32 = 50;
/// Acquirable pool: stop examining the catalog after this many items even
/// if the target is not met (bounds work when filters reject everything).
const MAX_EXAMINED_FACTOR: usize = 8;
/// Feed page size for batched external-ID resolution.
const RESOLUTION_PAGE_LIMIT: u32 = 100;

/// Which candidate pool to draw from.
//
// Schema note: variant docs are plain `//` and the type is
// `#[schemars(inline)]`, on purpose. Per-variant `///` docs make schemars
// emit a `oneOf` of `const`s, which the claude.ai tool-input serializer
// drops to `null`; a flat, inline `{type: string, enum: [...]}` is what
// every client handles. The `pool` tool parameter carries the full
// description.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum Pool {
    // The caller's untouched backlog (primary).
    Library,
    // Tsundoku's catalog minus everything the caller owns (Tier 1).
    Acquirable,
}

impl Pool {
    fn name(self) -> &'static str {
        match self {
            Pool::Library => "library",
            Pool::Acquirable => "acquirable",
        }
    }
}

/// Caller-supplied filters. Genre/tag matching is any-of (Tsundoku's
/// default match mode); status is exact.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Filters {
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    pub status: Option<String>,
    /// Codex libraries to draw from (names or IDs); empty = all. Library
    /// pool only — the acquirable pool is catalog-wide and rejects it.
    /// Scoping never touches the owned anti-join, which stays global.
    pub libraries: Vec<String>,
    /// Over-provision target; default 40, capped at 100.
    pub limit: Option<usize>,
}

impl Filters {
    fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }

    fn matches(&self, genres: &[String], tags: &[String], status: Option<&str>) -> bool {
        let any_ci = |wanted: &[String], have: &[String]| {
            wanted.is_empty()
                || wanted
                    .iter()
                    .any(|w| have.iter().any(|h| h.eq_ignore_ascii_case(w)))
        };
        if !any_ci(&self.genres, genres) || !any_ci(&self.tags, tags) {
            return false;
        }
        match &self.status {
            Some(wanted) => status.is_some_and(|s| s.eq_ignore_ascii_case(wanted)),
            None => true,
        }
    }
}

/// Where a candidate's metadata came from: `full` (Tsundoku catalog),
/// `mangabaka` (the backbone's dump/API where Tsundoku lacks the series),
/// or `none` (Codex-only — description/genre coverage may be too thin for
/// red-flag screening; verify via web research).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Enrichment {
    Full,
    Mangabaka,
    None,
}

/// One candidate, compact. Never carries ownership facts from Tsundoku's
/// Codex overlay — the caller's own Codex view is the only ownership
/// source, and it already gated this candidate's presence.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub title: String,
    /// Set for library-pool candidates (the caller's Codex series UUID).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_series_id: Option<String>,
    /// The Codex library the series shelves in (library pool only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tsundoku_series_id: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mangabaka_id: Option<i64>,
    /// Canonical `provider:id` strings.
    pub external_ids: Vec<String>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub volume_coverage: Vec<CoverageSpan>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub chapter_coverage: Vec<CoverageSpan>,
    pub enrichment: Enrichment,
}

/// Per-source data ages, so the model can phrase staleness honestly.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AsOf {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library: Option<DateTime<Utc>>,
    /// Oldest Tsundoku data used (most conservative bound).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tsundoku: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateSet {
    pub pool: Pool,
    pub candidates: Vec<Candidate>,
    pub as_of: AsOf,
    /// More candidates existed than were returned.
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CandidatesError {
    /// The library snapshot could not be built — both pools require it
    /// (it is the ownership gate). A 401 inside means the caller's Codex
    /// key expired or was revoked.
    #[error("library snapshot unavailable")]
    Codex(#[source] SwrError<CodexError>),
    /// The named pool needs Tsundoku and Tsundoku is unavailable. Never a
    /// silent empty result.
    #[error("the {pool} pool is degraded: Tsundoku is unavailable")]
    TsundokuDegraded {
        pool: &'static str,
        #[source]
        source: SwrError<TsundokuError>,
    },
    /// A `libraries` selector matched nothing — typo or wrong instance.
    /// Named loudly instead of returning a silently empty pool.
    #[error("no Codex library matches {selectors:?}; available libraries: {available:?}")]
    UnknownLibraries {
        selectors: Vec<String>,
        available: Vec<String>,
    },
    /// The acquirable pool is catalog-wide; a library scope is a mistake.
    #[error(
        "the acquirable pool is not scoped by Codex libraries (it is the catalog minus \
         everything you own); omit `libraries` for pool=acquirable"
    )]
    LibrariesNotApplicable,
    #[error("verdict query failed")]
    Db(#[from] sea_orm::DbErr),
}

pub async fn get_candidates(
    services: &Services,
    user: &UserCtx,
    pool: Pool,
    filters: &Filters,
) -> Result<CandidateSet, CandidatesError> {
    match pool {
        Pool::Library => library_pool(services, user, filters).await,
        Pool::Acquirable => acquirable_pool(services, user, filters).await,
    }
}

/// The primary pool: the caller's untouched backlog.
async fn library_pool(
    services: &Services,
    user: &UserCtx,
    filters: &Filters,
) -> Result<CandidateSet, CandidatesError> {
    let snapshot_hit = services
        .snapshots
        .get(&user.user_sub, &user.credential)
        .await
        .map_err(CandidatesError::Codex)?;
    let snapshot = snapshot_hit.value;
    let verdicts = VerdictIndex::load(&services.db, &user.user_sub).await?;

    // Library scoping is a query-time filter only; the snapshot (and the
    // owned anti-join) always spans every library.
    if !filters.libraries.is_empty() {
        let unmatched = library_snapshot::unmatched_selectors(&snapshot, &filters.libraries);
        if !unmatched.is_empty() {
            return Err(CandidatesError::UnknownLibraries {
                selectors: unmatched,
                available: snapshot.library_names(),
            });
        }
    }
    let in_selected_libraries = |s: &SnapshotSeries| {
        filters.libraries.is_empty()
            || filters
                .libraries
                .iter()
                .any(|sel| library_snapshot::selector_matches(sel, &s.library_id, &s.library_name))
    };

    let backlog: Vec<&SnapshotSeries> = snapshot
        .series
        .iter()
        .filter(|s| in_selected_libraries(s))
        .filter(|s| s.read_state == ReadState::Untouched)
        .filter(|s| !verdicts.excludes(&s.external_ids))
        .collect();

    // One batched feed query maps the backlog's external IDs to Tsundoku
    // series (and their coverage). Tsundoku being down degrades enrichment
    // for the whole pool — the pool itself never fails on it.
    let resolution = match resolve_in_tsundoku(services, &backlog).await {
        Ok(map) => map,
        Err(err) => {
            tracing::warn!(error = %err, "Tsundoku resolution failed; library pool degrades to enrichment=none");
            HashMap::new()
        }
    };

    let limit = filters.limit();
    let mut candidates = Vec::new();
    let mut tsundoku_as_of: Option<DateTime<Utc>> = None;
    let mut truncated = false;

    for series in &backlog {
        if candidates.len() == limit {
            truncated = true;
            break;
        }

        let resolved = series
            .external_ids
            .iter()
            .find_map(|id| resolution.get(&canonical(id)));

        let mut candidate = match resolved {
            Some(resolved) => match services.tsundoku.detail(resolved.series_id).await {
                Ok(hit) => {
                    tsundoku_as_of = oldest(tsundoku_as_of, hit.as_of);
                    enriched_candidate(series, &hit.value, resolved)
                }
                Err(err) => {
                    tracing::warn!(series = %series.title, error = %err,
                        "Tsundoku detail failed; candidate degrades to enrichment=none");
                    codex_only_candidate(series)
                }
            },
            None => codex_only_candidate(series),
        };
        // Backbone backfill before the filter pass, so dump genres/status
        // participate in matching.
        backfill_from_mangabaka(services, &mut candidate).await;

        if !filters.matches(
            &candidate.genres,
            &candidate.tags,
            candidate.status.as_deref(),
        ) {
            continue;
        }
        candidates.push(candidate);
    }

    Ok(CandidateSet {
        pool: Pool::Library,
        candidates,
        as_of: AsOf {
            library: Some(snapshot_hit.as_of),
            tsundoku: tsundoku_as_of,
        },
        truncated,
    })
}

/// The Tier 1 pool: Tsundoku's catalog minus everything the caller owns.
async fn acquirable_pool(
    services: &Services,
    user: &UserCtx,
    filters: &Filters,
) -> Result<CandidateSet, CandidatesError> {
    if !filters.libraries.is_empty() {
        return Err(CandidatesError::LibrariesNotApplicable);
    }
    // The acquirable pool needs the caller's owned set only (no read-state), so
    // it uses the slim cached Codex index rather than the full library snapshot:
    // an external id that keys into the index is owned.
    let index_hit = services
        .snapshots
        .external_index(&user.user_sub, &user.credential)
        .await
        .map_err(CandidatesError::Codex)?;
    let owned_index = index_hit.value;
    let verdicts = VerdictIndex::load(&services.db, &user.user_sub).await?;

    let degraded = |source| CandidatesError::TsundokuDegraded {
        pool: Pool::Acquirable.name(),
        source,
    };

    let limit = filters.limit();
    let max_examined = limit * MAX_EXAMINED_FACTOR;
    let mut candidates = Vec::new();
    let mut tsundoku_as_of: Option<DateTime<Utc>> = None;
    let mut truncated = false;
    let mut examined = 0usize;
    let mut page: u32 = 0;

    'pages: loop {
        let query = SeriesListQuery {
            page: Some(page),
            page_size: Some(CATALOG_PAGE_SIZE),
            kind: Some("manga".into()),
            has_releases: Some(true),
            status: filters.status.clone(),
            genres: (!filters.genres.is_empty()).then(|| filters.genres.clone()),
            tags: (!filters.tags.is_empty()).then(|| filters.tags.clone()),
            sort: Some("rating".into()),
            order: Some(Order::Desc),
            // Bandwidth pre-filter only, and only when the deployment opted
            // in; the anti-join below is the correctness gate.
            owned: services.tsundoku_owned_prefilter.then_some(false),
            ..Default::default()
        };

        let list_hit = services.tsundoku.list(&query).await.map_err(degraded)?;
        tsundoku_as_of = oldest(tsundoku_as_of, list_hit.as_of);
        let items = &list_hit.value.items;
        if items.is_empty() {
            break;
        }

        for item in items {
            if candidates.len() == limit || examined == max_examined {
                truncated = true;
                break 'pages;
            }
            examined += 1;

            // The detail call carries the external IDs the anti-join needs;
            // a candidate that cannot be ownership-checked is never shipped.
            let detail_hit = services.tsundoku.detail(item.id).await.map_err(degraded)?;
            tsundoku_as_of = oldest(tsundoku_as_of, detail_hit.as_of);
            let detail = detail_hit.value;

            let external_ids: Vec<ExternalId> = detail
                .external_ids
                .iter()
                .map(|e| e.normalized())
                .filter(|e| !matches!(e.provider, Provider::Other(_)))
                .collect();

            // The mandatory per-user, Codex-authoritative anti-join.
            if external_ids
                .iter()
                .any(|e| owned_index.contains_key(&canonical(e)))
            {
                continue;
            }
            if verdicts.excludes(&external_ids) {
                continue;
            }

            let mut candidate = catalog_candidate(&detail, &external_ids);
            backfill_from_mangabaka(services, &mut candidate).await;
            candidates.push(candidate);
        }

        let total = list_hit.value.total;
        if (u64::from(page) + 1) * u64::from(CATALOG_PAGE_SIZE) >= total as u64 {
            break;
        }
        page += 1;
    }

    if let Err(err) = attach_coverage(services, &mut candidates).await {
        tracing::warn!(error = %err, "coverage attach failed; acquirable candidates ship without spans");
    }

    Ok(CandidateSet {
        pool: Pool::Acquirable,
        candidates,
        as_of: AsOf {
            library: Some(index_hit.as_of),
            tsundoku: tsundoku_as_of,
        },
        truncated,
    })
}

/// What one backlog series resolved to in Tsundoku.
struct Resolved {
    series_id: i32,
    volume_coverage: Vec<CoverageSpan>,
    chapter_coverage: Vec<CoverageSpan>,
}

/// Map the backlog's external IDs to Tsundoku series via the filtered feed
/// (the only by-external-ID query Tsundoku offers), keyed by canonical
/// `provider:id`.
async fn resolve_in_tsundoku(
    services: &Services,
    backlog: &[&SnapshotSeries],
) -> Result<HashMap<String, Resolved>, TsundokuError> {
    let client = services.tsundoku.client();
    let ids: Vec<String> = backlog
        .iter()
        .flat_map(|s| s.external_ids.iter())
        .filter_map(ExternalId::as_tsundoku)
        .collect();
    if ids.is_empty() || !client.is_configured() {
        return Ok(HashMap::new());
    }

    let mut map = HashMap::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = client
            .feed_filtered(cursor.as_deref(), Some(RESOLUTION_PAGE_LIMIT), &ids)
            .await?;
        for item in &page.items {
            for eid in &item.external_ids {
                map.insert(
                    canonical(&eid.normalized()),
                    Resolved {
                        series_id: item.series_id,
                        volume_coverage: item.volume_coverage.clone(),
                        chapter_coverage: item.chapter_coverage.clone(),
                    },
                );
            }
        }
        cursor = page.next_cursor;
        if !page.has_more || cursor.is_none() {
            return Ok(map);
        }
    }
}

/// Attach coverage spans to acquirable candidates in one batched feed call.
async fn attach_coverage(
    services: &Services,
    candidates: &mut [Candidate],
) -> Result<(), TsundokuError> {
    let ids: Vec<String> = candidates
        .iter()
        .flat_map(|c| c.external_ids.iter().cloned())
        .collect();
    if ids.is_empty() {
        return Ok(());
    }

    let client = services.tsundoku.client();
    let mut by_series: HashMap<i32, (Vec<CoverageSpan>, Vec<CoverageSpan>)> = HashMap::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = client
            .feed_filtered(cursor.as_deref(), Some(RESOLUTION_PAGE_LIMIT), &ids)
            .await?;
        for item in page.items {
            by_series.insert(
                item.series_id,
                (item.volume_coverage, item.chapter_coverage),
            );
        }
        cursor = page.next_cursor;
        if !page.has_more || cursor.is_none() {
            break;
        }
    }

    for candidate in candidates.iter_mut() {
        if let Some((volumes, chapters)) = candidate
            .tsundoku_series_id
            .and_then(|id| by_series.get(&id))
            .cloned()
        {
            candidate.volume_coverage = volumes;
            candidate.chapter_coverage = chapters;
        }
    }
    Ok(())
}

/// Library-pool candidate enriched from a resolved Tsundoku series.
fn enriched_candidate(
    series: &SnapshotSeries,
    detail: &SeriesDetail,
    resolved: &Resolved,
) -> Candidate {
    let detail_ids: Vec<ExternalId> = detail
        .external_ids
        .iter()
        .map(|e| e.normalized())
        .filter(|e| !matches!(e.provider, Provider::Other(_)))
        .collect();

    Candidate {
        title: series.title.clone(),
        codex_series_id: Some(series.id.clone()),
        library: Some(series.library_name.clone()),
        tsundoku_series_id: Some(detail.id),
        mangabaka_id: mangabaka_id(series.external_ids.iter().chain(detail_ids.iter())),
        external_ids: union_canonical(series.external_ids.iter().chain(detail_ids.iter())),
        genres: union_strings(&series.genres, &detail.genres),
        tags: union_strings(&series.tags, &detail.tags),
        rating: detail.rating,
        description: detail
            .description
            .clone()
            .or_else(|| series.summary.clone()),
        status: detail.status.clone().or_else(|| series.status.clone()),
        year: detail.year.or(series.year),
        volume_coverage: resolved.volume_coverage.clone(),
        chapter_coverage: resolved.chapter_coverage.clone(),
        enrichment: Enrichment::Full,
    }
}

/// Library-pool candidate with Codex metadata only (PRD Q8: surfaced
/// honestly so the model knows to verify via web research).
fn codex_only_candidate(series: &SnapshotSeries) -> Candidate {
    Candidate {
        title: series.title.clone(),
        codex_series_id: Some(series.id.clone()),
        library: Some(series.library_name.clone()),
        tsundoku_series_id: None,
        mangabaka_id: mangabaka_id(series.external_ids.iter()),
        external_ids: union_canonical(series.external_ids.iter()),
        genres: series.genres.clone(),
        tags: series.tags.clone(),
        rating: None,
        description: series.summary.clone(),
        status: series.status.clone(),
        year: series.year,
        volume_coverage: Vec::new(),
        chapter_coverage: Vec::new(),
        enrichment: Enrichment::None,
    }
}

/// Acquirable-pool candidate from a catalog detail. Deliberately never
/// reads `detail.owned` or `detail.codex` — overlay facts are not relayed.
fn catalog_candidate(detail: &SeriesDetail, external_ids: &[ExternalId]) -> Candidate {
    Candidate {
        title: detail.canonical_title.clone(),
        codex_series_id: None,
        library: None,
        tsundoku_series_id: Some(detail.id),
        mangabaka_id: mangabaka_id(external_ids.iter()),
        external_ids: union_canonical(external_ids.iter()),
        genres: detail.genres.clone(),
        tags: detail.tags.clone(),
        rating: detail.rating,
        description: detail.description.clone(),
        status: detail.status.clone(),
        year: detail.year,
        volume_coverage: Vec::new(),
        chapter_coverage: Vec::new(),
        enrichment: Enrichment::Full,
    }
}

/// Fill metadata gaps from the MangaBaka backbone: a Codex-only candidate
/// gains description/genres/tags/rating where the dump has the series
/// (and is promoted to `enrichment: mangabaka`), and no candidate ships
/// with an empty description when the dump has one. Tsundoku-sourced
/// fields are never overwritten — the backbone only fills what is missing.
async fn backfill_from_mangabaka(services: &Services, candidate: &mut Candidate) {
    let codex_only = candidate.enrichment == Enrichment::None;
    if !codex_only && candidate.description.is_some() {
        return;
    }
    let ids: Vec<ExternalId> = candidate
        .external_ids
        .iter()
        .filter_map(|s| ExternalId::from_tsundoku(s).ok())
        .filter(|id| !matches!(id.provider, Provider::Other(_)))
        .collect();
    let Some(meta) = services.metadata.resolve_any(ids.iter()).await else {
        return;
    };

    if candidate.description.is_none() {
        candidate.description = meta.description.clone();
    }
    if candidate.rating.is_none() {
        candidate.rating = meta.rating;
    }
    if candidate.genres.is_empty() {
        candidate.genres = meta.genres.clone();
    }
    if candidate.tags.is_empty() {
        candidate.tags = meta.tags.clone();
    }
    if candidate.status.is_none() {
        candidate.status = meta.status.map(|s| s.as_str().to_owned());
    }
    if candidate.year.is_none() {
        candidate.year = meta.year;
    }
    if candidate.mangabaka_id.is_none() {
        candidate.mangabaka_id = Some(meta.mangabaka_id);
        let token = format!("mangabaka:{}", meta.mangabaka_id);
        if !candidate.external_ids.contains(&token) {
            candidate.external_ids.push(token);
        }
    }
    if codex_only {
        candidate.enrichment = Enrichment::Mangabaka;
    }
}

fn canonical(id: &ExternalId) -> String {
    id.as_tsundoku()
        .unwrap_or_else(|| format!("{}:{}", id.provider, id.id))
}

fn mangabaka_id<'a>(ids: impl Iterator<Item = &'a ExternalId>) -> Option<i64> {
    let mut ids = ids;
    ids.find(|id| id.provider == Provider::Mangabaka)
        .and_then(|id| id.id.parse().ok())
}

fn union_canonical<'a>(ids: impl Iterator<Item = &'a ExternalId>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        let s = canonical(id);
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

fn union_strings(a: &[String], b: &[String]) -> Vec<String> {
    let mut out = a.to_vec();
    for s in b {
        if !out.iter().any(|x| x.eq_ignore_ascii_case(s)) {
            out.push(s.clone());
        }
    }
    out
}

fn oldest(current: Option<DateTime<Utc>>, new: DateTime<Utc>) -> Option<DateTime<Utc>> {
    Some(match current {
        Some(t) => t.min(new),
        None => new,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_schema_is_a_flat_inline_string_enum_not_oneof() {
        // claude.ai's tool-input serializer drops `oneOf`/`const` enums to
        // null; the `pool` parameter must be a flat string enum.
        let schema = serde_json::to_value(schemars::schema_for!(Pool)).unwrap();
        assert!(
            schema.get("oneOf").is_none(),
            "Pool must not use oneOf: {schema}"
        );
        assert_eq!(schema["type"], "string");
        let values: Vec<&str> = schema["enum"]
            .as_array()
            .expect("flat enum array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(values, ["library", "acquirable"]);
    }
}
