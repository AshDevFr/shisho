//! `lookup` service tests: one fixture per input class, plus per-leg
//! failure injection.

use migration::{Migrator, MigratorTrait};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, Database, DatabaseConnection};
use shisho_codex::{CodexClient, CodexCredential};
use shisho_config::{CacheConfig, Secret};
use shisho_db::entities::verdict;
use shisho_services::lookup::{LookupError, lookup};
use shisho_services::read_state::ReadState;
use shisho_services::{Services, UserCtx};
use shisho_tsundoku::TsundokuClient;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(dir: &str, name: &str) -> serde_json::Value {
    let path = format!(
        "{}/../../tests/fixtures/{dir}/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

async fn fresh_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.expect("connect");
    Migrator::up(&db, None).await.expect("migrate");
    db
}

fn services(codex: &MockServer, tsundoku: &MockServer, db: DatabaseConnection) -> Services {
    let http = reqwest::Client::new();
    Services::new(
        CodexClient::new(&codex.uri(), http.clone()),
        TsundokuClient::new(&tsundoku.uri(), Secret::new("admin-token"), http),
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

/// Codex: the caller's library + a live rating for the owned Frieren row.
async fn mount_codex(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/v1/user/ratings"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ratings": [] })),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("codex", "candidates_library")),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/lib-a/rating"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "r-1",
            "seriesId": "lib-a",
            "rating": 92,
            "notes": "A favourite."
        })))
        .mount(server)
        .await;
}

/// Tsundoku: Frieren resolves (feed) and has a detail record.
async fn mount_tsundoku_frieren(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "candidates_resolution")),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/201"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "candidates_detail_201")),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn canonical_and_codex_notation_ids_resolve_to_the_merged_view() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex(&codex).await;
    mount_tsundoku_frieren(&tsundoku).await;

    // History includes retracted verdicts (it is a log, not a filter).
    verdict::ActiveModel {
        user_sub: Set("user-1".into()),
        mangabaka_id: Set(Some(112092)),
        provider: Set(None),
        external_id: Set(None),
        session_id: Set("session-9".into()),
        tier: Set("S".into()),
        rationale: Set("matched the profile".into()),
        verified: Set(true),
        retracted: Set(true),
        outcome: Set(None),
        notes: Set(None),
        created_at: Set(chrono::Utc::now().into()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("seed verdict");

    let services = services(&codex, &tsundoku, db);

    for input in ["mal:126287", "plugin:myanimelist:126287"] {
        let view = lookup(&services, &user(), input).await.expect(input);

        assert_eq!(view.title, "Sousou no Frieren");
        assert_eq!(view.mangabaka_id, Some(112092));
        assert!(view.gaps.is_empty(), "no gaps expected: {:?}", view.gaps);

        let t = view.tsundoku.as_ref().expect("tsundoku leg");
        assert_eq!(t.series_id, 201);
        assert!(t.description.as_deref().unwrap_or("").contains("elf mage"));
        assert_eq!(t.volume_coverage.len(), 1);

        let c = view.codex.as_ref().expect("codex leg");
        assert_eq!(c.series_id, "lib-a");
        assert_eq!(c.read_state, ReadState::Untouched);
        assert_eq!(c.rating, Some(92));
        assert_eq!(c.notes.as_deref(), Some("A favourite."));

        assert_eq!(view.verdicts.len(), 1);
        assert!(view.verdicts[0].retracted);

        assert!(view.external_ids.contains(&"mal:126287".to_owned()));
        assert!(view.external_ids.contains(&"mangabaka:112092".to_owned()));
    }
}

#[tokio::test]
async fn fuzzy_title_resolves_through_provider_search() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex(&codex).await;
    mount_tsundoku_frieren(&tsundoku).await;

    Mock::given(method("GET"))
        .and(path("/api/v1/providers/mangabaka/search"))
        .and(query_param("q", "Sousou no Frieren"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "lookup_title_search")),
        )
        .expect(1)
        .mount(&tsundoku)
        .await;

    let view = lookup(
        &services(&codex, &tsundoku, db),
        &user(),
        "Sousou no Frieren",
    )
    .await
    .expect("title lookup");

    // The top Dice hit (mangabaka:112092) leads to the same merged view.
    assert_eq!(view.mangabaka_id, Some(112092));
    assert!(view.tsundoku.is_some());
    assert!(view.codex.is_some(), "owned series includes Codex state");
}

#[tokio::test]
async fn unowned_series_omits_codex_state_cleanly() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex(&codex).await;

    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "acquirable_coverage")),
        )
        .mount(&tsundoku)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/301"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "acquirable_detail_301")),
        )
        .mount(&tsundoku)
        .await;

    let view = lookup(
        &services(&codex, &tsundoku, db),
        &user(),
        "mangadex:zzz-yyy",
    )
    .await
    .expect("unowned lookup");

    assert!(view.codex.is_none(), "not owned: no Codex section");
    assert!(view.gaps.is_empty(), "absence is not a gap");
    assert_eq!(view.tsundoku.as_ref().map(|t| t.series_id), Some(301));
    assert_eq!(view.title, "Top Rated Pick");
}

