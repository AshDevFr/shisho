//! Map MangaBaka payloads onto this crate's canonical types, and translate
//! `shisho_ids::Provider` tokens to MangaBaka source components.
//!
//! Translation rules:
//! - MangaBaka source keys → canonical providers: `manga_updates` →
//!   `mangaupdates`, `my_anime_list` → `mal`, the rest are identical to
//!   Tsundoku's tokens and parse through `Provider::from_tsundoku_token`.
//! - `type` strings → [`SeriesKind`] (unknown values land in `Other(_)`).
//! - `status` strings → [`SeriesStatus`] (MangaBaka uses `releasing` for
//!   ongoing series; `unknown` and missing both land in `Unknown`).
//! - `rating` is published 0-100 and normalized to 0-10.

use std::collections::BTreeMap;

use shisho_ids::{ExternalId, Provider};

use crate::client::{MbScaledImage, MbSeries};
use crate::types::{
    ContentRating, MetadataOrigin, SearchHit, SeriesKind, SeriesMetadata, SeriesStatus,
};

/// Canonical provider → MangaBaka's source component, used both as the
/// `/v1/source/{source}/{id}` URL segment and (by the offline store) to
/// pick the `source_*_id` column. `None` for `Mangabaka` itself (that is
/// the native ID, resolved by `get`, not a foreign source) and for
/// `Other` (unknown providers never reach resolution). The match is
/// exhaustive: adding a `Provider` variant forces a decision here.
pub fn provider_to_mb_source(provider: &Provider) -> Option<&'static str> {
    match provider {
        Provider::Mangabaka => None,
        Provider::Mangaupdates => Some("manga_updates"),
        Provider::Mal => Some("my_anime_list"),
        Provider::Anilist => Some("anilist"),
        Provider::Mangadex => Some("mangadex"),
        Provider::Kitsu => Some("kitsu"),
        Provider::AnimePlanet => Some("anime_planet"),
        Provider::AnimeNewsNetwork => Some("anime_news_network"),
        Provider::Shikimori => Some("shikimori"),
        Provider::Other(_) => None,
    }
}

/// Map a MangaBaka API payload to [`SeriesMetadata`] (origin: API).
pub fn series_to_metadata(series: MbSeries) -> SeriesMetadata {
    let alternate_titles = collect_alternates(&series);
    let kind = series.kind.as_deref().map(parse_kind);
    let status = series.status.as_deref().map(parse_status);
    let cover_url = pick_cover(series.cover.as_ref());
    let foreign_ids = source_to_foreign_ids(series.source.as_ref());

    SeriesMetadata {
        mangabaka_id: series.id,
        title: series.title,
        alternate_titles,
        kind,
        status,
        year: series.year,
        cover_url,
        total_volumes: parse_count(series.final_volume.as_deref()),
        total_chapters: parse_count(series.total_chapters.as_deref()),
        rating: normalize_rating(series.rating),
        description: series.description.filter(|s| !s.is_empty()),
        genres: series.genres.unwrap_or_default(),
        tags: series.tags.unwrap_or_default(),
        content_rating: series
            .content_rating
            .as_deref()
            .and_then(ContentRating::parse),
        authors: series.authors.unwrap_or_default(),
        foreign_ids,
        origin: MetadataOrigin::Api,
    }
}

/// Map a MangaBaka search-result row to a [`SearchHit`].
pub fn series_to_search_hit(series: &MbSeries) -> SearchHit {
    SearchHit {
        mangabaka_id: series.id,
        title: series.title.clone(),
        year: series.year,
        cover_url: pick_cover(series.cover.as_ref()),
        kind: series.kind.as_deref().map(parse_kind),
        score: None, // MangaBaka doesn't return a relevance score.
    }
}

/// MangaBaka publishes ratings on a 0-100 scale (e.g. `85.06`); the
/// canonical value is 0-10. Out-of-range or non-finite values are dropped
/// rather than displayed as junk.
pub(crate) fn normalize_rating(raw: Option<f64>) -> Option<f64> {
    let r = raw?;
    if !r.is_finite() || !(0.0..=100.0).contains(&r) {
        return None;
    }
    Some(r / 10.0)
}

