//! `search_library` fixture tests: Codex server-side text search composed
//! with snapshot-side genre and read-state filters.

use shisho_codex::{CodexClient, CodexCredential};
use shisho_config::{CacheConfig, Secret};
use shisho_services::library::{LibraryQuery, LibrarySort, search_library};
use shisho_services::read_state::ReadState;
use shisho_services::{Services, UserCtx};
use shisho_tsundoku::TsundokuClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/../../tests/fixtures/codex/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

async fn services(codex: &MockServer) -> Services {
    let db = sea_orm::Database::connect("sqlite::memory:")
        .await
        .expect("connect");
    let http = reqwest::Client::new();
    Services::new(
        CodexClient::new(&codex.uri(), http.clone()),
        TsundokuClient::new("", Secret::default(), http),
        mangabaka_inert(),
        db,
        &CacheConfig::default(),
        false,
    )
}

fn user() -> UserCtx {
    UserCtx {
        user_sub: "user-1".into(),
        credential: CodexCredential::ApiKey("test-key".into()),
    }
}

async fn mount_codex(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/v1/user/ratings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("user_ratings")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/full"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("candidates_library")))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/series/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("library_search")))
        .mount(server)
        .await;
}

#[tokio::test]
async fn text_query_uses_codex_server_side_search() {
    let codex = MockServer::start().await;
    mount_codex(&codex).await;

    let results = search_library(
        &services(&codex).await,
        &user(),
        &LibraryQuery {
            text: Some("frieren".into()),
            ..Default::default()
        },
    )
    .await
    .expect("text search");

    let titles: Vec<&str> = results.series.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, vec!["Sousou no Frieren", "Already Read"]);
    // Snapshot data is merged in: read state, genres, external IDs.
    assert_eq!(results.series[0].read_state, ReadState::Untouched);
    assert_eq!(results.series[0].genres, vec!["Fantasy"]);
    assert!(
        results.series[0]
            .external_ids
            .contains(&"mangabaka:112092".to_owned())
    );
}

#[tokio::test]
async fn genre_query_filters_from_the_snapshot() {
    let codex = MockServer::start().await;
    mount_codex(&codex).await;

    let results = search_library(
        &services(&codex).await,
        &user(),
        &LibraryQuery {
            genre: Some("mystery".into()),
            ..Default::default()
        },
    )
    .await
    .expect("genre search");

    let titles: Vec<&str> = results.series.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["Obscure Title"],
        "case-insensitive genre match"
    );
}

#[tokio::test]
async fn state_query_returns_the_read_state_slice() {
    let codex = MockServer::start().await;
    mount_codex(&codex).await;

    let results = search_library(
        &services(&codex).await,
        &user(),
        &LibraryQuery {
            state: Some(ReadState::Completed),
            ..Default::default()
        },
    )
    .await
    .expect("state search");

    let titles: Vec<&str> = results.series.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, vec!["Already Read"]);
}

#[tokio::test]
async fn state_filter_composes_with_text_queries() {
    let codex = MockServer::start().await;
    mount_codex(&codex).await;

    // The text search matches lib-a (untouched) and lib-e (completed);
    // the state filter narrows to the untouched one.
    let results = search_library(
        &services(&codex).await,
        &user(),
        &LibraryQuery {
            text: Some("anything".into()),
            state: Some(ReadState::Untouched),
            ..Default::default()
        },
    )
    .await
    .expect("composed search");

    let titles: Vec<&str> = results.series.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, vec!["Sousou no Frieren"]);
}

/// An inert MangaBaka provider (no dump, no API fallback): resolves
/// nothing, touches nothing on disk.
fn mangabaka_inert() -> std::sync::Arc<shisho_mangabaka::MangabakaProvider> {
    let cfg = shisho_config::MangabakaConfig {
        api_fallback: false,
        offline_dump_url: None,
        ..Default::default()
    };
    std::sync::Arc::new(
        shisho_mangabaka::MangabakaProvider::new(
            &cfg,
            std::env::temp_dir().join("shisho-mangabaka-inert"),
        )
        .expect("inert mangabaka provider"),
    )
}

#[tokio::test]
async fn rating_sort_puts_the_top_shelf_first() {
    let codex = MockServer::start().await;
    // lib-e (Already Read) rated 90, lib-a (Frieren) rated 40, rest unrated.
    Mock::given(method("GET"))
        .and(path("/api/v1/user/ratings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ratings": [
                { "id": "r1", "seriesId": "lib-a", "rating": 40, "notes": null },
                { "id": "r2", "seriesId": "lib-e", "rating": 90, "notes": "a favourite" },
            ]
        })))
        .mount(&codex)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/full"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("candidates_library")))
        .mount(&codex)
        .await;

    let results = search_library(
        &services(&codex).await,
        &user(),
        &LibraryQuery {
            sort: Some(LibrarySort::Rating),
            ..Default::default()
        },
    )
    .await
    .expect("rating sort");

    let ranked: Vec<(&str, Option<i32>)> = results
        .series
        .iter()
        .map(|s| (s.title.as_str(), s.rating))
        .collect();
    assert_eq!(ranked[0], ("Already Read", Some(90)));
    assert_eq!(ranked[1], ("Sousou no Frieren", Some(40)));
    assert!(ranked[2..].iter().all(|(_, r)| r.is_none()), "unrated last");
}

#[test]
fn library_sort_schema_is_a_flat_inline_string_enum() {
    let schema = serde_json::to_value(schemars::schema_for!(LibrarySort)).unwrap();
    assert!(schema.get("oneOf").is_none(), "no oneOf: {schema}");
    assert_eq!(schema["type"], "string");
}

#[tokio::test]
async fn a_page_reports_the_full_match_count_and_pages_with_offset() {
    let codex = MockServer::start().await;
    mount_codex(&codex).await;
    let services = services(&codex).await;

    let first = search_library(
        &services,
        &user(),
        &LibraryQuery {
            limit: Some(4),
            ..Default::default()
        },
    )
    .await
    .expect("first page");
    assert_eq!(first.total, 6);
    assert_eq!(first.series.len(), 4);

    let rest = search_library(
        &services,
        &user(),
        &LibraryQuery {
            limit: Some(4),
            offset: Some(4),
            ..Default::default()
        },
    )
    .await
    .expect("second page");
    assert_eq!(rest.total, 6);
    let titles: Vec<&str> = rest.series.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, vec!["Already Read", "Verdicted By MangaBaka Id"]);
}
