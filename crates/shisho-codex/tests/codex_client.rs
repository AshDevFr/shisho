//! Codex client fixture tests: recorded response bodies under
//! `tests/fixtures/codex/` replayed through wiremock. No live calls.

use shisho_codex::{
    CodexClient, CodexCredential, CodexError, SHISHO_KEY_SCOPES, SHISHO_READ_ONLY_SCOPES,
    SetRating, version_supported,
};
use shisho_ids::Provider;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/../../tests/fixtures/codex/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn client(server: &MockServer) -> CodexClient {
    CodexClient::new(&server.uri(), reqwest::Client::new())
}

fn api_key() -> CodexCredential {
    CodexCredential::ApiKey("test-key".into())
}

#[tokio::test]
async fn me_sends_api_key_header_and_decodes_user() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .and(header("X-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("auth_me")))
        .expect(1)
        .mount(&server)
        .await;

    let user = client(&server).me(&api_key()).await.expect("me");
    assert_eq!(user.id, "550e8400-e29b-41d4-a716-446655440000");
    assert_eq!(user.username, "ash");
    assert!(user.email_verified);
}

#[tokio::test]
async fn me_with_bearer_credential_uses_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .and(header("Authorization", "Bearer oidc-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("auth_me")))
        .expect(1)
        .mount(&server)
        .await;

    let cred = CodexCredential::Bearer("oidc-token".into());
    client(&server).me(&cred).await.expect("me");
}

#[tokio::test]
async fn unauthorized_maps_to_dedicated_error_variant() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = client(&server).me(&api_key()).await.unwrap_err();
    assert!(matches!(err, CodexError::Unauthorized { .. }), "{err:?}");
    // The message must be self-explanatory: it reaches the MCP caller.
    assert!(err.to_string().contains("expired or revoked"));
}

#[tokio::test]
async fn info_is_unauthenticated() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("info")))
        .expect(1)
        .mount(&server)
        .await;

    let info = client(&server).info().await.expect("info");
    assert_eq!(info.name, "codex");
}

#[tokio::test]
async fn list_libraries_returns_the_callers_libraries() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/libraries"))
        .and(header("X-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("libraries")))
        .expect(1)
        .mount(&server)
        .await;

    let libs = client(&server)
        .list_libraries(&api_key())
        .await
        .expect("list_libraries");
    assert_eq!(libs.len(), 2);
    assert_eq!(libs[0].name, "Manga");
    assert_eq!(libs[0].series_count, 42);
}

#[tokio::test]
async fn list_series_carries_unread_counts_and_external_ids_in_one_call() {
    // The load-bearing combined query for both candidate pools.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/full"))
        .and(query_param("page", "1"))
        .and(query_param("pageSize", "50"))
        .and(header("X-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("series_full_page1")))
        .expect(1)
        .mount(&server)
        .await;

    let page = client(&server)
        .list_series(&api_key(), 1, 50)
        .await
        .expect("list_series");

    assert_eq!(page.total, 2);
    assert_eq!(page.data.len(), 2);

    let frieren = &page.data[0];
    assert_eq!(frieren.metadata.title, "Sousou no Frieren");
    // Unread count and external IDs together in the same row.
    assert_eq!(frieren.unread_count, Some(11));
    assert_eq!(frieren.book_count, 11);
    assert_eq!(frieren.external_ids.len(), 3);

    // Normalization drops the non-provider source (comicinfo) from joins and
    // maps the rest onto canonical Tsundoku tokens.
    let normalized = frieren.normalized_external_ids();
    let joinable: Vec<_> = normalized.iter().filter_map(|e| e.as_tsundoku()).collect();
    assert_eq!(joinable, vec!["mangabaka:112092", "mal:126287"]);
    assert!(
        normalized
            .iter()
            .any(|e| e.provider == Provider::Other("comicinfo".into()))
    );

    // unread == 0 -> completed (read-state derivation input).
    assert_eq!(page.data[1].unread_count, Some(0));
}

#[tokio::test]
async fn search_series_posts_query() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/series/search"))
        .and(body_json(serde_json::json!({ "query": "dungeon" })))
        .and(header("X-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": fixture("series_search"),
            "page": 1, "pageSize": 50, "total": 1, "totalPages": 1
        })))
        .expect(1)
        .mount(&server)
        .await;

    let hits = client(&server)
        .search_series(&api_key(), "dungeon", None)
        .await
        .expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "Dungeon Meshi");
    assert_eq!(hits[0].unread_count, Some(0));
}

#[tokio::test]
async fn user_ratings_decodes_the_bulk_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user/ratings"))
        .and(header("X-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("user_ratings")))
        .expect(1)
        .mount(&server)
        .await;

    let list = client(&server)
        .user_ratings(&api_key())
        .await
        .expect("user ratings");
    assert_eq!(list.ratings.len(), 2);
    assert_eq!(list.ratings[0].rating, 85);
    assert!(list.ratings[1].notes.is_none());
}

#[tokio::test]
async fn rating_read_and_missing_rating_decodes_as_none() {
    let series_id = "550e8400-e29b-41d4-a716-446655440003";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/series/{series_id}/rating")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("rating")))
        .expect(1)
        .mount(&server)
        .await;

    let rating = client(&server)
        .series_rating(&api_key(), series_id)
        .await
        .expect("get rating")
        .expect("rating exists");
    assert_eq!(rating.rating, 85);

    // A JSON `null` body means the caller has no rating for the series.
    let empty = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/none/rating"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::Value::Null))
        .mount(&empty)
        .await;
    let none = client(&empty)
        .series_rating(&api_key(), "none")
        .await
        .expect("get rating");
    assert!(none.is_none());
}

#[tokio::test]
async fn unrated_series_answered_with_no_content_is_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/unrated/rating"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let none = client(&server)
        .series_rating(&api_key(), "unrated")
        .await
        .expect("204 is not an error");
    assert!(none.is_none());
}

