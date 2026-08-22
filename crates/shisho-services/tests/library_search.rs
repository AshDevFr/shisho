//! `search_library` fixture tests: Codex server-side text search composed
//! with snapshot-side genre and read-state filters.

use shisho_codex::{CodexClient, CodexCredential};
use shisho_config::{CacheConfig, Secret};
use shisho_services::library::{LibraryQuery, search_library};
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

    let titles: Vec<&str> = results.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, vec!["Sousou no Frieren", "Already Read"]);
    // Snapshot data is merged in: read state, genres, external IDs.
    assert_eq!(results[0].read_state, ReadState::Untouched);
    assert_eq!(results[0].genres, vec!["Fantasy"]);
    assert!(
        results[0]
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

    let titles: Vec<&str> = results.iter().map(|s| s.title.as_str()).collect();
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

    let titles: Vec<&str> = results.iter().map(|s| s.title.as_str()).collect();
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

    let titles: Vec<&str> = results.iter().map(|s| s.title.as_str()).collect();
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
