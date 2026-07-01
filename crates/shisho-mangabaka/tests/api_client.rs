//! API client + provider fallback tests: responses recorded from the real
//! `api.mangabaka.dev` under `tests/fixtures/mangabaka/`, replayed through
//! wiremock. No live calls.

use shisho_config::{MangabakaConfig, Secret};
use shisho_ids::Provider;
use shisho_mangabaka::mapping::series_to_metadata;
use shisho_mangabaka::{MangabakaClient, MangabakaProvider, SeriesKind, SeriesStatus};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/../../tests/fixtures/mangabaka/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn client(server: &MockServer) -> MangabakaClient {
    MangabakaClient::new(
        server.uri(),
        Some("test-key".into()),
        std::time::Duration::from_secs(5),
    )
    .unwrap()
}

#[tokio::test]
async fn recorded_series_response_parses_and_maps() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/series/1677"))
        .and(header("x-api-key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("series_1677")))
        .expect(1)
        .mount(&server)
        .await;

    let series = client(&server).get_series("1677").await.unwrap().unwrap();
    let m = series_to_metadata(series);

    assert_eq!(m.mangabaka_id, 1677);
    assert_eq!(m.title, "Chainsaw Man");
    assert_eq!(m.year, Some(2018));
    assert_eq!(m.kind, Some(SeriesKind::Manga));
    assert_eq!(m.status, Some(SeriesStatus::Completed));
    assert_eq!(m.total_volumes, Some(24));
    assert_eq!(m.total_chapters, Some(232));
    // The recorded rating is 85.06... on MangaBaka's 0-100 scale.
    let rating = m.rating.expect("rating mapped");
    assert!((rating - 8.5063).abs() < 1e-3, "got {rating}");
    assert!(m.description.as_deref().unwrap_or("").contains("Denji"));
    assert!(!m.genres.is_empty());

    // Source IDs land on canonical providers (renames applied).
    let find = |p: Provider| {
        m.foreign_ids
            .iter()
            .find(|f| f.provider == p)
            .unwrap_or_else(|| panic!("missing {p} in {:?}", m.foreign_ids))
    };
    assert_eq!(find(Provider::Mal).id, "116778");
    assert_eq!(find(Provider::Anilist).id, "105778");
    assert_eq!(find(Provider::Mangaupdates).id, "ylx5wzn");
}

#[tokio::test]
async fn recorded_source_lookup_resolves_the_active_series() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/source/anilist/105778"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("source_anilist_105778")))
        .mount(&server)
        .await;

    let series = client(&server)
        .get_by_source_id("anilist", "105778")
        .await
        .unwrap()
        .expect("source lookup resolves");
    assert_eq!(series.id, 1677);
    assert_eq!(series.title, "Chainsaw Man");
}

#[tokio::test]
async fn recorded_search_returns_ranked_rows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/series/search"))
        .and(query_param("q", "chainsaw man"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("search_chainsaw_man")))
        .mount(&server)
        .await;

    let rows = client(&server).search("chainsaw man", 5).await.unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0].id, 1677);
    assert_eq!(rows[0].title, "Chainsaw Man");
}

#[tokio::test]
async fn not_found_maps_to_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/series/999999999"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(serde_json::json!({"status": 404, "message": "Not Found"})),
        )
        .mount(&server)
        .await;

    assert!(
        client(&server)
            .get_series("999999999")
            .await
            .unwrap()
            .is_none()
    );
}

/// Facade fallback path: with no dump on disk, an API hit resolves; an API
/// miss records a negative entry so the second call never reaches the
/// network (asserted via wiremock's expected call count).
#[tokio::test]
async fn provider_falls_back_to_api_once_and_caches_the_miss() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/series/1677"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("series_1677")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/series/424242"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(serde_json::json!({"status": 404, "message": "Not Found"})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::TempDir::new().unwrap();
    let cfg = MangabakaConfig {
        api_key: Secret::new("test-key"),
        api_base_url: server.uri(),
        offline_dump_url: None,
        ..Default::default()
    };
    let provider = MangabakaProvider::new(&cfg, dir.path().to_path_buf()).unwrap();

    let hit = provider.get("1677").await.unwrap().expect("api hit");
    assert_eq!(hit.title, "Chainsaw Man");
    assert_eq!(hit.origin.as_str(), "mangabaka-api");

    assert!(provider.get("424242").await.unwrap().is_none());
    // Second miss short-circuits on the negative cache: wiremock's
    // expect(1) on the 404 mock fails the test if a second request lands.
    assert!(provider.get("424242").await.unwrap().is_none());
    assert_eq!(provider.status().await.negative_cache_size, 1);
}
