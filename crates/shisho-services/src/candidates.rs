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
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};

use crate::affinity::{
    Anchor, OutcomeSignal, Score, TasteVector, by_mangabaka_id, franchise_adjustment,
};
use crate::library_snapshot::{self, CodexIndex, SnapshotSeries};
use crate::profile_filters::ProfileFilters;
use crate::read_state::ReadState;
use crate::tsundoku_cache::Resolved;
use crate::verdicts::Outcome;
use crate::verdicts::VerdictIndex;
use crate::{Services, UserCtx};
use shisho_cache::SwrError;
use shisho_codex::CodexError;
use shisho_db::repos::verdicts::{self as verdict_repo, verdict_series_token};
use shisho_ids::{ExternalId, Provider};
use shisho_mangabaka::offline::DiscoverQuery;
use shisho_mangabaka::{ContentRating, SeriesMetadata};
use shisho_tsundoku::{CoverageSpan, Order, SeriesDetail, SeriesListQuery, TsundokuError};

/// Default over-provision target (PRD: 30-50).
const DEFAULT_LIMIT: usize = 40;
/// Hard cap on the over-provision target.
const MAX_LIMIT: usize = 100;
/// Acquirable pool: catalog page size per cached list query.
const CATALOG_PAGE_SIZE: u32 = 50;
/// Acquirable pool: catalog series examined by rating per wanted candidate,
/// with a floor and a cap on the slice (bounds work when filters reject much).
const ACQUIRABLE_SLICE_FACTOR: usize = 3;
const ACQUIRABLE_MIN_SLICE: usize = 150;
const ACQUIRABLE_MAX_SLICE: usize = 300;
/// Acquirable pool score weights: taste, community rating (0-1), and novelty
/// (among the newest catalog series). Initial values; tune with `shisho eval`
/// once outcome data exists.
const ACQUIRABLE_W_AFFINITY: f64 = 0.6;
const ACQUIRABLE_W_COMMUNITY: f64 = 0.3;
const ACQUIRABLE_W_NOVELTY: f64 = 0.1;
/// Diversity trade-off: 1.0 is pure score order.
const MMR_LAMBDA: f64 = 0.7;
/// Descriptions ship trimmed to about this many characters; `lookup` has the
/// full text.
const DESCRIPTION_CHARS: usize = 280;
/// Anchors shipped per candidate.
const MAX_BECAUSE: usize = 3;
/// Tags shipped per candidate.
const MAX_TAGS: usize = 8;
/// Discover pool: comic formats in scope (novels are out).
const DISCOVER_KINDS: &[&str] = &["manga", "manhwa", "manhua"];
/// Discover pool: community rating floor on MangaBaka's 0-100 scale, so
/// obscure, barely rated series do not crowd the list.
const DISCOVER_MIN_RATING: f64 = 65.0;
/// Discover pool: the caller's strongest liked terms sent to the dump query.
const DISCOVER_QUERY_TERMS: usize = 30;
/// Discover pool: rows fetched per wanted candidate, and the absolute cap.
const DISCOVER_OVERFETCH: usize = 4;
const DISCOVER_MAX_FETCH: usize = 500;
/// Discover pool: weight of the community rating (0-10) in the final score.
const DISCOVER_RATING_PRIOR: f64 = 0.05;
/// Recorded outcomes (most recent first, one per series) folded into taste.
const OUTCOME_SIGNALS: usize = 50;
/// Backend calls in flight per pool while enriching candidates.
const FAN_OUT: usize = 8;

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
    // MangaBaka dump series matching the caller's taste, minus owned.
    Discover,
}

impl Pool {
    fn name(self) -> &'static str {
        match self {
            Pool::Library => "library",
            Pool::Acquirable => "acquirable",
            Pool::Discover => "discover",
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
    /// Ranked candidates to skip, for paging past the first slice.
    pub offset: Option<usize>,
    /// Include series MangaBaka rates pornographic (excluded by default).
    pub allow_explicit: bool,
    /// The caller's profile filters; loaded server-side, never caller input.
    #[serde(skip)]
    pub profile: ProfileFilters,
}

impl Filters {
    fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }

