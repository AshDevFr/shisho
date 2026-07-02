//! `lookup`: the full picture of one series in one call.
//!
//! Input resolution: a string that parses to a known provider (canonical
//! `provider:id`, Codex `plugin:`/`api:` notation, or bare `source:id`)
//! resolves by ID; anything else is a title, resolved through Tsundoku's
//! Dice-ranked provider search with the MangaBaka backbone's own title
//! search as the fallback. The three legs then run in parallel — MangaBaka
//! canonical metadata ∥ Tsundoku coverage ∥ Codex per-user state (snapshot
//! plus live rating) — and the Shisho verdict history is read last so it
//! can match on every external identity the legs surfaced.
//!
//! Merge order: MangaBaka is authoritative for canonical metadata,
//! Tsundoku contributes coverage/presence, Codex the caller's own state.
//! One leg failing yields a partial view with the failure named in `gaps`,
//! never an error for the whole call; only all sources failing (or the
//! input resolving nowhere) errors. This is the M7 exit: a `mangabaka:` id
//! resolves with Codex and Tsundoku both cold.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::read_state::ReadState;
use crate::verdicts;
use crate::{Services, UserCtx};
use shisho_db::repos::verdicts::RetractionReason;
use shisho_ids::{ExternalId, Provider};
use shisho_tsundoku::CoverageSpan;

/// Provider used for title resolution.
const TITLE_SEARCH_PROVIDER: &str = "mangabaka";
const TITLE_SEARCH_LIMIT: u32 = 5;

/// Canonical metadata from the MangaBaka backbone (dump-first, API
/// fallback). `source` says which layer answered so the model can phrase
/// provenance honestly.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MangabakaView {
    pub mangabaka_id: i64,
    pub title: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub alternate_titles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_volumes: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_chapters: Option<i32>,
    /// `mangabaka-dump` or `mangabaka-api`.
    pub source: &'static str,
}

/// The Tsundoku-side picture (catalog metadata + coverage).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TsundokuView {
    pub series_id: i32,
    pub title: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub alternate_titles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_volumes: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_chapters: Option<i32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub volume_coverage: Vec<CoverageSpan>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub chapter_coverage: Vec<CoverageSpan>,
    pub as_of: DateTime<Utc>,
}

/// The caller's own Codex state for an owned series.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexView {
    pub series_id: String,
    pub title: String,
    pub read_state: ReadState,
    pub book_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread_count: Option<i64>,
    /// 1-100, fetched live (not snapshot-aged).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub as_of: DateTime<Utc>,
}

/// One verdict from the caller's history.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerdictView {
    pub session_id: String,
    pub tier: String,
    pub rationale: String,
    pub verified: bool,
    pub retracted: bool,
    /// Why the retraction was recorded (`failedVerification` excludes the
    /// series from candidate pools for good; `withdrawn` does not).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retraction_reason: Option<RetractionReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// The merged view. Every section is per-source and independently absent.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesView {
    /// Best available title (Codex's when owned, else Tsundoku's, else
    /// the backbone's canonical title).
    pub title: String,
    /// Canonical `provider:id` strings from all sources, deduplicated.
    pub external_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mangabaka_id: Option<i64>,
    /// Canonical metadata from the MangaBaka backbone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mangabaka: Option<MangabakaView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tsundoku: Option<TsundokuView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub verdicts: Vec<VerdictView>,
    /// Sources that failed, named (`"tsundoku: ..."`, `"codex: ..."`), so a
    /// partial view is explicit about what it is missing.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum LookupError {
    /// The input matched nothing anywhere (all reachable sources agreed).
    #[error("{input:?} did not resolve to any known series")]
    Unresolved { input: String },
    /// Title inputs need a working title search (Tsundoku's provider
    /// search, or the backbone's own as fallback) and both failed.
    #[error("title search for {input:?} failed: {message}")]
    TitleSearch { input: String, message: String },
    /// Every source failed; the per-source failures are in `gaps`.
    #[error("all sources failed for {input:?}: {gaps:?}")]
    AllSourcesFailed { input: String, gaps: Vec<String> },
    #[error("verdict history query failed")]
    Db(#[from] sea_orm::DbErr),
}

