//! Typed Tsundoku client. One instance-level admin bearer (from config) on
//! every call: the Codex-overlay filters Shisho uses as server-side
//! pre-filters are admin-gated. Tsundoku timestamps are epoch seconds.

use serde::{Deserialize, Serialize};
use shisho_config::Secret;

use shisho_ids::ExternalId;

#[derive(Debug, thiserror::Error)]
pub enum TsundokuError {
    /// 401 — the configured admin bearer was rejected. Deployment
    /// misconfiguration, not a caller problem.
    #[error("Tsundoku rejected the admin bearer (401 from {endpoint})")]
    Unauthorized { endpoint: String },
    #[error("Tsundoku returned {status} from {endpoint}: {body}")]
    Api {
        status: reqwest::StatusCode,
        endpoint: String,
        body: String,
    },
    #[error("error calling Tsundoku ({endpoint})")]
    Transport {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("could not decode Tsundoku response from {endpoint}")]
    Decode {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("Tsundoku base URL is not configured")]
    NotConfigured,
}

/// `GET /api/v1/health`.
#[derive(Debug, Clone, Deserialize)]
pub struct Health {
    pub status: String,
}

/// Match mode for genre/tag filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    Any,
    All,
}

impl MatchMode {
    fn as_str(self) -> &'static str {
        match self {
            MatchMode::Any => "any",
            MatchMode::All => "all",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Asc,
    Desc,
}

impl Order {
    fn as_str(self) -> &'static str {
        match self {
            Order::Asc => "asc",
            Order::Desc => "desc",
        }
    }
}

/// Query for `GET /api/v1/series`. All filters compose; `None` fields are
/// omitted from the request.
#[derive(Debug, Clone, Default)]
pub struct SeriesListQuery {
    pub page: Option<u32>,
    pub page_size: Option<u32>,
    /// e.g. `manga`.
    pub kind: Option<String>,
    /// e.g. `ongoing`.
    pub status: Option<String>,
    /// Sweep-derived, admin-gated. A server-side pre-filter only — never
    /// the ownership gate (that is always the caller's own Codex view).
    pub owned: Option<bool>,
    pub has_releases: Option<bool>,
    pub genres: Option<Vec<String>>,
    pub genres_mode: Option<MatchMode>,
    pub tags: Option<Vec<String>>,
    pub tags_mode: Option<MatchMode>,
    /// e.g. `rating`, `last_release_at`, `first_seen_at`.
    pub sort: Option<String>,
    pub order: Option<Order>,
    /// Free-text title search.
    pub q: Option<String>,
    /// Comma-joined of `any|missing|complete|behind|present|ignored`
    /// (admin-only).
    pub codex_status: Option<Vec<String>>,
}

impl SeriesListQuery {
    /// Stable cache key: the encoded query string. Two queries that encode
    /// identically hit the same cache entry.
    pub fn cache_key(&self) -> String {
        let pairs = self.to_pairs();
        let mut key = String::new();
        for (i, (k, v)) in pairs.iter().enumerate() {
            if i > 0 {
                key.push('&');
            }
            key.push_str(k);
            key.push('=');
            key.push_str(v);
        }
        key
    }

    fn to_pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::new();
        if let Some(v) = self.page {
            pairs.push(("page", v.to_string()));
        }
        if let Some(v) = self.page_size {
            pairs.push(("pageSize", v.to_string()));
        }
        if let Some(v) = &self.kind {
            pairs.push(("kind", v.clone()));
        }
        if let Some(v) = &self.status {
            pairs.push(("status", v.clone()));
        }
        if let Some(v) = self.owned {
            pairs.push(("owned", v.to_string()));
        }
        if let Some(v) = self.has_releases {
            pairs.push(("hasReleases", v.to_string()));
        }
        if let Some(v) = &self.genres {
            pairs.push(("genres", v.join(",")));
        }
        if let Some(v) = self.genres_mode {
            pairs.push(("genresMode", v.as_str().to_owned()));
        }
        if let Some(v) = &self.tags {
            pairs.push(("tags", v.join(",")));
        }
        if let Some(v) = self.tags_mode {
            pairs.push(("tagsMode", v.as_str().to_owned()));
        }
        if let Some(v) = &self.sort {
            pairs.push(("sort", v.clone()));
        }
        if let Some(v) = self.order {
            pairs.push(("order", v.as_str().to_owned()));
        }
        if let Some(v) = &self.q {
            pairs.push(("q", v.clone()));
        }
        if let Some(v) = &self.codex_status {
            pairs.push(("codexStatus", v.join(",")));
        }
        pairs
    }
}

