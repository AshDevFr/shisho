//! Typed Codex client. One client instance serves all users: the caller's
//! credential is a per-call parameter and is never stored (PRD: identity
//! flows through, credentials don't).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use shisho_ids::ExternalId;

/// The caller's own Codex credential, passed per call.
#[derive(Clone)]
pub enum CodexCredential {
    /// A Codex API key, sent as `X-API-Key`. Validated by use.
    ApiKey(String),
    /// An OIDC access token forwarded as `Authorization: Bearer` (OIDC
    /// phase; Codex must trust the same IdP).
    Bearer(String),
}

impl std::fmt::Debug for CodexCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodexCredential::ApiKey(_) => f.write_str("CodexCredential::ApiKey(***)"),
            CodexCredential::Bearer(_) => f.write_str("CodexCredential::Bearer(***)"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CodexError {
    /// 401 — the caller's Codex API key has expired or was revoked. Surfaced
    /// as its own variant so it reaches the user verbatim, never silently.
    #[error("Codex API key expired or revoked (401 from {endpoint})")]
    Unauthorized { endpoint: String },
    #[error("Codex returned {status} from {endpoint}: {body}")]
    Api {
        status: reqwest::StatusCode,
        endpoint: String,
        body: String,
    },
    #[error("error calling Codex ({endpoint})")]
    Transport {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("could not decode Codex response from {endpoint}")]
    Decode {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("Codex base URL is not configured")]
    NotConfigured,
}

/// `GET /api/v1/auth/me` — the canonical `user_sub` source.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexUser {
    pub id: String,
    pub username: String,
    pub email: String,
    pub role: String,
    pub email_verified: bool,
    pub permissions: Vec<String>,
}

/// `GET /api/v1/info` — used as the health probe.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAppInfo {
    pub name: String,
    pub version: String,
}

/// One Codex library (`GET /api/v1/libraries` row; config fields ignored).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Library {
    pub id: String,
    pub name: String,
    pub series_count: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LibrariesList {
    data: Vec<Library>,
}

/// Codex's generic pagination envelope (HATEOAS links ignored).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub data: Vec<T>,
    pub page: u64,
    pub page_size: u64,
    pub total: u64,
    pub total_pages: u64,
}

/// Compact series row (`SeriesDto`) — search results and detail without
/// `full=true`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Series {
    pub id: String,
    pub library_id: String,
    pub library_name: String,
    pub title: String,
    pub book_count: i64,
    pub unread_count: Option<i64>,
    pub tracked: bool,
    pub summary: Option<String>,
}

/// Full series row (`FullSeriesResponse`, `full=true`): unread counts and
/// external IDs in one paginated call — the load-bearing query for both
/// candidate pools.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FullSeries {
    pub id: String,
    pub library_id: String,
    pub library_name: String,
    pub book_count: i64,
    pub unread_count: Option<i64>,
    pub metadata: SeriesMetadata,
    #[serde(default)]
    pub genres: Vec<Genre>,
    #[serde(default)]
    pub tags: Vec<Tag>,
    #[serde(default)]
    pub external_ids: Vec<SeriesExternalId>,
}

