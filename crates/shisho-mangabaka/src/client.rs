//! MangaBaka HTTP client (`api.mangabaka.org`, `x-api-key` header).
//!
//! Internal to this crate: callers go through [`crate::MangabakaProvider`].
//! The client only knows about MangaBaka's URL shape and JSON envelope; the
//! mapping to [`crate::types::SeriesMetadata`] happens in [`crate::mapping`].
//!
//! No rate limiter by design: Shisho's call volume is one user's lookups on
//! the fallback path, not a sweep. Revisit only if measured need arises.

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::Deserialize;

use crate::error::MangabakaError;

/// MangaBaka's JSON envelope. `status` mirrors the HTTP status code.
#[derive(Debug, Deserialize)]
pub struct MbEnvelope<T> {
    #[allow(dead_code)]
    pub status: i32,
    pub data: T,
}

/// `GET /v1/source/{source}/{id}` payload: the series cross-referencing
/// that foreign ID. The endpoint returns a *list* (a foreign ID can map to
/// several MangaBaka rows when upstream entries were merged); resolution
/// picks the first active row.
#[derive(Debug, Deserialize)]
pub struct MbSourceLookup {
    #[serde(default)]
    #[allow(dead_code)]
    pub source_response: Option<serde_json::Value>,
    #[serde(default)]
    pub series: Vec<MbSeries>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MbSeries {
    pub id: i64,
    #[serde(default)]
    pub state: Option<String>,
    pub title: String,
    #[serde(default)]
    pub native_title: Option<String>,
    #[serde(default)]
    pub romanized_title: Option<String>,
    /// `{ "en": [{type, title, note?}], "ja": [...] }`.
    #[serde(default)]
    pub secondary_titles: Option<serde_json::Value>,
    #[serde(default)]
    pub cover: Option<MbCover>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub year: Option<i32>,
    /// Last/final volume number, as a nullable string.
    #[serde(default)]
    pub final_volume: Option<String>,
    /// Total chapter count, as a nullable string.
    #[serde(default)]
    pub total_chapters: Option<String>,
    /// Average user rating on a 0-100 scale (e.g. `85.06` ≈ 8.5/10). The
    /// mapping layer divides by 10 so the canonical value is always 0-10.
    #[serde(default)]
    pub rating: Option<f64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    #[serde(rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub genres: Option<Vec<String>>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// `{anilist: {id, ...}, my_anime_list: {...}, manga_updates: {...},
    ///  mangadex: {...}, kitsu: {...}, anime_planet: {...},
    ///  anime_news_network: {...}, shikimori: {...}}`
    #[serde(default)]
    pub source: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MbCover {
    #[serde(default)]
    pub raw: Option<MbCoverRaw>,
    #[serde(default)]
    pub x350: Option<MbScaledImage>,
    #[serde(default)]
    pub x250: Option<MbScaledImage>,
    #[serde(default)]
    pub x150: Option<MbScaledImage>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MbCoverRaw {
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MbScaledImage {
    #[serde(default)]
    pub x1: Option<String>,
    #[serde(default)]
    pub x2: Option<String>,
    #[serde(default)]
    pub x3: Option<String>,
}

/// Thin HTTP client around the MangaBaka v1 REST API. Stateless apart from
/// the API key.
pub struct MangabakaClient {
    http: Client,
    base_url: String,
    api_key: Option<String>,
}

impl MangabakaClient {
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        timeout: Duration,
    ) -> Result<Self, MangabakaError> {
        let http = Client::builder()
            .timeout(timeout)
            .user_agent(concat!("shisho/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| MangabakaError::NotConfigured {
                message: format!("building MangaBaka http client: {e}"),
            })?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
        })
    }

    /// `GET /v1/series/{id}`. Returns `Ok(None)` on 404.
    pub async fn get_series(&self, id: &str) -> Result<Option<MbSeries>, MangabakaError> {
        let url = format!("{}/v1/series/{}", self.base_url, urlencode(id));
        match self.request_envelope::<MbSeries>(&url).await? {
            EnvelopeOutcome::Ok(series) => Ok(Some(series)),
            EnvelopeOutcome::NotFound => Ok(None),
        }
    }

    /// `GET /v1/series/search?q=...&page=1&limit=N`. One page of results.
    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<MbSeries>, MangabakaError> {
        let url = format!(
            "{}/v1/series/search?q={}&page=1&limit={}",
            self.base_url,
            urlencode(query),
            limit
        );
        match self.request_envelope::<Vec<MbSeries>>(&url).await? {
            EnvelopeOutcome::Ok(rows) => Ok(rows),
            EnvelopeOutcome::NotFound => Ok(Vec::new()),
        }
    }

    /// `GET /v1/source/{source}/{id}`: foreign provider ID → MangaBaka
    /// series. Returns `Ok(None)` on 404 or when the cross-reference list
    /// comes back empty. Prefers the first `active` row over merged ones.
    pub async fn get_by_source_id(
        &self,
        source: &str,
        id: &str,
    ) -> Result<Option<MbSeries>, MangabakaError> {
        let url = format!(
            "{}/v1/source/{}/{}",
            self.base_url,
            urlencode(source),
            urlencode(id)
        );
        match self.request_envelope::<MbSourceLookup>(&url).await? {
            EnvelopeOutcome::Ok(lookup) => {
                let active = lookup
                    .series
                    .iter()
                    .position(|s| s.state.as_deref() == Some("active"))
                    .unwrap_or(0);
                Ok(lookup.series.into_iter().nth(active))
            }
            EnvelopeOutcome::NotFound => Ok(None),
        }
    }

    async fn request_envelope<T>(&self, url: &str) -> Result<EnvelopeOutcome<T>, MangabakaError>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        let mut req = self.http.get(url).header("accept", "application/json");
        if let Some(key) = &self.api_key {
            req = req.header("x-api-key", key);
        }

        let resp = req.send().await.map_err(|e| MangabakaError::Transport {
            endpoint: url.to_owned(),
            source: e,
        })?;

        match resp.status() {
            StatusCode::OK => {
                let bytes = resp.bytes().await.map_err(|e| MangabakaError::Transport {
                    endpoint: url.to_owned(),
                    source: e,
                })?;
                let env: MbEnvelope<T> =
                    serde_json::from_slice(&bytes).map_err(|e| MangabakaError::Malformed {
                        endpoint: url.to_owned(),
                        message: e.to_string(),
                    })?;
                Ok(EnvelopeOutcome::Ok(env.data))
            }
            StatusCode::NOT_FOUND => Ok(EnvelopeOutcome::NotFound),
            StatusCode::TOO_MANY_REQUESTS => {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(Duration::from_secs);
                Err(MangabakaError::RateLimited { retry_after })
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(MangabakaError::AuthFailed),
            other => Err(MangabakaError::Api {
                status: other,
                endpoint: url.to_owned(),
            }),
        }
    }
}

enum EnvelopeOutcome<T> {
    Ok(T),
    NotFound,
}

/// Minimal URL component encoder. MangaBaka source ids are numeric and slug
/// names are ASCII identifiers, so a full URL encoder is overkill; we just
/// percent-encode the characters that would break path parsing.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_deserializes_single_series() {
        let raw = r#"{
            "status": 200,
            "data": {
                "id": 1,
                "title": "Chainsaw Man",
                "year": 2018,
                "type": "manga",
                "status": "releasing",
                "source": {
                    "anilist": { "id": 105778 },
                    "my_anime_list": { "id": 116778 }
                }
            }
        }"#;
        let env: MbEnvelope<MbSeries> = serde_json::from_str(raw).unwrap();
        assert_eq!(env.data.id, 1);
        assert_eq!(env.data.title, "Chainsaw Man");
        assert_eq!(env.data.year, Some(2018));
        assert_eq!(env.data.kind.as_deref(), Some("manga"));
    }

    #[test]
    fn envelope_deserializes_source_lookup_list() {
        let raw = r#"{
            "status": 200,
            "data": {
                "source_response": null,
                "series": [
                    {"id": 9, "title": "Old Merge", "state": "merged"},
                    {"id": 1677, "title": "Chainsaw Man", "state": "active"}
                ]
            }
        }"#;
        let env: MbEnvelope<MbSourceLookup> = serde_json::from_str(raw).unwrap();
        assert_eq!(env.data.series.len(), 2);
        assert_eq!(env.data.series[1].state.as_deref(), Some("active"));
    }

    #[test]
    fn urlencode_passes_safe_chars_through() {
        assert_eq!(urlencode("123"), "123");
        assert_eq!(urlencode("manga-updates"), "manga-updates");
        assert_eq!(urlencode("hello world"), "hello%20world");
        assert_eq!(urlencode("a/b"), "a%2Fb");
    }
}