pub async fn lookup(
    services: &Services,
    user: &UserCtx,
    input: &str,
) -> Result<SeriesView, LookupError> {
    let mut gaps: Vec<String> = Vec::new();

    // Input resolution. Unknown-provider parses (e.g. a title containing a
    // colon) fall through to title search.
    let id = match parse_known_id(input) {
        Some(id) => id,
        None => resolve_title(services, input).await?,
    };

    // The three legs in parallel: backbone metadata ∥ Tsundoku ∥ Codex.
    let (mangabaka_leg, tsundoku_leg, codex_leg) = tokio::join!(
        mangabaka_view(services, &id),
        tsundoku_view(services, &id),
        codex_view(services, user, &id),
    );

    let mangabaka = mangabaka_leg.unwrap_or_else(|gap| {
        gaps.push(gap);
        None
    });
    let tsundoku = tsundoku_leg.unwrap_or_else(|gap| {
        gaps.push(gap);
        None
    });
    let codex = codex_leg.unwrap_or_else(|gap| {
        gaps.push(gap);
        None
    });

    // Verdict history last, so it matches on every identity any leg
    // surfaced (local SQLite — negligible latency).
    let mut all_ids: Vec<ExternalId> = vec![id.clone()];
    let merge_ids = |ids: &[ExternalId], all: &mut Vec<ExternalId>| {
        for id in ids {
            if !all.iter().any(|x| x.key() == id.key()) {
                all.push(id.clone());
            }
        }
    };
    if let Some((_, backbone_ids)) = &mangabaka {
        merge_ids(backbone_ids, &mut all_ids);
    }
    if let Some((_, detail_ids)) = &tsundoku {
        merge_ids(detail_ids, &mut all_ids);
    }
    if let Some((_, snapshot_ids)) = &codex {
        merge_ids(snapshot_ids, &mut all_ids);
    }
    let verdicts: Vec<VerdictView> =
        verdicts::history(&services.db, &user.user_sub, all_ids.iter())
            .await?
            .into_iter()
            .map(|row| VerdictView {
                session_id: row.session_id,
                tier: row.tier,
                rationale: row.rationale,
                verified: row.verified,
                retracted: row.retracted,
                retraction_reason: row
                    .retracted
                    .then(|| RetractionReason::from_db(row.retraction_reason.as_deref())),
                outcome: row.outcome,
                notes: row.notes,
                created_at: row.created_at.to_utc(),
            })
            .collect();

    let mangabaka = mangabaka.map(|(view, _)| view);
    let tsundoku = tsundoku.map(|(view, _)| view);
    let codex = codex.map(|(view, _)| view);

    if mangabaka.is_none() && tsundoku.is_none() && codex.is_none() && verdicts.is_empty() {
        return Err(if gaps.is_empty() {
            LookupError::Unresolved {
                input: input.to_owned(),
            }
        } else {
            LookupError::AllSourcesFailed {
                input: input.to_owned(),
                gaps,
            }
        });
    }

    let title = codex
        .as_ref()
        .map(|c| c.title.clone())
        .or_else(|| tsundoku.as_ref().map(|t| t.title.clone()))
        .or_else(|| mangabaka.as_ref().map(|m| m.title.clone()))
        .unwrap_or_else(|| input.to_owned());
    let mangabaka_id = mangabaka.as_ref().map(|m| m.mangabaka_id).or_else(|| {
        all_ids
            .iter()
            .find(|i| i.provider == Provider::Mangabaka)
            .and_then(|i| i.id.parse().ok())
    });
    let external_ids = all_ids.iter().filter_map(ExternalId::as_tsundoku).collect();

    Ok(SeriesView {
        title,
        external_ids,
        mangabaka_id,
        mangabaka,
        tsundoku,
        codex,
        verdicts,
        gaps,
    })
}

/// Parse an ID in either notation: Codex (`plugin:myanimelist:123`,
/// `myanimelist:123`) or canonical Tsundoku (`mal:123`). Only a known
/// provider counts — anything else is treated as a title. Also the parse
/// the write tools use to pin a verdict's identity.
pub fn parse_known_id(input: &str) -> Option<ExternalId> {
    if let Ok(id) = ExternalId::from_codex(input)
        && !matches!(id.provider, Provider::Other(_))
    {
        return Some(id);
    }
    if let Ok(id) = ExternalId::from_tsundoku(input)
        && !matches!(id.provider, Provider::Other(_))
    {
        return Some(id);
    }
    None
}

/// Title → external ID: Tsundoku's Dice-ranked provider search first
/// (when configured and reachable), then the backbone's own title search
/// (dump FTS, Dice re-ranked) — so titles resolve with Tsundoku cold.
async fn resolve_title(services: &Services, input: &str) -> Result<ExternalId, LookupError> {
    let client = services.tsundoku.client();
    let mut tsundoku_failure: Option<String> = None;
    if client.is_configured() {
        match client
            .provider_search(TITLE_SEARCH_PROVIDER, input, Some(TITLE_SEARCH_LIMIT))
            .await
        {
            // Hits are ranked server-side; the best one wins.
            Ok(response) => {
                if let Some(hit) = response.hits.into_iter().next() {
                    return Ok(ExternalId {
                        provider: Provider::from_tsundoku_token(TITLE_SEARCH_PROVIDER),
                        id: hit.external_id,
                        provenance: None,
                    });
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "Tsundoku provider search failed; falling back to the backbone's title search");
                tsundoku_failure = Some(err.to_string());
            }
        }
    }

    match services
        .metadata
        .search_title(input, TITLE_SEARCH_LIMIT)
        .await
    {
        Ok(hits) => match hits.into_iter().next() {
            Some(hit) => Ok(ExternalId {
                provider: Provider::Mangabaka,
                id: hit.mangabaka_id.to_string(),
                provenance: None,
            }),
            // Both searches agreed (or were the only reachable one):
            // nothing matches. A Tsundoku transport failure is still worth
            // naming over a generic miss.
            None => Err(match tsundoku_failure {
                Some(message) => LookupError::TitleSearch {
                    input: input.to_owned(),
                    message,
                },
                None => LookupError::Unresolved {
                    input: input.to_owned(),
                },
            }),
        },
        Err(err) => Err(LookupError::TitleSearch {
            input: input.to_owned(),
            message: match tsundoku_failure {
                Some(tsundoku) => format!("tsundoku: {tsundoku}; mangabaka: {err}"),
                None => err.to_string(),
            },
        }),
    }
}