impl FullSeries {
    /// All external IDs that normalize to a known provider, ready for the
    /// Tsundoku join. Unknown sources are dropped here (already logged at
    /// parse time).
    pub fn normalized_external_ids(&self) -> Vec<ExternalId> {
        self.external_ids
            .iter()
            .filter_map(|e| ExternalId::from_codex_parts(&e.source, &e.external_id).ok())
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesMetadata {
    pub title: String,
    pub summary: Option<String>,
    pub status: Option<String>,
    pub year: Option<i32>,
    pub publisher: Option<String>,
    pub total_volume_count: Option<i32>,
    pub total_chapter_count: Option<f32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Genre {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tag {
    pub name: String,
}

/// One row of `FullSeriesResponse.externalIds` (`SeriesExternalIdDto`).
/// `source` carries the provenance-prefixed source name
/// (`plugin:mangabaka`); the id arrives separately.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesExternalId {
    pub source: String,
    pub external_id: String,
    pub external_url: Option<String>,
}

/// One entry of the slim external-index projection
/// (`GET /api/v1/series/external-index`): a Codex series UUID and its linked
/// external IDs, without the heavy full DTO. Purpose-built for resolving an
/// external id (e.g. a MangaBaka id) back to its Codex series.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesExternalIndexEntry {
    /// Codex series UUID; build the web deep link as `/series/{id}`.
    pub id: String,
    #[serde(default)]
    pub external_ids: Vec<SeriesExternalId>,
}

impl SeriesExternalIndexEntry {
    /// The entry's external IDs normalized to known providers (unknown
    /// providers dropped), mirroring [`FullSeries::normalized_external_ids`].
    pub fn normalized_external_ids(&self) -> Vec<ExternalId> {
        self.external_ids
            .iter()
            .filter_map(|e| ExternalId::from_codex_parts(&e.source, &e.external_id).ok())
            .collect()
    }
}

/// The caller's rating + notes for one series (`UserSeriesRatingDto`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserSeriesRating {
    pub id: String,
    pub series_id: String,
    /// 1-100 (Codex displays 1-10 with one decimal).
    pub rating: i32,
    pub notes: Option<String>,
}

/// `GET /api/v1/user/ratings` envelope (`UserRatingsListResponse`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRatingsList {
    pub ratings: Vec<UserSeriesRating>,
}

/// `PUT /series/{id}/rating` body.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetRating {
    /// 1-100.
    pub rating: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchSeriesRequest<'a> {
    query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    library_id: Option<&'a str>,
}

/// A Codex permission, in `resource:action` form. A closed set of exactly the
/// scopes a Shisho-minted key needs, so the request body is compile-checked
/// rather than stringly-typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `series:read` — read series, per-user unread counts, external IDs, and
    /// ratings.
    SeriesRead,
    /// `libraries:read` — list the caller's libraries.
    LibrariesRead,
}

impl Scope {
    /// The verbatim Codex permission token.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::SeriesRead => "series:read",
            Scope::LibrariesRead => "libraries:read",
        }
    }
}

/// The least-privilege scope set for a Shisho-minted Codex key. Read-only:
/// `series:read` + `libraries:read`. Rating-write is deliberately not
/// requested — Codex currently gates rating writes under `series:read` (no
/// dedicated permission), so Shisho does not exercise them until Codex
/// separates that permission; withholding `books:read` blocks progress writes.
pub const SHISHO_KEY_SCOPES: &[Scope] = &[Scope::SeriesRead, Scope::LibrariesRead];

/// `POST /api/v1/api-keys` request body. `permissions` is always sent
/// explicitly: omitting it makes Codex copy the caller's (broad) permissions,
/// which would defeat least privilege.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MintApiKeyRequest {
    name: String,
    permissions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<DateTime<Utc>>,
}

/// `POST /api/v1/api-keys` response (201). Codex flattens the key's metadata
/// alongside the one-time plaintext `key`; only the fields Shisho stores are
/// deserialized (unknown fields are ignored).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MintedApiKey {
    /// The plaintext API key (`codex_<hex>_<hex>`), returned only once. The
    /// caller stores it encrypted.
    pub key: String,
    /// Codex's id for the key (for later revocation / regeneration).
    pub id: String,
    pub name: String,
    /// Non-secret prefix Codex uses to identify the key in listings.
    pub key_prefix: String,
    pub is_active: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct CodexClient {
    http: reqwest::Client,
    /// Without the trailing slash. Empty = not configured.
    base_url: String,
}

