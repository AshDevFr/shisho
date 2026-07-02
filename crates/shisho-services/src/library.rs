//! `search_library`: the caller's own library by title/author/genre/state.
//! Powers chat questions like "did I read anything by this author?".
//!
//! Text queries go to Codex's server-side search (it matches titles and
//! authors) with the caller's credential; genre and read-state filters
//! compose on top from the snapshot, which already carries both. Tsundoku
//! is deliberately not involved: this is a pure library view.

use serde::{Deserialize, Serialize};

use crate::library_snapshot;
use crate::read_state::ReadState;
use crate::{Services, UserCtx};
use shisho_cache::SwrError;
use shisho_codex::CodexError;
use shisho_ids::ExternalId;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

/// Composable query: every field narrows the result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LibraryQuery {
    /// Title/author text, matched server-side by Codex.
    pub text: Option<String>,
    /// Case-insensitive genre membership.
    pub genre: Option<String>,
    pub state: Option<ReadState>,
    /// Codex libraries to search (names or IDs); empty = all.
    pub libraries: Vec<String>,
    /// Default 50, capped at 200.
    pub limit: Option<usize>,
}

/// One library series, compact.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibrarySeries {
    /// Codex series UUID.
    pub id: String,
    pub title: String,
    /// The Codex library the series shelves in.
    pub library: String,
    pub read_state: ReadState,
    pub book_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread_count: Option<i64>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    /// The caller has a Codex rating for this series.
    pub rated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    /// Canonical `provider:id` strings.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub external_ids: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("library snapshot unavailable")]
    Snapshot(#[source] SwrError<CodexError>),
    #[error("Codex library search failed")]
    Search(#[from] CodexError),
    /// A `libraries` selector matched nothing — typo or wrong instance.
    #[error("no Codex library matches {selectors:?}; available libraries: {available:?}")]
    UnknownLibraries {
        selectors: Vec<String>,
        available: Vec<String>,
    },
}

pub async fn search_library(
    services: &Services,
    user: &UserCtx,
    query: &LibraryQuery,
) -> Result<Vec<LibrarySeries>, LibraryError> {
    let snapshot = services
        .snapshots
        .get(&user.user_sub, &user.credential)
        .await
        .map_err(LibraryError::Snapshot)?
        .value;

    if !query.libraries.is_empty() {
        let unmatched = library_snapshot::unmatched_selectors(&snapshot, &query.libraries);
        if !unmatched.is_empty() {
            return Err(LibraryError::UnknownLibraries {
                selectors: unmatched,
                available: snapshot.library_names(),
            });
        }
    }

    // Server-side title/author match first; it returns the series IDs the
    // in-memory filters compose against.
    let text_matches: Option<Vec<String>> = match &query.text {
        Some(text) => Some(
            services
                .snapshots
                .codex()
                .search_series(&user.credential, text, None)
                .await?
                .into_iter()
                .map(|s| s.id)
                .collect(),
        ),
        None => None,
    };

    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let results = snapshot
        .series
        .iter()
        .filter(|s| match &text_matches {
            Some(ids) => ids.contains(&s.id),
            None => true,
        })
        .filter(|s| {
            query.libraries.is_empty()
                || query.libraries.iter().any(|sel| {
                    library_snapshot::selector_matches(sel, &s.library_id, &s.library_name)
                })
        })
        .filter(|s| match &query.genre {
            Some(genre) => s.genres.iter().any(|g| g.eq_ignore_ascii_case(genre)),
            None => true,
        })
        .filter(|s| match query.state {
            Some(state) => s.read_state == state,
            None => true,
        })
        .take(limit)
        .map(|s| LibrarySeries {
            id: s.id.clone(),
            title: s.title.clone(),
            library: s.library_name.clone(),
            read_state: s.read_state,
            book_count: s.book_count,
            unread_count: s.unread_count,
            genres: s.genres.clone(),
            tags: s.tags.clone(),
            rated: s.rated,
            status: s.status.clone(),
            year: s.year,
            external_ids: s
                .external_ids
                .iter()
                .filter_map(ExternalId::as_tsundoku)
                .collect(),
        })
        .collect();

    Ok(results)
}