#[tokio::test]
async fn non_success_statuses_surface_status_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/info"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let err = client(&server).info().await.unwrap_err();
    match err {
        CodexError::Api { status, body, .. } => {
            assert_eq!(status, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(body, "boom");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn unconfigured_base_url_errors_without_a_request() {
    let c = CodexClient::new("", reqwest::Client::new());
    assert!(!c.is_configured());
    let err = c.info().await.unwrap_err();
    assert!(matches!(err, CodexError::NotConfigured), "{err:?}");
}

#[tokio::test]
async fn mint_api_key_sends_least_priv_scopes_and_parses_the_one_time_key() {
    let server = MockServer::start().await;
    // Onboarding via a forwarded OIDC bearer (the api-keys:write path).
    Mock::given(method("POST"))
        .and(path("/api/v1/api-keys"))
        .and(header("Authorization", "Bearer onboarding-token"))
        .and(body_json(serde_json::json!({
            "name": "shisho",
            "permissions": ["series:read", "libraries:read", "books:read", "progress:read", "ratings:write"]
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(fixture("mint_api_key")))
        .expect(1)
        .mount(&server)
        .await;

    let cred = CodexCredential::Bearer("onboarding-token".into());
    let minted = client(&server)
        .mint_api_key(&cred, "shisho", SHISHO_KEY_SCOPES)
        .await
        .expect("mint");
    assert_eq!(minted.key, "codex_abc123_def456ghi789");
    assert_eq!(minted.key_prefix, "codex_abc123");
    assert_eq!(minted.id, "550e8400-e29b-41d4-a716-4466554400a0");
    assert!(minted.is_active);
    assert!(minted.expires_at.is_none());
}

#[test]
fn the_read_only_fallback_drops_only_the_write_scope() {
    let full: Vec<_> = SHISHO_KEY_SCOPES.iter().map(|s| s.as_str()).collect();
    let read_only: Vec<_> = SHISHO_READ_ONLY_SCOPES.iter().map(|s| s.as_str()).collect();
    assert!(full.contains(&"ratings:write"));
    assert!(!read_only.iter().any(|s| s.ends_with(":write")));
    assert_eq!(
        full.iter()
            .filter(|s| !s.ends_with(":write"))
            .collect::<Vec<_>>(),
        read_only.iter().collect::<Vec<_>>()
    );
}

#[test]
fn codex_versions_are_checked_against_the_minimum() {
    assert_eq!(version_supported("2.9.0"), Some(true));
    assert_eq!(version_supported("2.10.1"), Some(true));
    assert_eq!(version_supported("3.0.0-rc.1"), Some(true));
    assert_eq!(version_supported("v2.9.0"), Some(true));
    assert_eq!(version_supported("2.8.1"), Some(false));
    assert_eq!(version_supported("1.99.0"), Some(false));
    assert_eq!(version_supported("dev"), None);
}

#[tokio::test]
async fn a_403_reads_as_forbidden() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/series/s-1/rating"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string("Missing permission: ratings:write"),
        )
        .mount(&server)
        .await;

    let err = client(&server)
        .set_series_rating(
            &api_key(),
            "s-1",
            &SetRating {
                rating: 80,
                notes: None,
            },
        )
        .await
        .unwrap_err();
    assert!(err.is_forbidden(), "{err:?}");
}

#[tokio::test]
async fn set_series_rating_puts_rating_and_notes() {
    let series_id = "550e8400-e29b-41d4-a716-446655440003";
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(format!("/api/v1/series/{series_id}/rating")))
        .and(body_json(serde_json::json!({
            "rating": 85,
            "notes": "Great series, loved the art style!"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("rating")))
        .expect(1)
        .mount(&server)
        .await;

    let written = client(&server)
        .set_series_rating(
            &api_key(),
            series_id,
            &SetRating {
                rating: 85,
                notes: Some("Great series, loved the art style!".into()),
            },
        )
        .await
        .expect("put rating");
    assert_eq!(written.series_id, series_id);
}

#[tokio::test]
async fn series_books_carry_the_callers_read_progress() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/s-1/books"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "id": "b-1", "seriesId": "s-1", "title": "Vol. 1",
              "readProgress": { "id": "p-1", "bookId": "b-1", "userId": "u", "currentPage": 180,
                                "completed": true, "completedAt": "2026-05-01T10:00:00Z",
                                "startedAt": "2026-04-30T10:00:00Z",
                                "updatedAt": "2026-05-01T10:00:00Z", "progressPercentage": 1.0 } },
            { "id": "b-2", "seriesId": "s-1", "title": "Vol. 2" }
        ])))
        .mount(&server)
        .await;

    let books = client(&server)
        .series_books(&api_key(), "s-1")
        .await
        .expect("books");
    assert_eq!(books.len(), 2);
    let progress = books[0].read_progress.as_ref().expect("progress");
    assert!(progress.completed);
    assert_eq!(
        progress.updated_at.to_rfc3339(),
        "2026-05-01T10:00:00+00:00"
    );
    assert!(books[1].read_progress.is_none());
}