impl CodexClient {
    pub fn new(base_url: &str, http: reqwest::Client) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.base_url.is_empty()
    }

    /// The configured base URL (no trailing slash; empty = unconfigured).
    /// Used to build deep links into Codex's own UI (e.g. the
    /// metadata-match flow for unresolvable series).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET /api/v1/auth/me` — resolve the credential to its Codex user.
    pub async fn me(&self, credential: &CodexCredential) -> Result<CodexUser, CodexError> {
        let req = self.get("/api/v1/auth/me")?;
        self.send(Self::authed(req, credential), "/api/v1/auth/me")
            .await
    }

    /// `GET /api/v1/info` — unauthenticated reachability probe.
    pub async fn info(&self) -> Result<CodexAppInfo, CodexError> {
        self.send(self.get("/api/v1/info")?, "/api/v1/info").await
    }

    /// `GET /api/v1/libraries` — the caller's libraries (the `list_libraries`
    /// tool and the agent-side library-selection flow).
    pub async fn list_libraries(
        &self,
        credential: &CodexCredential,
    ) -> Result<Vec<Library>, CodexError> {
        let req = self.get("/api/v1/libraries")?;
        let list: LibrariesList = self
            .send(Self::authed(req, credential), "/api/v1/libraries")
            .await?;
        Ok(list.data)
    }

    /// `GET /api/v1/series?full=true` — one page of the caller's library
    /// with unread counts and external IDs inline. Pages are
    /// 1-indexed; `page_size` is capped at 100 by Codex.
    pub async fn list_series(
        &self,
        credential: &CodexCredential,
        page: u64,
        page_size: u64,
    ) -> Result<Page<FullSeries>, CodexError> {
        let req = self.get("/api/v1/series")?.query(&[
            ("full", "true".to_owned()),
            ("page", page.to_string()),
            ("pageSize", page_size.to_string()),
        ]);
        self.send(Self::authed(req, credential), "/api/v1/series")
            .await
    }

    /// `GET /api/v1/series/external-index` — one page of the slim external-id
    /// projection (series UUID + linked external IDs). The lightweight way to
    /// resolve external ids to Codex series without over-fetching the full DTO;
    /// `page_size` is capped at 500 by Codex.
    pub async fn list_series_external_index(
        &self,
        credential: &CodexCredential,
        page: u64,
        page_size: u64,
    ) -> Result<Page<SeriesExternalIndexEntry>, CodexError> {
        let req = self.get("/api/v1/series/external-index")?.query(&[
            ("page", page.to_string()),
            ("pageSize", page_size.to_string()),
        ]);
        self.send(
            Self::authed(req, credential),
            "/api/v1/series/external-index",
        )
        .await
    }

    /// `POST /api/v1/series/search` — title/author search over the caller's
    /// library (powers `search_library`).
    pub async fn search_series(
        &self,
        credential: &CodexCredential,
        query: &str,
        library_id: Option<&str>,
    ) -> Result<Vec<Series>, CodexError> {
        let req = self
            .post("/api/v1/series/search")?
            .json(&SearchSeriesRequest { query, library_id });
        let page: Page<Series> = self
            .send(Self::authed(req, credential), "/api/v1/series/search")
            .await?;
        Ok(page.data)
    }

    /// `GET /api/v1/series/{id}?full=true` — one series with metadata and
    /// external IDs.
    pub async fn series_detail(
        &self,
        credential: &CodexCredential,
        series_id: &str,
    ) -> Result<FullSeries, CodexError> {
        let endpoint = format!("/api/v1/series/{series_id}");
        let req = self.get(&endpoint)?.query(&[("full", "true")]);
        self.send(Self::authed(req, credential), &endpoint).await
    }

    /// `GET /api/v1/user/ratings` — every rating the caller has set, in one
    /// call (ratings presence for the library snapshot).
    pub async fn user_ratings(
        &self,
        credential: &CodexCredential,
    ) -> Result<UserRatingsList, CodexError> {
        let req = self.get("/api/v1/user/ratings")?;
        self.send(Self::authed(req, credential), "/api/v1/user/ratings")
            .await
    }

    /// `GET /api/v1/series/{id}/rating` — the caller's rating/notes, if any.
    pub async fn series_rating(
        &self,
        credential: &CodexCredential,
        series_id: &str,
    ) -> Result<Option<UserSeriesRating>, CodexError> {
        let endpoint = format!("/api/v1/series/{series_id}/rating");
        let req = self.get(&endpoint)?;
        self.send(Self::authed(req, credential), &endpoint).await
    }

    /// `PUT /api/v1/series/{id}/rating` — write the caller's rating/notes
    /// back to Codex.
    ///
    /// Not exercised by the service layer yet: Codex currently gates this
    /// write under `series:read` (no dedicated rating-write permission), so a
    /// Shisho-minted key would leak write access. The write path stays dormant
    /// until Codex gates rating-write under its own permission and the minted
    /// key is granted it. The method is kept so the client is complete.
    pub async fn set_series_rating(
        &self,
        credential: &CodexCredential,
        series_id: &str,
        rating: &SetRating,
    ) -> Result<UserSeriesRating, CodexError> {
        let endpoint = format!("/api/v1/series/{series_id}/rating");
        let req = self.put(&endpoint)?.json(rating);
        self.send(Self::authed(req, credential), &endpoint).await
    }

    /// `POST /api/v1/api-keys` — mint a new, least-privilege Codex API key for
    /// Shisho's own use, authorized by the onboarding credential (a pasted
    /// `api-keys:write` key or a one-shot forwarded OIDC bearer). Codex returns
    /// the plaintext key once; the caller stores it encrypted and uses it for
    /// every subsequent per-user call. Codex requires the authorizing
    /// credential to already hold every scope granted.
    pub async fn mint_api_key(
        &self,
        authz: &CodexCredential,
        name: &str,
        scopes: &[Scope],
    ) -> Result<MintedApiKey, CodexError> {
        let req = self.post("/api/v1/api-keys")?.json(&MintApiKeyRequest {
            name: name.to_owned(),
            permissions: scopes.iter().map(|s| s.as_str().to_owned()).collect(),
            expires_at: None,
        });
        self.send(Self::authed(req, authz), "/api/v1/api-keys")
            .await
    }

    fn get(&self, path: &str) -> Result<reqwest::RequestBuilder, CodexError> {
        Ok(self.http.get(self.url(path)?))
    }

    fn post(&self, path: &str) -> Result<reqwest::RequestBuilder, CodexError> {
        Ok(self.http.post(self.url(path)?))
    }

    fn put(&self, path: &str) -> Result<reqwest::RequestBuilder, CodexError> {
        Ok(self.http.put(self.url(path)?))
    }

    fn url(&self, path: &str) -> Result<String, CodexError> {
        if self.base_url.is_empty() {
            return Err(CodexError::NotConfigured);
        }
        Ok(format!("{}{path}", self.base_url))
    }

    fn authed(
        req: reqwest::RequestBuilder,
        credential: &CodexCredential,
    ) -> reqwest::RequestBuilder {
        match credential {
            CodexCredential::ApiKey(key) => req.header("X-API-Key", key),
            CodexCredential::Bearer(token) => req.bearer_auth(token),
        }
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
        endpoint: &str,
    ) -> Result<T, CodexError> {
        let resp = req.send().await.map_err(|source| CodexError::Transport {
            endpoint: endpoint.to_owned(),
            source,
        })?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(CodexError::Unauthorized {
                endpoint: endpoint.to_owned(),
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CodexError::Api {
                status,
                endpoint: endpoint.to_owned(),
                body,
            });
        }

        resp.json().await.map_err(|source| CodexError::Decode {
            endpoint: endpoint.to_owned(),
            source,
        })
    }
}

/// The resolved caller: every per-user query is keyed by `user_sub` and
/// made with the caller's own credential (identity flows through;
/// credentials are never stored).
#[derive(Debug, Clone)]
pub struct UserCtx {
    /// Codex user ID from `GET /api/v1/auth/me`.
    pub user_sub: String,
    pub credential: CodexCredential,
}