    /// The hard gates: pornographic series stay out unless the caller opted
    /// in (an unknown rating passes), and so does any series carrying a genre
    /// or tag the caller's profile blocks.
    fn gates_out(&self, candidate: &Candidate) -> bool {
        let explicit = !self.allow_explicit
            && candidate.content_rating.as_deref() == Some(ContentRating::Pornographic.as_str());
        explicit
            || self
                .profile
                .blocks(&candidate.genres, &candidate.tags)
                .is_some()
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
    /// MangaBaka's content rating: safe, suggestive, erotica, or pornographic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_rating: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub volume_coverage: Vec<CoverageSpan>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub chapter_coverage: Vec<CoverageSpan>,
    pub enrichment: Enrichment,
    /// Affinity to the caller's taste (higher is closer); the pool is ordered
    /// by it. Absent when the caller has no ratings or completed series yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// The caller's own series that explain the score, with the genres and
    /// tags shared.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub because: Vec<Anchor>,
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
        "the {pool} pool is not scoped by Codex libraries (it draws from outside your \
         library and excludes everything you own); omit `libraries` for pool={pool}"
    )]
    LibrariesNotApplicable { pool: &'static str },
    /// The discover pool reads the MangaBaka offline dump, which this
    /// instance has not loaded (API-only mode, or the first refresh is
    /// still running).
    #[error(
        "the discover pool needs the MangaBaka offline dump, which is not loaded; \
         use pool=library or pool=acquirable"
    )]
    DiscoverUnavailable,
    #[error("verdict query failed")]
    Db(#[from] sea_orm::DbErr),
}