/// `GET /api/v1/series` response (`SeriesListPage`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesListPage {
    pub items: Vec<SeriesListItem>,
    pub page: i32,
    pub page_size: i32,
    pub total: i64,
    /// Epoch seconds of the last Codex sweep, if one is configured.
    pub codex_synced_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesListItem {
    pub id: i32,
    pub canonical_title: String,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    pub metadata_source: String,
    pub last_release_at: i64,
    pub first_seen_at: i64,
    pub release_count: i64,
    pub total_volumes: Option<i32>,
    pub total_chapters: Option<i32>,
    pub highest_volume: Option<f64>,
    pub highest_chapter: Option<f64>,
    pub rating: Option<f64>,
    pub owned: bool,
    pub codex: Option<CodexInfo>,
}

/// Admin-only Codex overlay. Consumed as a pre-filter / join pivot
/// (`series_uuid`) only — ownership facts shown to a user always come from
/// their own Codex view.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexInfo {
    /// `complete | behind | present | ignored`.
    pub status: String,
    /// The Codex series UUID — the pre-joined reverse mapping.
    pub series_uuid: String,
    pub deep_link: String,
    pub link_kind: String,
    pub local_max_volume: Option<f64>,
    pub local_max_chapter: Option<f64>,
    pub volumes_owned: Option<i64>,
    pub synced_at: i64,
}

/// `GET /api/v1/series/{id}` (`SeriesDetail`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesDetail {
    pub id: i32,
    pub canonical_title: String,
    #[serde(default)]
    pub alternate_titles: Vec<String>,
    pub description: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub year: Option<i32>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    pub metadata_source: String,
    pub rating: Option<f64>,
    pub total_volumes: Option<i32>,
    pub total_chapters: Option<i32>,
    pub highest_volume: Option<f64>,
    pub highest_chapter: Option<f64>,
    pub owned: bool,
    pub external_ids: Vec<ExternalIdDto>,
    pub codex: Option<CodexInfo>,
    pub cover_url: Option<String>,
    pub first_seen_at: i64,
    pub last_release_at: i64,
}

/// One external ID as Tsundoku stores it: verbatim lowercase provider token
/// plus the bare id.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalIdDto {
    pub provider: String,
    pub external_id: String,
    pub fetched_at: i64,
}

impl ExternalIdDto {
    /// Normalize through the canonical `Provider` module.
    pub fn normalized(&self) -> ExternalId {
        ExternalId {
            provider: shisho_ids::Provider::from_tsundoku_token(&self.provider),
            id: self.external_id.clone(),
            provenance: None,
        }
    }
}

/// `GET|POST /api/v1/series/feed` response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesFeedResponse {
    pub items: Vec<SeriesFeedItem>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesFeedItem {
    pub series_id: i32,
    pub canonical_title: String,
    pub external_ids: Vec<ExternalIdDto>,
    pub volume_coverage: Vec<CoverageSpan>,
    pub chapter_coverage: Vec<CoverageSpan>,
    pub highest_volume: Option<f64>,
    pub highest_chapter: Option<f64>,
    pub updated_at: i64,
}

/// One inclusive `[start, end]` coverage range. `Serialize` because
/// candidate DTOs relay coverage spans verbatim.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverageSpan {
    pub start: f64,
    pub end: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SeriesFeedRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<u32>,
    /// `provider:externalId` strings, verbatim tokens.
    external_ids: &'a [String],
}

/// `GET /api/v1/providers/{id}/search` response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSearchResponse {
    pub provider: String,
    pub hits: Vec<ProviderSearchHit>,
}

/// One Dice-ranked hit (`score` is 1.0 on the externalId short-circuit
/// path). Note: carries no description (title-only), so soft-red-flag
/// screening of a hit falls back to the MangaBaka metadata backbone.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSearchHit {
    pub external_id: String,
    pub title: String,
    pub native_title: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub year: Option<i32>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    pub total_volumes: Option<i32>,
    pub total_chapters: Option<i32>,
    pub cover_url: Option<String>,
    pub score: f32,
}

#[derive(Clone)]
pub struct TsundokuClient {
    http: reqwest::Client,
    /// Without the trailing slash. Empty = not configured (Tier 0).
    base_url: String,
    admin_bearer: Secret,
}