/// The MangaBaka leg: native ids resolve directly, foreign ids pivot
/// through `resolve_by_foreign_id`. `Ok(None)` = cleanly absent from the
/// backbone; `Err(gap)` = the leg failed and the view should say so.
async fn mangabaka_view(
    services: &Services,
    id: &ExternalId,
) -> Result<Option<(MangabakaView, Vec<ExternalId>)>, String> {
    let meta = services
        .metadata
        .resolve(id)
        .await
        .map_err(|e| format!("mangabaka: {e}"))?;
    let Some(meta) = meta else {
        return Ok(None);
    };

    let mut ids = vec![ExternalId {
        provider: Provider::Mangabaka,
        id: meta.mangabaka_id.to_string(),
        provenance: None,
    }];
    ids.extend(meta.foreign_ids.iter().cloned());

    Ok(Some((
        MangabakaView {
            mangabaka_id: meta.mangabaka_id,
            title: meta.title,
            alternate_titles: meta.alternate_titles,
            description: meta.description,
            genres: meta.genres,
            tags: meta.tags,
            rating: meta.rating,
            status: meta.status.map(|s| s.as_str().to_owned()),
            year: meta.year,
            total_volumes: meta.total_volumes,
            total_chapters: meta.total_chapters,
            source: meta.origin.as_str(),
        },
        ids,
    )))
}

/// The Tsundoku leg: external ID → series (filtered feed) → cached detail.
/// `Ok(None)` = cleanly absent from the catalog; `Err(gap)` = the leg
/// failed and the view should say so.
async fn tsundoku_view(
    services: &Services,
    id: &ExternalId,
) -> Result<Option<(TsundokuView, Vec<ExternalId>)>, String> {
    let client = services.tsundoku.client();
    if !client.is_configured() {
        return Ok(None);
    }
    let Some(token) = id.as_tsundoku() else {
        return Ok(None);
    };

    let feed = client
        .feed_filtered(None, Some(1), std::slice::from_ref(&token))
        .await
        .map_err(|e| format!("tsundoku: {e}"))?;
    let Some(item) = feed.items.first() else {
        return Ok(None);
    };

    let hit = services
        .tsundoku
        .detail(item.series_id)
        .await
        .map_err(|e| format!("tsundoku: {e}"))?;
    let detail = hit.value;

    let detail_ids: Vec<ExternalId> = detail
        .external_ids
        .iter()
        .map(|e| e.normalized())
        .filter(|e| !matches!(e.provider, Provider::Other(_)))
        .collect();

    Ok(Some((
        TsundokuView {
            series_id: detail.id,
            title: detail.canonical_title.clone(),
            alternate_titles: detail.alternate_titles.clone(),
            description: detail.description.clone(),
            genres: detail.genres.clone(),
            tags: detail.tags.clone(),
            rating: detail.rating,
            status: detail.status.clone(),
            year: detail.year,
            total_volumes: detail.total_volumes,
            total_chapters: detail.total_chapters,
            volume_coverage: item.volume_coverage.clone(),
            chapter_coverage: item.chapter_coverage.clone(),
            as_of: hit.as_of,
        },
        detail_ids,
    )))
}

/// The Codex leg: per-user state from the snapshot (the per-user pivot —
/// never the instance-global overlay), plus the live rating/notes.
/// `Ok(None)` = the caller does not own the series.
async fn codex_view(
    services: &Services,
    user: &UserCtx,
    id: &ExternalId,
) -> Result<Option<(CodexView, Vec<ExternalId>)>, String> {
    let snapshot_hit = services
        .snapshots
        .get(&user.user_sub, &user.credential)
        .await
        .map_err(|e| format!("codex: {e}"))?;

    let Some(series) = snapshot_hit.value.series_by_external_id(id) else {
        return Ok(None);
    };

    // Rating/notes are cheap and single-series: fetch live rather than
    // serving snapshot-aged values.
    let rating = services
        .snapshots
        .codex()
        .series_rating(&user.credential, &series.id)
        .await
        .map_err(|e| format!("codex: {e}"))?;

    Ok(Some((
        CodexView {
            series_id: series.id.clone(),
            title: series.title.clone(),
            read_state: series.read_state,
            book_count: series.book_count,
            unread_count: series.unread_count,
            rating: rating.as_ref().map(|r| r.rating),
            notes: rating.and_then(|r| r.notes),
            as_of: snapshot_hit.as_of,
        },
        series.external_ids.clone(),
    )))
}