/// MangaBaka returns `final_volume` / `total_chapters` as nullable JSON
/// strings (e.g. `"24"`). Anything non-numeric (or absent) becomes `None`;
/// decimal chapter numbers (e.g. `"232.5"`) truncate to the integer part.
pub(crate) fn parse_count(s: Option<&str>) -> Option<i32> {
    let s = s?.trim();
    if s.is_empty() {
        return None;
    }
    s.parse::<i32>()
        .ok()
        .or_else(|| s.parse::<f64>().ok().map(|f| f as i32))
}

pub(crate) fn parse_kind(s: &str) -> SeriesKind {
    match s {
        "manga" => SeriesKind::Manga,
        "manhwa" => SeriesKind::Manhwa,
        "manhua" => SeriesKind::Manhua,
        "novel" => SeriesKind::Novel,
        "one_shot" | "oneshot" => SeriesKind::OneShot,
        "oel" => SeriesKind::Oel,
        other => SeriesKind::Other(other.to_string()),
    }
}

pub(crate) fn parse_status(s: &str) -> SeriesStatus {
    match s {
        // MangaBaka uses "releasing" for ongoing series.
        "releasing" | "ongoing" => SeriesStatus::Ongoing,
        "completed" => SeriesStatus::Completed,
        "hiatus" => SeriesStatus::Hiatus,
        "cancelled" => SeriesStatus::Cancelled,
        "upcoming" => SeriesStatus::Upcoming,
        _ => SeriesStatus::Unknown,
    }
}

/// Best-effort cover URL: prefer the 350-wide variant (x2 retina), fall
/// back through smaller scales, then to the raw upload.
fn pick_cover(cover: Option<&crate::client::MbCover>) -> Option<String> {
    let cover = cover?;
    pick_scaled(cover.x350.as_ref())
        .or_else(|| pick_scaled(cover.x250.as_ref()))
        .or_else(|| pick_scaled(cover.x150.as_ref()))
        .or_else(|| cover.raw.as_ref().and_then(|r| r.url.clone()))
}

fn pick_scaled(scaled: Option<&MbScaledImage>) -> Option<String> {
    let s = scaled?;
    s.x2.clone()
        .or_else(|| s.x1.clone())
        .or_else(|| s.x3.clone())
}

/// Flatten `secondary_titles` (keyed by language) and the explicit
/// `native_title` / `romanized_title` fields into a deduplicated alternate
/// list, with the canonical title excluded.
fn collect_alternates(series: &MbSeries) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: Option<&str>| {
        if let Some(value) = s
            && !value.is_empty()
            && value != series.title
            && !out.iter().any(|existing| existing == value)
        {
            out.push(value.to_string());
        }
    };
    push(series.native_title.as_deref());
    push(series.romanized_title.as_deref());

    if let Some(obj) = series.secondary_titles.as_ref().and_then(|v| v.as_object()) {
        for entries in obj.values() {
            let Some(arr) = entries.as_array() else {
                continue;
            };
            for entry in arr {
                if let Some(t) = entry.get("title").and_then(|t| t.as_str()) {
                    push(Some(t));
                }
            }
        }
    }
    out
}