impl TsundokuClient {
    pub fn new(base_url: &str, admin_bearer: Secret, http: reqwest::Client) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            admin_bearer,
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.base_url.is_empty()
    }

    /// The configured base URL (no trailing slash; empty = Tier 0). For
    /// deep links into Tsundoku's own UI — fixes happen at the source.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET /api/v1/health` — reachability probe.
    pub async fn health(&self) -> Result<Health, TsundokuError> {
        self.send(self.get("/api/v1/health")?, "/api/v1/health")
            .await
    }

    /// `GET /api/v1/series` — the acquirable-pool query.
    pub async fn list_series(
        &self,
        query: &SeriesListQuery,
    ) -> Result<SeriesListPage, TsundokuError> {
        let req = self.get("/api/v1/series")?.query(&query.to_pairs());
        self.send(req, "/api/v1/series").await
    }

    /// `GET /api/v1/series/{id}` — full metadata for one series.
    pub async fn series_detail(&self, id: i32) -> Result<SeriesDetail, TsundokuError> {
        let endpoint = format!("/api/v1/series/{id}");
        let req = self.get(&endpoint)?;
        self.send(req, &endpoint).await
    }

    /// `GET /api/v1/series/feed` — cursor-paged change signal. Omit the
    /// cursor to start from the beginning.
    pub async fn feed(
        &self,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<SeriesFeedResponse, TsundokuError> {
        let mut pairs: Vec<(&str, String)> = Vec::new();
        if let Some(c) = cursor {
            pairs.push(("cursor", c.to_owned()));
        }
        if let Some(l) = limit {
            pairs.push(("limit", l.to_string()));
        }
        let req = self.get("/api/v1/series/feed")?.query(&pairs);
        self.send(req, "/api/v1/series/feed").await
    }

    /// `POST /api/v1/series/feed` — the feed filtered to series carrying one
    /// of the given external IDs (verbatim `provider:externalId` strings).
    pub async fn feed_filtered(
        &self,
        cursor: Option<&str>,
        limit: Option<u32>,
        external_ids: &[String],
    ) -> Result<SeriesFeedResponse, TsundokuError> {
        let req = self.post("/api/v1/series/feed")?.json(&SeriesFeedRequest {
            cursor,
            limit,
            external_ids,
        });
        self.send(req, "/api/v1/series/feed").await
    }

    /// `GET /api/v1/providers/{id}/search` — Dice-ranked provider title
    /// search (used by `lookup` for series outside the catalog).
    pub async fn provider_search(
        &self,
        provider_id: &str,
        q: &str,
        limit: Option<u32>,
    ) -> Result<ProviderSearchResponse, TsundokuError> {
        let endpoint = format!("/api/v1/providers/{provider_id}/search");
        let mut pairs = vec![("q", q.to_owned())];
        if let Some(l) = limit {
            pairs.push(("limit", l.to_string()));
        }
        let req = self.get(&endpoint)?.query(&pairs);
        self.send(req, &endpoint).await
    }

    fn get(&self, path: &str) -> Result<reqwest::RequestBuilder, TsundokuError> {
        Ok(self.authed(self.http.get(self.url(path)?)))
    }

    fn post(&self, path: &str) -> Result<reqwest::RequestBuilder, TsundokuError> {
        Ok(self.authed(self.http.post(self.url(path)?)))
    }

    fn url(&self, path: &str) -> Result<String, TsundokuError> {
        if self.base_url.is_empty() {
            return Err(TsundokuError::NotConfigured);
        }
        Ok(format!("{}{path}", self.base_url))
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.bearer_auth(self.admin_bearer.expose())
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
        endpoint: &str,
    ) -> Result<T, TsundokuError> {
        let resp = req
            .send()
            .await
            .map_err(|source| TsundokuError::Transport {
                endpoint: endpoint.to_owned(),
                source,
            })?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(TsundokuError::Unauthorized {
                endpoint: endpoint.to_owned(),
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TsundokuError::Api {
                status,
                endpoint: endpoint.to_owned(),
                body,
            });
        }

        resp.json().await.map_err(|source| TsundokuError::Decode {
            endpoint: endpoint.to_owned(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acquirable(sort: &str) -> SeriesListQuery {
        SeriesListQuery {
            kind: Some("manga".into()),
            has_releases: Some(true),
            owned: Some(false),
            sort: Some(sort.into()),
            order: Some(Order::Desc),
            genres: Some(vec!["fantasy".into(), "adventure".into()]),
            genres_mode: Some(MatchMode::All),
            ..Default::default()
        }
    }

    #[test]
    fn cache_key_is_deterministic_for_identical_queries() {
        let a = acquirable("rating");
        let b = acquirable("rating");
        assert_eq!(a.cache_key(), b.cache_key());
        // Encodes the fields that matter, verbatim.
        let key = a.cache_key();
        assert!(key.contains("kind=manga"), "{key}");
        assert!(key.contains("sort=rating"), "{key}");
        assert!(key.contains("owned=false"), "{key}");
        assert!(key.contains("genres=fantasy,adventure"), "{key}");
        assert!(key.contains("genresMode=all"), "{key}");
        assert!(key.contains("order=desc"), "{key}");
    }

    #[test]
    fn cache_key_distinguishes_distinct_queries() {
        assert_ne!(
            acquirable("rating").cache_key(),
            acquirable("last_release_at").cache_key()
        );

        let mut paged = acquirable("rating");
        paged.page = Some(2);
        assert_ne!(acquirable("rating").cache_key(), paged.cache_key());

        // The empty query is a distinct, stable key of its own.
        let empty = SeriesListQuery::default();
        assert_eq!(empty.cache_key(), "");
        assert_ne!(empty.cache_key(), acquirable("rating").cache_key());
    }
}