pub async fn get_candidates(
    services: &Services,
    user: &UserCtx,
    pool: Pool,
    filters: &Filters,
) -> Result<CandidateSet, CandidatesError> {
    // The profile's hard filters apply to every pool.
    let profile = shisho_db::repos::profiles::get_or_default(&services.db, &user.user_sub).await?;
    let filters = &Filters {
        profile: ProfileFilters::parse(&profile.content),
        ..filters.clone()
    };
    match pool {
        Pool::Library => library_pool(services, user, filters).await,
        Pool::Acquirable => acquirable_pool(services, user, filters).await,
        Pool::Discover => discover_pool(services, user, filters).await,
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

    let eligible: Vec<&SnapshotSeries> = snapshot
        .series
        .iter()
        .filter(|s| in_selected_libraries(s))
        .filter(|s| s.read_state == ReadState::Untouched)
        .filter(|s| !verdicts.excludes(&s.external_ids))
        .collect();

    // Rank the whole eligible backlog from the snapshot's own genres and tags
    // (no backend calls), then enrich only as far as the requested page.
    // Ties, and every series on a cold start, keep the library's own order.
    let taste = taste_for(services, &user.user_sub, &snapshot).await;
    let library = by_mangabaka_id(&snapshot);
    let eligible_ids: Vec<i64> = eligible
        .iter()
        .filter_map(|s| mangabaka_id(s.external_ids.iter()))
        .collect();
    let links = franchise_links(services, &eligible_ids).await;
    let mut ranked: Vec<(&SnapshotSeries, Option<Score>)> = eligible
        .into_iter()
        .map(|s| {
            let mut score = (!taste.is_empty()).then(|| Score {
                affinity: taste.affinity(&s.genres, &s.tags, &s.authors),
                anchors: Vec::new(),
            });
            if let Some(series_links) =
                mangabaka_id(s.external_ids.iter()).and_then(|id| links.get(&id))
            {
                apply_franchise(&mut score, series_links, &library);
            }
            (s, score)
        })
        .collect();
    ranked.sort_by(|a, b| {
        let affinity = |x: &Option<Score>| x.as_ref().map_or(0.0, |s| s.affinity);
        affinity(&b.1).total_cmp(&affinity(&a.1))
    });
    let backlog: Vec<&SnapshotSeries> = ranked.iter().map(|(s, _)| *s).collect();

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
    let mut skip = filters.offset.unwrap_or(0);
    let mut candidates = Vec::new();
    let mut tsundoku_as_of: Option<DateTime<Utc>> = None;
    let mut truncated = false;

    // Enrichment runs a bounded number of series ahead of the consumer, so
    // stopping at `limit` stops the backend calls too. The futures are built
    // up front but only run when the buffered stream polls them (mapping
    // inside the stream trips a higher-ranked lifetime error once callers
    // require `Send`).
    let pending: Vec<_> = ranked
        .iter()
        .map(|(series, score)| enrich_backlog_series(services, &resolution, series, score.as_ref()))
        .collect();
    let mut enriched = stream::iter(pending).buffered(FAN_OUT);

    while let Some((candidate, as_of)) = enriched.next().await {
        if let Some(as_of) = as_of {
            tsundoku_as_of = oldest(tsundoku_as_of, as_of);
        }
        if filters.gates_out(&candidate)
            || !filters.matches(
                &candidate.genres,
                &candidate.tags,
                candidate.status.as_deref(),
            )
        {
            continue;
        }
        if skip > 0 {
            skip -= 1;
            continue;
        }
        if candidates.len() == limit {
            truncated = true;
            break;
        }
        candidates.push(candidate);
    }

    for candidate in &mut candidates {
        // Coverage is an acquisition concern; the caller already owns these.
        candidate.volume_coverage.clear();
        candidate.chapter_coverage.clear();
        explain(candidate, &taste);
        compact(candidate, Some(&taste));
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

/// The Tier 1 pool: Tsundoku's catalog minus everything the caller owns,
/// re-ranked for the caller. A wide slice (the best-rated series plus the
/// newest) is examined, scored by taste, community rating and novelty, then
/// diversified so one genre does not take the whole page.
async fn acquirable_pool(
    services: &Services,
    user: &UserCtx,
    filters: &Filters,
) -> Result<CandidateSet, CandidatesError> {
    if !filters.libraries.is_empty() {
        return Err(CandidatesError::LibrariesNotApplicable {
            pool: Pool::Acquirable.name(),
        });
    }
    // Ownership comes from the slim cached Codex index (an external id that
    // keys into it is owned); the full snapshot supplies the taste.
    let index_hit = services
        .snapshots
        .external_index(&user.user_sub, &user.credential)
        .await
        .map_err(CandidatesError::Codex)?;
    let owned_index = index_hit.value;
    let snapshot = services
        .snapshots
        .get(&user.user_sub, &user.credential)
        .await
        .map_err(CandidatesError::Codex)?
        .value;
    let verdicts = VerdictIndex::load(&services.db, &user.user_sub).await?;
    let taste = taste_for(services, &user.user_sub, &snapshot).await;

    let degraded = |source| CandidatesError::TsundokuDegraded {
        pool: Pool::Acquirable.name(),
        source,
    };

    let limit = filters.limit();
    let offset = filters.offset.unwrap_or(0);
    let wanted = offset + limit;
    let by_rating =
        (wanted * ACQUIRABLE_SLICE_FACTOR).clamp(ACQUIRABLE_MIN_SLICE, ACQUIRABLE_MAX_SLICE);

    // The slice: catalog ids in rating order, then the newest page; an id
    // seen in both counts as rating-sourced and novel.
    let mut tsundoku_as_of: Option<DateTime<Utc>> = None;
    let mut slice: Vec<i32> = Vec::new();
    let mut catalog_has_more = false;
    let mut page: u32 = 0;
    while slice.len() < by_rating {
        let list_hit = services
            .tsundoku
            .list(&catalog_query(services, filters, page, "rating"))
            .await
            .map_err(degraded)?;
        tsundoku_as_of = oldest(tsundoku_as_of, list_hit.as_of);
        let items = &list_hit.value.items;
        if items.is_empty() {
            break;
        }
        slice.extend(items.iter().map(|i| i.id));
        if !page_has_more(page, list_hit.value.total) {
            break;
        }
        catalog_has_more = true;
        page += 1;
    }
    catalog_has_more &= slice.len() >= by_rating;
    let newest_hit = services
        .tsundoku
        .list(&catalog_query(services, filters, 0, "published_start_date"))
        .await
        .map_err(degraded)?;
    tsundoku_as_of = oldest(tsundoku_as_of, newest_hit.as_of);
    let novel: std::collections::HashSet<i32> =
        newest_hit.value.items.iter().map(|i| i.id).collect();
    for item in &newest_hit.value.items {
        if !slice.contains(&item.id) {
            slice.push(item.id);
        }
    }

    let pending: Vec<_> = slice
        .iter()
        .map(|&id| catalog_item_candidate(services, &owned_index, &verdicts, filters, id))
        .collect();
    let mut details = stream::iter(pending).buffered(FAN_OUT);
    let mut pool: Vec<Candidate> = Vec::new();
    while let Some(result) = details.next().await {
        let (candidate, as_of) = result.map_err(degraded)?;
        tsundoku_as_of = oldest(tsundoku_as_of, as_of);
        pool.extend(candidate);
    }

    let library = by_mangabaka_id(&snapshot);
    let ids: Vec<i64> = pool.iter().filter_map(|c| c.mangabaka_id).collect();
    let links = franchise_links(services, &ids).await;
    let mut scored: Vec<(f64, Candidate)> = pool
        .into_iter()
        .map(|mut candidate| {
            let mut score = Some(Score {
                affinity: taste.affinity(&candidate.genres, &candidate.tags, &candidate.authors),
                anchors: Vec::new(),
            });
            if let Some(series_links) = candidate.mangabaka_id.and_then(|id| links.get(&id)) {
                apply_franchise(&mut score, series_links, &library);
            }
            let score = score.unwrap_or_else(|| Score {
                affinity: 0.0,
                anchors: Vec::new(),
            });
            let community = candidate.rating.unwrap_or(0.0) / 10.0;
            let novelty = if candidate
                .tsundoku_series_id
                .is_some_and(|id| novel.contains(&id))
            {
                1.0
            } else {
                0.0
            };
            let total = ACQUIRABLE_W_AFFINITY * score.affinity
                + ACQUIRABLE_W_COMMUNITY * community
                + ACQUIRABLE_W_NOVELTY * novelty;
            candidate.score = Some(round3(total));
            candidate.because = score.anchors;
            (total, candidate)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let ranked = diversify(scored, wanted);
    let truncated = ranked.len() > wanted || catalog_has_more;
    let mut candidates: Vec<Candidate> = ranked.into_iter().skip(offset).take(limit).collect();

    if let Err(err) = attach_coverage(services, &mut candidates).await {
        tracing::warn!(error = %err, "coverage attach failed; acquirable candidates ship without spans");
    }
    for candidate in &mut candidates {
        explain(candidate, &taste);
        compact(candidate, Some(&taste));
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

/// One page of the Tsundoku catalog for the acquirable pool, sorted by `sort`.
fn catalog_query(services: &Services, filters: &Filters, page: u32, sort: &str) -> SeriesListQuery {
    SeriesListQuery {
        page: Some(page),
        page_size: Some(CATALOG_PAGE_SIZE),
        kind: Some("manga".into()),
        has_releases: Some(true),
        status: filters.status.clone(),
        genres: (!filters.genres.is_empty()).then(|| filters.genres.clone()),
        tags: (!filters.tags.is_empty()).then(|| filters.tags.clone()),
        sort: Some(sort.into()),
        order: Some(Order::Desc),
        // Bandwidth pre-filter only, and only when the deployment opted in;
        // the anti-join is the correctness gate.
        owned: services.tsundoku_owned_prefilter.then_some(false),
        ..Default::default()
    }
}

/// Maximal-marginal-relevance reordering over genre sets: repeatedly take the
/// candidate with the best `lambda * score - (1 - lambda) * similarity`,
/// where similarity is the highest genre overlap (Jaccard) with anything
/// already taken. Orders the first `wanted` (plus one, so truncation can be
/// detected); the rest keep score order.
fn diversify(mut scored: Vec<(f64, Candidate)>, wanted: usize) -> Vec<Candidate> {
    let mut taken: Vec<Candidate> = Vec::with_capacity(scored.len());
    while !scored.is_empty() && taken.len() <= wanted {
        let best = scored
            .iter()
            .enumerate()
            .map(|(i, (score, candidate))| {
                let similarity = taken
                    .iter()
                    .map(|t| genre_overlap(&t.genres, &candidate.genres))
                    .fold(0.0, f64::max);
                (i, MMR_LAMBDA * score - (1.0 - MMR_LAMBDA) * similarity)
            })
            .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
            .map(|(i, _)| i)
            .expect("non-empty");
        taken.push(scored.remove(best).1);
    }
    taken.extend(scored.into_iter().map(|(_, c)| c));
    taken
}

/// Jaccard overlap of two genre lists, case-insensitive.
fn genre_overlap(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let a: std::collections::HashSet<String> = a.iter().map(|g| g.to_lowercase()).collect();
    let b: std::collections::HashSet<String> = b.iter().map(|g| g.to_lowercase()).collect();
    a.intersection(&b).count() as f64 / a.union(&b).count() as f64
}

/// The discover pool: MangaBaka dump series in the comic formats, ranked by
/// the caller's taste and anti-joined against everything they own. Needs no
/// Tsundoku; where Tsundoku has a series, its id and coverage are attached.
async fn discover_pool(
    services: &Services,
    user: &UserCtx,
    filters: &Filters,
) -> Result<CandidateSet, CandidatesError> {
    if !filters.libraries.is_empty() {
        return Err(CandidatesError::LibrariesNotApplicable {
            pool: Pool::Discover.name(),
        });
    }
    if !services.metadata.provider().offline_loaded().await {
        return Err(CandidatesError::DiscoverUnavailable);
    }
    let snapshot_hit = services
        .snapshots
        .get(&user.user_sub, &user.credential)
        .await
        .map_err(CandidatesError::Codex)?;
    let index_hit = services
        .snapshots
        .external_index(&user.user_sub, &user.credential)
        .await
        .map_err(CandidatesError::Codex)?;
    let owned_index = index_hit.value;
    let verdicts = VerdictIndex::load(&services.db, &user.user_sub).await?;
    let taste = taste_for(services, &user.user_sub, &snapshot_hit.value).await;

    let limit = filters.limit();
    let offset = filters.offset.unwrap_or(0);
    // Over-fetch: owned, verdicted and filtered-out rows fall away below.
    let fetch = ((offset + limit) * DISCOVER_OVERFETCH).min(DISCOVER_MAX_FETCH);
    let weights = taste.top_terms(DISCOVER_QUERY_TERMS);
    let query = DiscoverQuery {
        weights: &weights,
        kinds: DISCOVER_KINDS,
        allow_explicit: filters.allow_explicit,
        min_rating: DISCOVER_MIN_RATING,
        limit: u32::try_from(fetch).unwrap_or(u32::MAX),
    };
    let found = match services.metadata.provider().discover(&query).await {
        Ok(Some(found)) => found,
        Ok(None) => return Err(CandidatesError::DiscoverUnavailable),
        Err(err) => {
            tracing::warn!(error = %err, "dump discovery failed");
            return Err(CandidatesError::DiscoverUnavailable);
        }
    };

    let library = by_mangabaka_id(&snapshot_hit.value);
    let found_ids: Vec<i64> = found.iter().map(|m| m.mangabaka_id).collect();
    let links = franchise_links(services, &found_ids).await;
    let mut scored: Vec<(f64, Candidate)> = Vec::new();
    for meta in found {
        let mut ids = vec![ExternalId {
            provider: Provider::Mangabaka,
            id: meta.mangabaka_id.to_string(),
            provenance: None,
        }];
        ids.extend(meta.foreign_ids.iter().cloned());
        if ids
            .iter()
            .any(|id| owned_index.contains_key(&id.canonical_key()))
            || verdicts.excludes(&ids)
        {
            continue;
        }
        let candidate = dump_candidate(&meta, &ids);
        if filters.gates_out(&candidate)
            || !filters.matches(
                &candidate.genres,
                &candidate.tags,
                candidate.status.as_deref(),
            )
        {
            continue;
        }
        let mut score = Some(Score {
            affinity: taste.affinity(&candidate.genres, &candidate.tags, &candidate.authors),
            anchors: Vec::new(),
        });
        if let Some(series_links) = links.get(&meta.mangabaka_id) {
            apply_franchise(&mut score, series_links, &library);
        }
        let score = score.unwrap_or_else(|| Score {
            affinity: 0.0,
            anchors: Vec::new(),
        });
        // A small community-rating prior separates equally matching series.
        let total = score.affinity + DISCOVER_RATING_PRIOR * candidate.rating.unwrap_or(0.0) / 10.0;
        let mut candidate = candidate;
        candidate.score = Some(round3(total));
        candidate.because = score.anchors;
        scored.push((total, candidate));
    }
    // Stable: ties keep the dump's own order (summed weight, then rating).
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let truncated = scored.len() > offset + limit;
    let mut candidates: Vec<Candidate> = scored
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(_, c)| c)
        .collect();

    let mut tsundoku_as_of = None;
    let tokens: Vec<String> = candidates
        .iter()
        .flat_map(|c| c.external_ids.iter().cloned())
        .collect();
    match services.tsundoku.resolve_external_ids(&tokens).await {
        Ok(resolved) if !resolved.is_empty() => {
            tsundoku_as_of = Some(Utc::now());
            for candidate in &mut candidates {
                if let Some(hit) = candidate
                    .external_ids
                    .iter()
                    .find_map(|id| resolved.get(id))
                {
                    candidate.tsundoku_series_id = Some(hit.series_id);
                    candidate.volume_coverage = hit.volume_coverage.clone();
                    candidate.chapter_coverage = hit.chapter_coverage.clone();
                }
            }
        }
        Ok(_) => {}
        Err(err) => {
            tracing::debug!(error = %err, "Tsundoku resolution failed; discover candidates ship without coverage");
        }
    }
    for candidate in &mut candidates {
        explain(candidate, &taste);
        compact(candidate, Some(&taste));
    }

    Ok(CandidateSet {
        pool: Pool::Discover,
        candidates,
        as_of: AsOf {
            library: Some(snapshot_hit.as_of),
            tsundoku: tsundoku_as_of,
        },
        truncated,
    })
}

/// The caller's taste: their ratings and finished series, their recorded
/// reading outcomes, and term specificity within their own library (rare
/// shared tags count for more than ubiquitous ones).
async fn taste_for(
    services: &Services,
    user_sub: &str,
    snapshot: &library_snapshot::LibrarySnapshot,
) -> TasteVector {
    let outcomes = outcome_signals(services, user_sub, snapshot).await;
    let mut taste = TasteVector::from_library(snapshot, &outcomes);
    if taste.is_empty() {
        return taste;
    }
    taste.apply_library_specificity(snapshot);
    taste
}

/// The caller's most recent reading outcomes, one per series (the latest
/// wins), as taste evidence. Owned series take their metadata from the
/// snapshot, others from the backbone; a series neither knows is skipped,
/// and a verdict-log read failure leaves taste to ratings alone.
async fn outcome_signals(
    services: &Services,
    user_sub: &str,
    snapshot: &library_snapshot::LibrarySnapshot,
) -> Vec<OutcomeSignal> {
    let rows = match verdict_repo::recent_outcomes(&services.db, user_sub, None).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(error = %err, "reading recorded outcomes failed; taste from ratings only");
            return Vec::new();
        }
    };
    // Oldest first, so later rows overwrite earlier ones per series.
    let mut latest: HashMap<String, (Outcome, i64)> = HashMap::new();
    for row in &rows {
        if let (Some(token), Some(outcome)) = (verdict_series_token(row), row.outcome.as_deref()) {
            latest.insert(token, (Outcome::parse(outcome), row.id));
        }
    }
    let mut recent: Vec<(String, Outcome, i64)> = latest
        .into_iter()
        .map(|(token, (outcome, id))| (token, outcome, id))
        .filter(|(_, outcome, _)| !matches!(outcome, Outcome::Mixed | Outcome::Other))
        .collect();
    recent.sort_by_key(|(_, _, id)| std::cmp::Reverse(*id));
    recent.truncate(OUTCOME_SIGNALS);

    let pending = recent.into_iter().filter_map(|(token, outcome, _)| {
        let id = ExternalId::from_tsundoku(&token).ok()?;
        Some(async move {
            if let Some(owned) = snapshot.series_by_external_id(&id) {
                return Some(OutcomeSignal {
                    title: owned.title.clone(),
                    codex_series_id: Some(owned.id.clone()),
                    outcome,
                    genres: owned.genres.clone(),
                    tags: owned.tags.clone(),
                    authors: owned.authors.clone(),
                });
            }
            let meta = services.metadata.resolve(&id).await.ok().flatten()?;
            Some(OutcomeSignal {
                title: meta.title,
                codex_series_id: None,
                outcome,
                genres: meta.genres,
                tags: meta.tags,
                authors: meta.authors,
            })
        })
    });
    futures_util::future::join_all(pending)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// Franchise links for the given MangaBaka ids; empty (no adjustment) when
/// the dump is absent or the lookup fails.
async fn franchise_links(services: &Services, ids: &[i64]) -> HashMap<i64, Vec<(String, i64)>> {
    match services.metadata.provider().relations(ids).await {
        Ok(links) => links,
        Err(err) => {
            tracing::debug!(error = %err, "franchise lookup failed; ranking without it");
            HashMap::new()
        }
    }
}

/// Shift a candidate's score for its franchise position, putting the
/// franchise anchor first. A candidate with no taste score yet gains one.
fn apply_franchise(
    score: &mut Option<Score>,
    links: &[(String, i64)],
    library: &HashMap<i64, &SnapshotSeries>,
) {
    let (shift, anchor) = franchise_adjustment(links, library);
    if shift == 0.0 {
        return;
    }
    let score = score.get_or_insert_with(|| Score {
        affinity: 0.0,
        anchors: Vec::new(),
    });
    score.affinity += shift;
    if let Some(anchor) = anchor {
        score.anchors.insert(0, anchor);
    }
}

/// A discover-pool candidate straight from the backbone.
fn dump_candidate(meta: &SeriesMetadata, ids: &[ExternalId]) -> Candidate {
    Candidate {
        title: meta.title.clone(),
        codex_series_id: None,
        library: None,
        tsundoku_series_id: None,
        mangabaka_id: Some(meta.mangabaka_id),
        external_ids: union_canonical(ids.iter()),
        genres: meta.genres.clone(),
        tags: meta.tags.clone(),
        rating: meta.rating,
        description: meta.description.clone(),
        status: meta.status.map(|s| s.as_str().to_owned()),
        year: meta.year,
        content_rating: meta.content_rating.map(|r| r.as_str().to_owned()),
        authors: meta.authors.clone(),
        volume_coverage: Vec::new(),
        chapter_coverage: Vec::new(),
        enrichment: Enrichment::Mangabaka,
        score: None,
        because: Vec::new(),
    }
}

/// One backlog series as a candidate: Tsundoku-enriched where it resolves,
/// Codex-only otherwise, then backfilled from the backbone. Returns the
/// Tsundoku data age when Tsundoku contributed.
async fn enrich_backlog_series(
    services: &Services,
    resolution: &HashMap<String, Resolved>,
    series: &SnapshotSeries,
    score: Option<&Score>,
) -> (Candidate, Option<DateTime<Utc>>) {
    let resolved = series
        .external_ids
        .iter()
        .find_map(|id| resolution.get(&id.canonical_key()));
    let (mut candidate, as_of) = match resolved {
        Some(resolved) => match services.tsundoku.detail(resolved.series_id).await {
            Ok(hit) => (
                enriched_candidate(series, &hit.value, resolved),
                Some(hit.as_of),
            ),
            Err(err) => {
                tracing::warn!(series = %series.title, error = %err,
                    "Tsundoku detail failed; candidate degrades to enrichment=none");
                (codex_only_candidate(series), None)
            }
        },
        None => (codex_only_candidate(series), None),
    };
    // Backbone backfill before the filter pass, so dump genres/status
    // participate in matching.
    backfill_from_mangabaka(services, &mut candidate).await;
    if let Some(score) = score {
        candidate.score = Some(round3(score.affinity));
        candidate.because = score.anchors.clone();
    }
    (candidate, as_of)
}

/// One catalog item as an acquirable candidate, or `None` when the caller
/// owns it or a verdict settles it. Returns the detail's data age either way.
async fn catalog_item_candidate(
    services: &Services,
    owned_index: &CodexIndex,
    verdicts: &VerdictIndex,
    filters: &Filters,
    tsundoku_id: i32,
) -> Result<(Option<Candidate>, DateTime<Utc>), SwrError<TsundokuError>> {
    // The detail call carries the external IDs the anti-join needs; a
    // candidate that cannot be ownership-checked is never shipped.
    let detail_hit = services.tsundoku.detail(tsundoku_id).await?;
    let detail = &detail_hit.value;
    let external_ids: Vec<ExternalId> = detail
        .external_ids
        .iter()
        .map(|e| e.normalized())
        .filter(ExternalId::is_known)
        .collect();

    // The mandatory per-user, Codex-authoritative anti-join.
    let owned = external_ids
        .iter()
        .any(|e| owned_index.contains_key(&e.canonical_key()));
    if owned || verdicts.excludes(&external_ids) {
        return Ok((None, detail_hit.as_of));
    }
    let mut candidate = catalog_candidate(detail, &external_ids);
    backfill_from_mangabaka(services, &mut candidate).await;
    if filters.gates_out(&candidate) {
        return Ok((None, detail_hit.as_of));
    }
    Ok((Some(candidate), detail_hit.as_of))
}

/// Map the backlog's external IDs to Tsundoku series (and their coverage),
/// keyed by canonical `provider:id`.
async fn resolve_in_tsundoku(
    services: &Services,
    backlog: &[&SnapshotSeries],
) -> Result<HashMap<String, Resolved>, TsundokuError> {
    let ids: Vec<String> = backlog
        .iter()
        .flat_map(|s| s.external_ids.iter())
        .filter_map(ExternalId::as_tsundoku)
        .collect();
    services.tsundoku.resolve_external_ids(&ids).await
}

/// Attach coverage spans to acquirable candidates in one batched feed pass.
async fn attach_coverage(
    services: &Services,
    candidates: &mut [Candidate],
) -> Result<(), TsundokuError> {
    let ids: Vec<String> = candidates
        .iter()
        .flat_map(|c| c.external_ids.iter().cloned())
        .collect();
    let by_series: HashMap<i32, Resolved> = services
        .tsundoku
        .resolve_external_ids(&ids)
        .await?
        .into_values()
        .map(|resolved| (resolved.series_id, resolved))
        .collect();

    for candidate in candidates.iter_mut() {
        if let Some(resolved) = candidate
            .tsundoku_series_id
            .and_then(|id| by_series.get(&id))
        {
            candidate.volume_coverage = resolved.volume_coverage.clone();
            candidate.chapter_coverage = resolved.chapter_coverage.clone();
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
        .filter(ExternalId::is_known)
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
        content_rating: None,
        authors: Vec::new(),
        volume_coverage: resolved.volume_coverage.clone(),
        chapter_coverage: resolved.chapter_coverage.clone(),
        enrichment: Enrichment::Full,
        score: None,
        because: Vec::new(),
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
        content_rating: None,
        authors: Vec::new(),
        volume_coverage: Vec::new(),
        chapter_coverage: Vec::new(),
        enrichment: Enrichment::None,
        score: None,
        because: Vec::new(),
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
        content_rating: None,
        authors: Vec::new(),
        volume_coverage: Vec::new(),
        chapter_coverage: Vec::new(),
        enrichment: Enrichment::Full,
        score: None,
        because: Vec::new(),
    }
}

/// Fill metadata from the MangaBaka backbone: every candidate takes its
/// content rating and authors from it (the content gate needs the rating),
/// a Codex-only candidate gains description/genres/tags/rating where the
/// backbone has the series (and is promoted to `enrichment: mangabaka`), and
/// no candidate ships with an empty description when the backbone has one.
/// Tsundoku-sourced fields are never overwritten.
async fn backfill_from_mangabaka(services: &Services, candidate: &mut Candidate) {
    let codex_only = candidate.enrichment == Enrichment::None;
    let ids: Vec<ExternalId> = candidate
        .external_ids
        .iter()
        .filter_map(|s| ExternalId::from_tsundoku(s).ok())
        .filter(ExternalId::is_known)
        .collect();
    let Some(meta) = services.metadata.resolve_any(ids.iter()).await else {
        return;
    };

    candidate.content_rating = meta.content_rating.map(|r| r.as_str().to_owned());
    if candidate.authors.is_empty() {
        candidate.authors = meta.authors.clone();
    }
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

fn mangabaka_id<'a>(ids: impl Iterator<Item = &'a ExternalId>) -> Option<i64> {
    let mut ids = ids;
    ids.find(|id| id.provider == Provider::Mangabaka)
        .and_then(|id| id.id.parse().ok())
}

fn union_canonical<'a>(ids: impl Iterator<Item = &'a ExternalId>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        let s = id.canonical_key();
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

/// Whether the catalog has items beyond `page`.
fn page_has_more(page: u32, total: i64) -> bool {
    (i64::from(page) + 1) * i64::from(CATALOG_PAGE_SIZE) < total
}

/// Attach the taste anchors explaining a returned candidate, after any
/// franchise anchor ranking already put first. Ranking itself only needs the
/// affinity; anchors are built for the page alone.
fn explain(candidate: &mut Candidate, taste: &TasteVector) {
    for anchor in taste.explain(&candidate.genres, &candidate.tags, &candidate.authors) {
        if !candidate.because.iter().any(|a| a.title == anchor.title) {
            candidate.because.push(anchor);
        }
    }
    candidate.because.truncate(MAX_BECAUSE);
}

/// Trim a candidate to what ranking and verification need: a short
/// description, the tags that matter most to the caller, and at most two
/// external ids (MangaBaka first). Full metadata is one `lookup` away.
fn compact(candidate: &mut Candidate, taste: Option<&TasteVector>) {
    if let Some(description) = candidate.description.as_mut() {
        *description = truncate_description(description, DESCRIPTION_CHARS);
    }
    if candidate.tags.len() > MAX_TAGS {
        if let Some(taste) = taste {
            // Stable: equally weighted tags keep their source order.
            candidate.tags.sort_by(|a, b| {
                taste
                    .term_weight(b)
                    .abs()
                    .total_cmp(&taste.term_weight(a).abs())
            });
        }
        candidate.tags.truncate(MAX_TAGS);
    }
    if let Some(pos) = candidate
        .external_ids
        .iter()
        .position(|id| id.starts_with("mangabaka:"))
    {
        let mangabaka = candidate.external_ids.remove(pos);
        candidate.external_ids.insert(0, mangabaka);
    }
    candidate.external_ids.truncate(2);
}

/// Cut `text` to about `max` characters, at a sentence end when one falls in
/// the second half of the window, else at a word boundary, marking the cut
/// with an ellipsis. Counts characters, not bytes.
fn truncate_description(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let window: String = text.chars().take(max).collect();
    let half = window.len() / 2;
    let cut = window
        .rfind(". ")
        .filter(|&i| i >= half)
        .map(|i| i + 1)
        .or_else(|| window.rfind(char::is_whitespace))
        .unwrap_or(window.len());
    format!("{}…", window[..cut].trim_end())
}

/// Scores ship rounded: three decimals is all the ordering signal there is.
fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
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
    fn payload_truncation_prefers_a_sentence_end() {
        let text = format!(
            "{} Second sentence keeps going and going.",
            "First sentence here.".repeat(3)
        );
        let cut = truncate_description(&text, 70);
        assert!(cut.ends_with(".…"), "{cut}");
        assert!(cut.chars().count() <= 71);
    }

    #[test]
    fn payload_truncation_is_char_boundary_safe() {
        let text = "日本語の説明文です。".repeat(60);
        let cut = truncate_description(&text, 280);
        assert!(cut.ends_with('…'));
        assert!(cut.chars().count() <= 281);
    }

    #[test]
    fn payload_short_descriptions_are_untouched() {
        assert_eq!(truncate_description("  Short. ", 280), "Short.");
    }

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
        assert_eq!(values, ["library", "acquirable", "discover"]);
    }
}