#[tokio::test]
async fn one_failing_leg_yields_a_partial_view_with_a_named_gap() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex(&codex).await;

    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(ResponseTemplate::new(500).set_body_string("down"))
        .mount(&tsundoku)
        .await;

    let view = lookup(&services(&codex, &tsundoku, db), &user(), "mal:126287")
        .await
        .expect("partial view, not an error");

    assert!(view.tsundoku.is_none());
    assert!(view.codex.is_some(), "the healthy leg still answers");
    assert_eq!(view.gaps.len(), 1);
    assert!(
        view.gaps[0].starts_with("tsundoku:"),
        "gap names the failed source: {:?}",
        view.gaps
    );
}

#[tokio::test]
async fn input_resolving_nowhere_is_a_typed_error() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex(&codex).await;

    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "feed_empty")))
        .mount(&tsundoku)
        .await;

    let err = lookup(&services(&codex, &tsundoku, db), &user(), "kitsu:31337")
        .await
        .expect_err("nothing anywhere");
    assert!(matches!(err, LookupError::Unresolved { .. }), "{err:?}");
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

/// Services over the shared fixture dump (Chainsaw Man, Frieren, …) —
/// the configured backbone the cold-backend matrix needs.
async fn services_with_dump(
    codex_url: &str,
    tsundoku_url: &str,
    db: DatabaseConnection,
) -> Services {
    let dir = std::env::temp_dir().join(format!("shisho-lookup-mb-{}", uuid::Uuid::new_v4()));
    shisho_mangabaka::fixtures::write_dump_fixture(&dir)
        .await
        .expect("fixture dump");
    let cfg = shisho_config::MangabakaConfig {
        api_fallback: false,
        ..Default::default()
    };
    let provider =
        std::sync::Arc::new(shisho_mangabaka::MangabakaProvider::new(&cfg, dir).expect("provider"));
    assert!(provider.load_existing_dump().await, "dump loads");

    let http = reqwest::Client::new();
    Services::new(
        CodexClient::new(codex_url, http.clone()),
        TsundokuClient::new(tsundoku_url, Secret::new("admin-token"), http),
        provider,
        db,
        &CacheConfig::default(),
        false,
    )
}

/// The M7 exit criterion: with Codex and Tsundoku both unreachable, a
/// `mangabaka:` id still resolves to title + description + genres from
/// the dump, with both failures named in `gaps`.
#[tokio::test]
async fn m7_exit_mangabaka_id_resolves_with_both_backends_unreachable() {
    // Mock servers with no mounted routes: reachable HTTP, broken service.
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    let services = services_with_dump(&codex.uri(), &tsundoku.uri(), db).await;

    let view = lookup(&services, &user(), "mangabaka:1677")
        .await
        .expect("the backbone answers alone");

    assert_eq!(view.title, "Chainsaw Man");
    assert_eq!(view.mangabaka_id, Some(1677));
    let m = view.mangabaka.as_ref().expect("backbone leg");
    assert_eq!(m.title, "Chainsaw Man");
    assert_eq!(m.description.as_deref(), Some("Denji has a devil dog."));
    assert_eq!(m.genres, vec!["Action", "Horror"]);
    assert_eq!(m.rating, Some(8.5));
    assert_eq!(m.source, "mangabaka-dump", "provenance is annotated");
    // Foreign IDs from the dump join the identity set.
    assert!(view.external_ids.contains(&"mal:116778".to_owned()));
    assert!(view.external_ids.contains(&"anilist:105778".to_owned()));
    // Both cold backends are named, never silent.
    assert_eq!(view.gaps.len(), 2, "gaps: {:?}", view.gaps);
    assert!(view.gaps.iter().any(|g| g.starts_with("codex:")));
    assert!(view.gaps.iter().any(|g| g.starts_with("tsundoku:")));
    assert!(view.tsundoku.is_none());
    assert!(view.codex.is_none());
}

/// Foreign-ID inputs pivot through the dump's source columns when
/// Tsundoku cannot resolve them.
#[tokio::test]
async fn foreign_id_pivots_through_the_dump_when_tsundoku_cannot_resolve() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex(&codex).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "feed_empty")))
        .mount(&tsundoku)
        .await;

    let services = services_with_dump(&codex.uri(), &tsundoku.uri(), db).await;
    let view = lookup(&services, &user(), "mal:126287")
        .await
        .expect("foreign id pivots");

    let m = view.mangabaka.as_ref().expect("backbone leg");
    assert_eq!(m.mangabaka_id, 112092, "mal:126287 pivots to Frieren");
    assert!(
        m.description.as_deref().unwrap_or("").contains("elf mage"),
        "dump description rides along"
    );
    assert_eq!(view.mangabaka_id, Some(112092));
    assert!(view.tsundoku.is_none(), "cleanly absent from the catalog");
    assert!(view.codex.is_some(), "the owned series still shows");
    assert!(
        view.gaps.is_empty(),
        "absence is not a gap: {:?}",
        view.gaps
    );
}

/// Title inputs fall back to the backbone's FTS when Tsundoku is not
/// configured (Tier 0 without Tsundoku still resolves titles locally).
#[tokio::test]
async fn title_search_falls_back_to_the_dump_fts_without_tsundoku() {
    let codex = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex(&codex).await;

    // Tsundoku unconfigured: empty base URL.
    let services = services_with_dump(&codex.uri(), "", db).await;
    let view = lookup(&services, &user(), "Chainsaw Man")
        .await
        .expect("dump FTS resolves the title");

    assert_eq!(view.mangabaka_id, Some(1677));
    let m = view.mangabaka.as_ref().expect("backbone leg");
    assert_eq!(m.title, "Chainsaw Man");
    assert_eq!(m.source, "mangabaka-dump");
}
