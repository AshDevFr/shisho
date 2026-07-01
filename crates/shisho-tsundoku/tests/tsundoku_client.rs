//! Tsundoku client fixture tests: recorded response bodies under
//! `tests/fixtures/tsundoku/` replayed through wiremock. No live calls.

use shisho_config::Secret;
use shisho_ids::Provider;
use shisho_tsundoku::{MatchMode, Order, SeriesListQuery, TsundokuClient, TsundokuError};
use wiremock::matchers::{body_json, header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/../../tests/fixtures/tsundoku/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn client(server: &MockServer) -> TsundokuClient {
    TsundokuClient::new(
        &server.uri(),
        Secret::new("admin-token"),
        reqwest::Client::new(),
    )
}

#[tokio::test]
async fn health_sends_admin_bearer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/health"))
        .and(header("Authorization", "Bearer admin-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("health")))
        .expect(1)
        .mount(&server)
        .await;

    let health = client(&server).health().await.expect("health");
    assert_eq!(health.status, "ok");
}

#[tokio::test]
async fn list_series_encodes_the_acquirable_pool_filters() {
    // The exact filter combination phase 1 relies on for the acquirable
    // pool: kind=manga + hasReleases + genre filter + rating sort + the
    // owned=false bandwidth pre-filter.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series"))
        .and(query_param("kind", "manga"))
        .and(query_param("hasReleases", "true"))
        .and(query_param("owned", "false"))
        .and(query_param("genres", "Fantasy,Drama"))
        .and(query_param("genresMode", "any"))
        .and(query_param("sort", "rating"))
        .and(query_param("order", "desc"))
        .and(query_param("page", "0"))
        .and(query_param("pageSize", "50"))
        .and(query_param_is_missing("tags"))
        .and(query_param_is_missing("q"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("series_page")))
        .expect(1)
        .mount(&server)
        .await;

    let query = SeriesListQuery {
        page: Some(0),
        page_size: Some(50),
        kind: Some("manga".into()),
        owned: Some(false),
        has_releases: Some(true),
        genres: Some(vec!["Fantasy".into(), "Drama".into()]),
        genres_mode: Some(MatchMode::Any),
        sort: Some("rating".into()),
        order: Some(Order::Desc),
        ..Default::default()
    };
    let page = client(&server).list_series(&query).await.expect("list");

    assert_eq!(page.total, 2);
    assert_eq!(page.items[0].canonical_title, "Witch Hat Atelier");
    assert_eq!(page.items[0].rating, Some(8.7));
    assert!(!page.items[0].owned);

    // The Codex overlay arrives pre-joined with the series UUID.
    let overlay = page.items[1].codex.as_ref().expect("codex overlay");
    assert_eq!(overlay.status, "complete");
    assert_eq!(overlay.series_uuid, "550e8400-e29b-41d4-a716-446655440003");
}

#[tokio::test]
async fn series_detail_external_ids_normalize_through_provider() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/101"))
        .and(header("Authorization", "Bearer admin-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("series_detail")))
        .expect(1)
        .mount(&server)
        .await;

    let detail = client(&server).series_detail(101).await.expect("detail");
    assert_eq!(detail.canonical_title, "Witch Hat Atelier");
    assert!(detail.description.is_some());
    assert_eq!(detail.rating, Some(8.7));

    let normalized: Vec<_> = detail.external_ids.iter().map(|e| e.normalized()).collect();
    assert_eq!(normalized[0].provider, Provider::Mangabaka);
    assert_eq!(
        normalized[0].as_tsundoku().as_deref(),
        Some("mangabaka:53104")
    );
    assert_eq!(normalized[1].provider, Provider::Mal);
    // Tokens Shisho doesn't know yet parse to Other and stay out of joins.
    assert_eq!(
        normalized[2].provider,
        Provider::Other("weird_future_source".into())
    );
    assert_eq!(normalized[2].as_tsundoku(), None);
}

#[tokio::test]
async fn feed_pages_with_cursor() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/feed"))
        .and(query_param_is_missing("cursor"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("feed_page1")))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server);
    let first = c.feed(None, None).await.expect("feed page 1");
    assert!(first.has_more);
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.items[0].volume_coverage[0].start, 1.0);
    assert_eq!(first.items[0].volume_coverage[0].end, 13.0);
    let cursor = first.next_cursor.expect("cursor");

    let server2 = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/feed"))
        .and(query_param("cursor", cursor.as_str()))
        .and(query_param("limit", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("feed_empty")))
        .expect(1)
        .mount(&server2)
        .await;

    // An empty page = nothing changed since the cursor.
    let second = client(&server2)
        .feed(Some(&cursor), Some(100))
        .await
        .expect("feed page 2");
    assert!(second.items.is_empty());
    assert!(!second.has_more);
    assert!(second.next_cursor.is_none());
}

#[tokio::test]
async fn filtered_feed_posts_verbatim_external_id_strings() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .and(body_json(serde_json::json!({
            "externalIds": ["mangabaka:53104", "mal:103897"]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("feed_page1")))
        .expect(1)
        .mount(&server)
        .await;

    let ids = vec!["mangabaka:53104".to_owned(), "mal:103897".to_owned()];
    let resp = client(&server)
        .feed_filtered(None, None, &ids)
        .await
        .expect("filtered feed");
    assert_eq!(resp.items.len(), 1);
}

#[tokio::test]
async fn provider_search_decodes_dice_ranked_hits() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/providers/mangabaka/search"))
        .and(query_param("q", "witch hat"))
        .and(query_param("limit", "10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("provider_search")))
        .expect(1)
        .mount(&server)
        .await;

    let resp = client(&server)
        .provider_search("mangabaka", "witch hat", Some(10))
        .await
        .expect("search");
    assert_eq!(resp.provider, "mangabaka");
    assert_eq!(resp.hits.len(), 2);
    assert!(resp.hits[0].score > resp.hits[1].score);
    assert_eq!(resp.hits[0].title, "Witch Hat Atelier");
}

#[tokio::test]
async fn unauthorized_maps_to_dedicated_error_variant() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/health"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = client(&server).health().await.unwrap_err();
    assert!(matches!(err, TsundokuError::Unauthorized { .. }), "{err:?}");
}

#[tokio::test]
async fn unconfigured_base_url_errors_without_a_request() {
    let c = TsundokuClient::new("", Secret::default(), reqwest::Client::new());
    assert!(!c.is_configured());
    let err = c.health().await.unwrap_err();
    assert!(matches!(err, TsundokuError::NotConfigured), "{err:?}");
}
