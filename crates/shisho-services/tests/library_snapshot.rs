//! Library snapshot fixture tests: multi-page Codex pagination replayed
//! through wiremock, folded into the owned set / read states / ratings
//! presence / unresolvable list.

use std::time::Duration;

use shisho_cache::{Freshness, SwrConfig, SwrError};
use shisho_codex::{CodexClient, CodexCredential, CodexError};
use shisho_ids::{ExternalId, Provider};
use shisho_services::library_snapshot::LibrarySnapshotService;
use shisho_services::read_state::ReadState;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/../../tests/fixtures/codex/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn service(server: &MockServer) -> LibrarySnapshotService {
    LibrarySnapshotService::new(
        CodexClient::new(&server.uri(), reqwest::Client::new()),
        SwrConfig {
            revalidate_after: Duration::from_secs(900),
            build_budget: Duration::from_secs(30),
        },
    )
}

async fn mount_happy_path(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/v1/user/ratings"))
        .and(header("X-API-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("user_ratings")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/full"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("snapshot_page1")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/full"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("snapshot_page2")))
        .mount(server)
        .await;
}

#[tokio::test]
async fn snapshot_folds_multiple_pages() {
    let server = MockServer::start().await;
    mount_happy_path(&server).await;

    let hit = service(&server)
        .get("user-1", &CodexCredential::ApiKey("test-key".into()))
        .await
        .expect("snapshot");
    assert_eq!(hit.freshness, Freshness::Fresh);

    let snapshot = hit.value;
    assert_eq!(snapshot.series.len(), 3, "both pages folded");

    // Owned set carries every mappable ID from every page, normalized.
    for raw in ["mangabaka:112092", "mal:126287", "anilist:86952"] {
        let id = ExternalId::from_tsundoku(raw).unwrap();
        assert!(snapshot.owns(&id), "owned set must contain {raw}");
    }
    assert_eq!(snapshot.owned_len(), 3);

    // Read states derive per series.
    let frieren = snapshot
        .series_by_uuid("550e8400-e29b-41d4-a716-446655440002")
        .unwrap();
    assert_eq!(frieren.read_state, ReadState::Untouched);
    let meshi = snapshot
        .series_by_uuid("550e8400-e29b-41d4-a716-446655440003")
        .unwrap();
    assert_eq!(meshi.read_state, ReadState::Completed);

    // Rating values and notes from the bulk ratings call.
    assert_eq!(meshi.rating, Some(85));
    assert_eq!(
        meshi.rating_notes.as_deref(),
        Some("Great series, loved the art style!")
    );
    assert_eq!(frieren.rating, None);

    // Genres flattened for filtering.
    assert_eq!(frieren.genres, vec!["Fantasy", "Adventure"]);
}

#[tokio::test]
async fn unknown_providers_are_excluded_but_counted_as_unresolvable() {
    let server = MockServer::start().await;
    mount_happy_path(&server).await;

    let snapshot = service(&server)
        .get("user-1", &CodexCredential::ApiKey("test-key".into()))
        .await
        .expect("snapshot")
        .value;

    // Frieren has a comicinfo ID alongside mappable ones: not unresolvable,
    // and the comicinfo ID never enters the owned set.
    let frieren = snapshot
        .series_by_uuid("550e8400-e29b-41d4-a716-446655440002")
        .unwrap();
    assert_eq!(frieren.external_ids.len(), 2);
    assert!(
        frieren
            .external_ids
            .iter()
            .all(|id| !matches!(id.provider, Provider::Other(_)))
    );

    // The page-2 series has only a comicinfo ID: unresolvable, retained
    // verbatim for Status, never patched.
    assert_eq!(snapshot.unresolvable.len(), 1);
    let unresolvable = &snapshot.unresolvable[0];
    assert_eq!(
        unresolvable.series_id,
        "550e8400-e29b-41d4-a716-446655440004"
    );
    assert_eq!(unresolvable.raw_sources, vec!["comicinfo:doujin-local"]);

    // But the series itself still appears in the snapshot (it is owned and
    // readable; it just cannot join Tsundoku).
    assert!(
        snapshot
            .series_by_uuid("550e8400-e29b-41d4-a716-446655440004")
            .is_some()
    );
}

#[tokio::test]
async fn second_get_is_served_from_cache() {
    let server = MockServer::start().await;
    // expect(1): wiremock verifies on drop that each endpoint was hit once.
    Mock::given(method("GET"))
        .and(path("/api/v1/user/ratings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("user_ratings")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/full"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("snapshot_page1")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/full"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("snapshot_page2")))
        .expect(1)
        .mount(&server)
        .await;

    let service = service(&server);
    let cred = CodexCredential::ApiKey("test-key".into());
    let first = service.get("user-1", &cred).await.expect("cold build");
    let second = service.get("user-1", &cred).await.expect("cached");
    assert_eq!(second.freshness, Freshness::Fresh);
    assert_eq!(first.as_of, second.as_of, "same underlying build");
    assert!(service.last_warm().is_some());
}

#[tokio::test]
async fn cold_build_401_surfaces_the_distinct_unauthorized_variant() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user/ratings"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = service(&server)
        .get("user-1", &CodexCredential::ApiKey("expired".into()))
        .await
        .expect_err("cold build must fail loud");
    match err {
        SwrError::Load(CodexError::Unauthorized { .. }) => {}
        other => panic!("expected Load(Unauthorized), got {other:?}"),
    }
}

#[tokio::test]
async fn cold_build_backend_failure_fails_loud_never_guesses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user/ratings"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let err = service(&server)
        .get("user-1", &CodexCredential::ApiKey("test-key".into()))
        .await
        .expect_err("cold build must fail loud");
    assert!(
        matches!(err, SwrError::Load(CodexError::Api { .. })),
        "{err:?}"
    );
}