/// Map MangaBaka's `source` payload onto canonical external IDs. Unknown
/// source keys parse to `Provider::Other` and are dropped (they cannot
/// join anywhere).
fn source_to_foreign_ids(source: Option<&serde_json::Value>) -> Vec<ExternalId> {
    let Some(obj) = source.and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    // Iterate deterministically (BTreeMap by key) so output order is stable.
    let sorted: BTreeMap<&String, &serde_json::Value> = obj.iter().collect();
    let mut out = Vec::with_capacity(sorted.len());
    for (mb_key, value) in sorted {
        let token = match mb_key.as_str() {
            "manga_updates" => "mangaupdates",
            "my_anime_list" => "mal",
            other => other,
        };
        let provider = Provider::from_tsundoku_token(token);
        if matches!(provider, Provider::Other(_)) {
            continue;
        }
        let Some(id) = value
            .get("id")
            .and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            })
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        out.push(ExternalId {
            provider,
            id,
            provenance: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_payload() -> serde_json::Value {
        serde_json::json!({
            "id": 1677,
            "title": "Chainsaw Man",
            "native_title": "チェンソーマン",
            "romanized_title": "Chensō Man",
            "year": 2018,
            "final_volume": "11",
            "total_chapters": "97",
            "rating": 85.0,
            "type": "manga",
            "status": "releasing",
            "secondary_titles": {
                "en": [{"type": "alternative", "title": "Chainsaw-Man"}],
                "ja": [{"type": "native", "title": "チェンソーマン"}]
            },
            "cover": {
                "raw": {"url": "https://mangabaka/raw.jpg"},
                "x350": {"x1": "https://mangabaka/350.jpg", "x2": "https://mangabaka/350@2x.jpg"}
            },
            "source": {
                "anilist": {"id": 105778},
                "my_anime_list": {"id": 116778},
                "manga_updates": {"id": 174610},
                "mangadex": {"id": "a77742b1-befd-49a4-bff5-1ad4375089ee"},
                "not_a_provider": {"id": 1}
            },
            "genres": ["action", "horror"],
            "content_rating": "suggestive",
            "authors": ["Tatsuki Fujimoto"]
        })
    }

    #[test]
    fn content_rating_and_authors_map_from_the_api() {
        let s: MbSeries = serde_json::from_value(sample_payload()).unwrap();
        let m = series_to_metadata(s);
        assert_eq!(m.content_rating, Some(ContentRating::Suggestive));
        assert_eq!(m.authors, vec!["Tatsuki Fujimoto".to_string()]);
    }

    #[test]
    fn maps_canonical_fields() {
        let s: MbSeries = serde_json::from_value(sample_payload()).unwrap();
        let m = series_to_metadata(s);
        assert_eq!(m.mangabaka_id, 1677);
        assert_eq!(m.title, "Chainsaw Man");
        assert_eq!(m.year, Some(2018));
        assert_eq!(m.kind, Some(SeriesKind::Manga));
        assert_eq!(m.status, Some(SeriesStatus::Ongoing));
        assert_eq!(m.cover_url.as_deref(), Some("https://mangabaka/350@2x.jpg"));
        assert!(m.genres.contains(&"action".to_string()));
        assert_eq!(m.total_volumes, Some(11));
        assert_eq!(m.total_chapters, Some(97));
        // 0-100 raw value normalized to the canonical 0-10 scale.
        assert_eq!(m.rating, Some(8.5));
        assert_eq!(m.origin, MetadataOrigin::Api);
    }

    #[test]
    fn normalize_rating_drops_out_of_range_and_non_finite() {
        assert_eq!(normalize_rating(Some(85.0)), Some(8.5));
        assert_eq!(normalize_rating(Some(0.0)), Some(0.0));
        assert_eq!(normalize_rating(Some(100.0)), Some(10.0));
        assert_eq!(normalize_rating(Some(-1.0)), None);
        assert_eq!(normalize_rating(Some(100.1)), None);
        assert_eq!(normalize_rating(Some(f64::NAN)), None);
        assert_eq!(normalize_rating(None), None);
    }

    #[test]
    fn parse_count_handles_strings_decimals_and_junk() {
        assert_eq!(parse_count(Some("24")), Some(24));
        assert_eq!(parse_count(Some("  232 ")), Some(232));
        assert_eq!(parse_count(Some("232.5")), Some(232));
        assert_eq!(parse_count(Some("")), None);
        assert_eq!(parse_count(Some("   ")), None);
        assert_eq!(parse_count(Some("?")), None);
        assert_eq!(parse_count(None), None);
    }

    #[test]
    fn collects_alternate_titles_dedup_and_skip_canonical() {
        let s: MbSeries = serde_json::from_value(sample_payload()).unwrap();
        let m = series_to_metadata(s);
        assert!(!m.alternate_titles.contains(&"Chainsaw Man".to_string()));
        assert!(m.alternate_titles.contains(&"Chainsaw-Man".to_string()));
        // Dedup: チェンソーマン appears in native_title and secondary_titles.ja.
        let count = m
            .alternate_titles
            .iter()
            .filter(|t| t.as_str() == "チェンソーマン")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn maps_source_keys_to_canonical_providers_and_drops_unknown() {
        let s: MbSeries = serde_json::from_value(sample_payload()).unwrap();
        let m = series_to_metadata(s);

        let mut providers: Vec<String> = m
            .foreign_ids
            .iter()
            .map(|f| f.provider.to_string())
            .collect();
        providers.sort();
        assert_eq!(
            providers,
            vec!["anilist", "mal", "mangadex", "mangaupdates"]
        );

        // Numeric ids stringified.
        let anilist = m
            .foreign_ids
            .iter()
            .find(|f| f.provider == Provider::Anilist)
            .unwrap();
        assert_eq!(anilist.id, "105778");
        // String ids preserved.
        let mangadex = m
            .foreign_ids
            .iter()
            .find(|f| f.provider == Provider::Mangadex)
            .unwrap();
        assert_eq!(mangadex.id, "a77742b1-befd-49a4-bff5-1ad4375089ee");
    }

    #[test]
    fn falls_back_to_raw_cover_when_no_scaled_variant_present() {
        let mut payload = sample_payload();
        payload["cover"]["x350"] = serde_json::Value::Null;
        let s: MbSeries = serde_json::from_value(payload).unwrap();
        let m = series_to_metadata(s);
        assert_eq!(m.cover_url.as_deref(), Some("https://mangabaka/raw.jpg"));
    }

    #[test]
    fn unknown_kind_lands_in_other_variant() {
        let mut payload = sample_payload();
        payload["type"] = serde_json::Value::String("doujinshi".into());
        let s: MbSeries = serde_json::from_value(payload).unwrap();
        let m = series_to_metadata(s);
        assert_eq!(m.kind, Some(SeriesKind::Other("doujinshi".into())));
    }

    #[test]
    fn search_hit_uses_canonical_title_and_cover() {
        let s: MbSeries = serde_json::from_value(sample_payload()).unwrap();
        let hit = series_to_search_hit(&s);
        assert_eq!(hit.mangabaka_id, 1677);
        assert_eq!(hit.title, "Chainsaw Man");
        assert_eq!(hit.year, Some(2018));
        assert!(hit.cover_url.is_some());
    }

    /// Every named provider token maps to its MangaBaka source component;
    /// the native token and unknown providers map to none.
    #[test]
    fn provider_source_map_is_exhaustive_and_rejects_other() {
        assert_eq!(
            provider_to_mb_source(&Provider::Mangaupdates),
            Some("manga_updates")
        );
        assert_eq!(provider_to_mb_source(&Provider::Mal), Some("my_anime_list"));
        assert_eq!(provider_to_mb_source(&Provider::Anilist), Some("anilist"));
        assert_eq!(provider_to_mb_source(&Provider::Mangadex), Some("mangadex"));
        assert_eq!(provider_to_mb_source(&Provider::Kitsu), Some("kitsu"));
        assert_eq!(
            provider_to_mb_source(&Provider::AnimePlanet),
            Some("anime_planet")
        );
        assert_eq!(
            provider_to_mb_source(&Provider::AnimeNewsNetwork),
            Some("anime_news_network")
        );
        assert_eq!(
            provider_to_mb_source(&Provider::Shikimori),
            Some("shikimori")
        );
        // The native identity is not a foreign source.
        assert_eq!(provider_to_mb_source(&Provider::Mangabaka), None);
        // Unknown providers are rejected from resolution, never a panic.
        assert_eq!(
            provider_to_mb_source(&Provider::Other("fandom".into())),
            None
        );
    }
}
