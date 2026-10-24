//! `get_candidates` service tests: fixture clients (wiremock) + in-memory
//! SQLite seeded with verdicts. Covers both pools, the ownership anti-join
//! invariant, and verdict subtraction semantics.

use migration::{Migrator, MigratorTrait};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, Database, DatabaseConnection};
use shisho_codex::{CodexClient, CodexCredential};
use shisho_config::{CacheConfig, Secret};
use shisho_db::entities::verdict;
use shisho_services::candidates::{CandidatesError, Enrichment, Filters, Pool, get_candidates};
use shisho_services::{Services, UserCtx};
use shisho_tsundoku::TsundokuClient;
use wiremock::matchers::{method, path, query_param_is_missing};
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

/// Seed one verdict row.
async fn seed_verdict(
    db: &DatabaseConnection,
    mangabaka_id: Option<i64>,
    pair: Option<(&str, &str)>,
    retracted: bool,
) {
    seed_row(db, mangabaka_id, pair, retracted, None, None).await;
}

/// Seed one verdict-log row of any kind (verdict, retraction, feedback).
async fn seed_row(
    db: &DatabaseConnection,
    mangabaka_id: Option<i64>,
    pair: Option<(&str, &str)>,
    retracted: bool,
    retraction_reason: Option<&str>,
    outcome: Option<&str>,
) {
    verdict::ActiveModel {
        user_sub: Set("user-1".into()),
        mangabaka_id: Set(mangabaka_id),
        provider: Set(pair.map(|(p, _)| p.to_owned())),
        external_id: Set(pair.map(|(_, id)| id.to_owned())),
        session_id: Set("session-1".into()),
        tier: Set("A".into()),
        rationale: Set("seeded by test".into()),
        verified: Set(true),
        retracted: Set(retracted),
        retraction_reason: Set(retraction_reason.map(str::to_owned)),
        outcome: Set(outcome.map(str::to_owned)),
        notes: Set(None),
        created_at: Set(chrono::Utc::now().into()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed verdict row");
}

/// Mount the caller's library (snapshot source) on the Codex mock.
async fn mount_codex_library(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/v1/user/ratings"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ratings": [] })),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/full"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("codex", "candidates_library")),
        )
        .mount(server)
        .await;
    // The acquirable pool resolves ownership from the slim external-index; the
    // library fixture doubles as it (the entry carries `id` + `externalIds`).
    Mock::given(method("GET"))
        .and(path("/api/v1/series/external-index"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("codex", "candidates_library")),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn library_pool_returns_exactly_the_untouched_non_verdicted_set() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex_library(&codex).await;

    // Resolution: only Frieren exists in Tsundoku.
    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "candidates_resolution")),
        )
        .mount(&tsundoku)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/201"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "candidates_detail_201")),
        )
        .mount(&tsundoku)
        .await;

    // lib-c excluded by a (provider, external_id) verdict; lib-f by a
    // MangaBaka-ID verdict; lib-d has a retracted-only history -> included.
    seed_verdict(&db, None, Some(("mangaupdates", "777")), false).await;
    seed_verdict(&db, Some(333), None, false).await;
    seed_verdict(&db, None, Some(("kitsu", "555")), true).await;

    let set = get_candidates(
        &services(&codex, &tsundoku, db),
        &user(),
        Pool::Library,
        &Filters::default(),
    )
    .await
    .expect("library pool");

    let titles: Vec<&str> = set.candidates.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["Sousou no Frieren", "Obscure Title", "Retracted Only"],
        "exactly the untouched, non-verdicted set"
    );
    assert!(!set.truncated);
    assert!(set.as_of.library.is_some());
    assert!(set.as_of.tsundoku.is_some());

    // Frieren resolved in Tsundoku: enriched.
    let frieren = &set.candidates[0];
    assert_eq!(frieren.enrichment, Enrichment::Full);
    assert_eq!(frieren.tsundoku_series_id, Some(201));
    assert_eq!(frieren.mangabaka_id, Some(112092));
    assert_eq!(frieren.rating, Some(8.9));
    assert_eq!(
        frieren.description.as_deref(),
        Some(
            "The adventure is over but life goes on for an elf mage just beginning to learn what living is all about."
        )
    );
    assert!(
        frieren.volume_coverage.is_empty(),
        "coverage is an acquisition concern; owned series do not ship it"
    );
    // Genres are the Codex ∪ Tsundoku union.
    assert_eq!(frieren.genres, vec!["Fantasy", "Adventure"]);

    // Absent from Tsundoku: kept, flagged honestly, never dropped.
    let obscure = &set.candidates[1];
    assert_eq!(obscure.enrichment, Enrichment::None);
    assert_eq!(obscure.tsundoku_series_id, None);
    assert_eq!(
        obscure.description.as_deref(),
        Some("A series Tsundoku has never heard of."),
        "Codex summary survives as the description"
    );
}

#[tokio::test]
async fn exclusion_follows_the_effective_state_fold() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex_library(&codex).await;
    // Tsundoku down: enrichment degrades, exclusion semantics are local.
    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(ResponseTemplate::new(500).set_body_string("down"))
        .mount(&tsundoku)
        .await;

    // Frieren: a recorded outcome settles it (the user read it).
    seed_row(&db, Some(112092), None, false, None, Some("loved")).await;
    // Verdicted By Pair: recommended, then withdrawn — back to candidacy.
    seed_row(&db, None, Some(("mangaupdates", "777")), false, None, None).await;
    seed_row(
        &db,
        None,
        Some(("mangaupdates", "777")),
        true,
        Some("withdrawn"),
        None,
    )
    .await;
    // Retracted Only: failed verification — never re-litigated.
    seed_row(
        &db,
        None,
        Some(("kitsu", "555")),
        true,
        Some("failed_verification"),
        None,
    )
    .await;

    let set = get_candidates(
        &services(&codex, &tsundoku, db),
        &user(),
        Pool::Library,
        &Filters::default(),
    )
    .await
    .expect("library pool");

    let titles: Vec<&str> = set.candidates.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(
        titles,
        vec![
            "Obscure Title",
            "Verdicted By Pair",
            "Verdicted By MangaBaka Id"
        ],
        "outcome and failed verification exclude; withdrawal restores"
    );
}

#[tokio::test]
async fn library_pool_survives_tsundoku_being_down() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex_library(&codex).await;

    // Tsundoku errors on everything: enrichment degrades, the pool lives.
    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(ResponseTemplate::new(500).set_body_string("down"))
        .mount(&tsundoku)
        .await;

    let set = get_candidates(
        &services(&codex, &tsundoku, db),
        &user(),
        Pool::Library,
        &Filters::default(),
    )
    .await
    .expect("library pool must not fail on Tsundoku");

    // No verdicts seeded here: all five untouched series qualify.
    assert_eq!(set.candidates.len(), 5);
    assert!(
        set.candidates
            .iter()
            .all(|c| c.enrichment == Enrichment::None)
    );
    assert!(set.as_of.tsundoku.is_none());
}

/// With Tsundoku down but the dump loaded, Codex-only candidates fall back
/// to the MangaBaka backbone: rating/genres/description gaps fill from the
/// dump, the source is annotated `mangabaka`, and a series the dump does
/// not know stays honestly `none`. Tsundoku-sourced fields are preferred
/// when present (the Codex summary is never overwritten).
#[tokio::test]
async fn candidates_fall_back_to_the_dump_where_tsundoku_lacks_the_series() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex_library(&codex).await;

    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(ResponseTemplate::new(500).set_body_string("down"))
        .mount(&tsundoku)
        .await;

    let http = reqwest::Client::new();
    let services = Services::new(
        CodexClient::new(&codex.uri(), http.clone()),
        TsundokuClient::new(&tsundoku.uri(), Secret::new("admin-token"), http),
        mangabaka_with_dump().await,
        db,
        &CacheConfig::default(),
        false,
    );

    let set = get_candidates(&services, &user(), Pool::Library, &Filters::default())
        .await
        .expect("library pool");
    let by_title = |t: &str| {
        set.candidates
            .iter()
            .find(|c| c.title == t)
            .unwrap_or_else(|| panic!("missing candidate {t}"))
    };

    // Obscure Title (anilist:99999) resolves via the dump's source column.
    let obscure = by_title("Obscure Title");
    assert_eq!(obscure.enrichment, Enrichment::Mangabaka);
    assert_eq!(obscure.mangabaka_id, Some(424242));
    assert_eq!(obscure.rating, Some(7.6), "dump rating fills the gap");
    assert_eq!(
        obscure.description.as_deref(),
        Some("A series Tsundoku has never heard of."),
        "the Codex summary is never overwritten"
    );
    assert!(
        obscure
            .external_ids
            .contains(&"mangabaka:424242".to_owned()),
        "the resolved native id joins the identity set"
    );

    // lib-f has no Codex summary: the dump's description ships instead of
    // an empty candidate.
    let backfilled = by_title("Verdicted By MangaBaka Id");
    assert_eq!(backfilled.enrichment, Enrichment::Mangabaka);
    assert_eq!(
        backfilled.description.as_deref(),
        Some("Dump description for the 333 row."),
        "never ship an empty description when the dump has one"
    );
    assert_eq!(backfilled.genres, vec!["Action"], "dump genres fill in");

    // The dump has never heard of mangaupdates:777: honestly none.
    let unknown = by_title("Verdicted By Pair");
    assert_eq!(unknown.enrichment, Enrichment::None);
    assert!(unknown.description.is_none());
}

#[tokio::test]
async fn acquirable_pool_anti_join_beats_the_lying_owned_flag() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex_library(&codex).await;

    // The default deployment does not opt into the owned pre-filter.
    Mock::given(method("GET"))
        .and(path("/api/v1/series"))
        .and(query_param_is_missing("owned"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "acquirable_page")),
        )
        .mount(&tsundoku)
        .await;
    for id in [301, 302, 303] {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/series/{id}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(fixture("tsundoku", &format!("acquirable_detail_{id}"))),
            )
            .mount(&tsundoku)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "acquirable_coverage")),
        )
        .mount(&tsundoku)
        .await;

    // 303 carries a non-retracted verdict.
    seed_verdict(&db, Some(9999), None, false).await;

    let set = get_candidates(
        &services(&codex, &tsundoku, db),
        &user(),
        Pool::Acquirable,
        &Filters::default(),
    )
    .await
    .expect("acquirable pool");

    let titles: Vec<&str> = set.candidates.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["Top Rated Pick"],
        "302 is owned by the caller (mal:126287, sweep lied), 303 is verdicted"
    );

    let pick = &set.candidates[0];
    assert_eq!(pick.tsundoku_series_id, Some(301));
    assert_eq!(pick.mangabaka_id, Some(50001));
    assert_eq!(pick.volume_coverage.len(), 1, "coverage spans attached");
    assert_eq!(pick.chapter_coverage.len(), 1);

    // Codex-overlay/ownership fields never appear anywhere in the DTO.
    let json = serde_json::to_value(&set).expect("serialize");
    let rendered = json.to_string();
    for forbidden in ["\"owned\"", "\"codex\":", "\"seriesUuid\"", "\"deepLink\""] {
        assert!(
            !rendered.contains(forbidden),
            "overlay field {forbidden} leaked into the response: {rendered}"
        );
    }
}

#[tokio::test]
async fn acquirable_pool_names_itself_in_the_degraded_error() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex_library(&codex).await;

    Mock::given(method("GET"))
        .and(path("/api/v1/series"))
        .respond_with(ResponseTemplate::new(500).set_body_string("down"))
        .mount(&tsundoku)
        .await;

    let err = get_candidates(
        &services(&codex, &tsundoku, db),
        &user(),
        Pool::Acquirable,
        &Filters::default(),
    )
    .await
    .expect_err("acquirable without Tsundoku must be a typed error, not empty");

    match err {
        CandidatesError::TsundokuDegraded { pool, .. } => assert_eq!(pool, "acquirable"),
        other => panic!("expected TsundokuDegraded, got {other:?}"),
    }
}

#[tokio::test]
async fn owned_prefilter_opt_in_adds_the_bandwidth_filter() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex_library(&codex).await;

    // The mock only matches when owned=false is present.
    Mock::given(method("GET"))
        .and(path("/api/v1/series"))
        .and(wiremock::matchers::query_param("owned", "false"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "acquirable_page")),
        )
        .expect(1)
        .mount(&tsundoku)
        .await;
    for id in [301, 302, 303] {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/series/{id}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(fixture("tsundoku", &format!("acquirable_detail_{id}"))),
            )
            .mount(&tsundoku)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "acquirable_coverage")),
        )
        .mount(&tsundoku)
        .await;

    let http = reqwest::Client::new();
    let services = Services::new(
        CodexClient::new(&codex.uri(), http.clone()),
        TsundokuClient::new(&tsundoku.uri(), Secret::new("admin-token"), http),
        mangabaka_inert(),
        db,
        &CacheConfig::default(),
        true,
    );

    let set = get_candidates(&services, &user(), Pool::Acquirable, &Filters::default())
        .await
        .expect("acquirable with prefilter");
    // The anti-join still applies on top of the pre-filter (no verdicts
    // seeded here, so 303 stays; owned 302 is gone).
    let titles: Vec<&str> = set.candidates.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, vec!["Top Rated Pick", "Already Verdicted"]);
}

#[tokio::test]
async fn filters_narrow_the_library_pool() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex_library(&codex).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "candidates_resolution")),
        )
        .mount(&tsundoku)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/201"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "candidates_detail_201")),
        )
        .mount(&tsundoku)
        .await;

    let set = get_candidates(
        &services(&codex, &tsundoku, db),
        &user(),
        Pool::Library,
        &Filters {
            genres: vec!["mystery".into()],
            ..Default::default()
        },
    )
    .await
    .expect("filtered library pool");

    let titles: Vec<&str> = set.candidates.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["Obscure Title"],
        "case-insensitive genre match"
    );
}

/// A provider over the shared fixture dump (Chainsaw Man, Frieren,
/// Obscure Title, the 333 row — see `shisho_mangabaka::fixtures`).
async fn mangabaka_with_dump() -> std::sync::Arc<shisho_mangabaka::MangabakaProvider> {
    let dir = std::env::temp_dir().join(format!("shisho-cand-mb-{}", uuid::Uuid::new_v4()));
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
    provider
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
async fn acquirable_details_are_fetched_concurrently() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex_library(&codex).await;

    Mock::given(method("GET"))
        .and(path("/api/v1/series"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "acquirable_page")),
        )
        .mount(&tsundoku)
        .await;
    // Each detail takes 300 ms: three in sequence would take 900 ms.
    for id in [301, 302, 303] {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/series/{id}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(fixture("tsundoku", &format!("acquirable_detail_{id}")))
                    .set_delay(std::time::Duration::from_millis(300)),
            )
            .mount(&tsundoku)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "acquirable_coverage")),
        )
        .mount(&tsundoku)
        .await;

    let services = services(&codex, &tsundoku, db);
    // Warm the ownership index so only the detail fan-out is timed.
    services
        .snapshots
        .external_index(&user().user_sub, &user().credential)
        .await
        .expect("index");

    let started = std::time::Instant::now();
    let set = get_candidates(&services, &user(), Pool::Acquirable, &Filters::default())
        .await
        .expect("acquirable pool");
    let elapsed = started.elapsed();

    let titles: Vec<&str> = set.candidates.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles[0], "Top Rated Pick", "catalog order is preserved");
    assert!(
        elapsed < std::time::Duration::from_millis(700),
        "details must be fetched concurrently, took {elapsed:?}"
    );
}

#[tokio::test]
async fn acquirable_pool_walks_past_the_first_catalog_page() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let db = fresh_db().await;
    mount_codex_library(&codex).await;

    let item = |id: i64, title: &str| {
        let mut row = fixture("tsundoku", "acquirable_page")["items"][0].clone();
        row["id"] = id.into();
        row["canonicalTitle"] = title.into();
        row
    };
    let detail = |id: i64, title: &str, mangabaka: &str| {
        let mut row = fixture("tsundoku", "acquirable_detail_301");
        row["id"] = id.into();
        row["canonicalTitle"] = title.into();
        row["externalIds"] = serde_json::json!([
            { "provider": "mangabaka", "externalId": mangabaka, "fetchedAt": 1765200000 }
        ]);
        row
    };
    // Two catalog pages: page 0 carries 301 + 302 (owned), page 1 carries 304.
    let page0 = serde_json::json!({
        "items": [item(301, "Top Rated Pick"), item(302, "Owned But The Sweep Lies")],
        "page": 0, "pageSize": 50, "total": 51,
    });
    let page1 = serde_json::json!({
        "items": [item(304, "Second Page Pick")],
        "page": 1, "pageSize": 50, "total": 51,
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/series"))
        .and(wiremock::matchers::query_param("page", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page0))
        .mount(&tsundoku)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series"))
        .and(wiremock::matchers::query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page1))
        .mount(&tsundoku)
        .await;
    for (id, body) in [
        (301, detail(301, "Top Rated Pick", "50001")),
        (302, fixture("tsundoku", "acquirable_detail_302")),
        (304, detail(304, "Second Page Pick", "50004")),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/series/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&tsundoku)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "acquirable_coverage")),
        )
        .mount(&tsundoku)
        .await;

    let set = get_candidates(
        &services(&codex, &tsundoku, db),
        &user(),
        Pool::Acquirable,
        &Filters::default(),
    )
    .await
    .expect("acquirable pool");

    let titles: Vec<&str> = set.candidates.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, vec!["Top Rated Pick", "Second Page Pick"]);
    assert!(!set.truncated, "the catalog was exhausted");
}

/// One Codex `/series/full` row.
fn codex_row(id: &str, title: &str, genres: &[&str], books: i64, unread: i64) -> serde_json::Value {
    serde_json::json!({
        "id": id, "libraryId": "lib", "libraryName": "Manga",
        "bookCount": books, "unreadCount": unread,
        "metadata": { "title": title, "summary": null, "status": "ended", "year": 2020,
                      "publisher": null, "totalVolumeCount": books, "totalChapterCount": null },
        "genres": genres.iter().enumerate()
            .map(|(i, g)| serde_json::json!({ "id": format!("g-{i}"), "name": g }))
            .collect::<Vec<_>>(),
        "tags": [],
        "externalIds": [{ "id": format!("x-{id}"), "seriesId": id, "source": "plugin:mangabaka",
                          "externalId": id.trim_start_matches(|c: char| !c.is_ascii_digit()),
                          "externalUrl": null }],
    })
}

/// A Fantasy reader: two Fantasy favourites, one disliked Horror read, and a
/// backlog that lists Horror before Fantasy.
async fn mount_fantasy_reader(codex: &MockServer, tsundoku: &MockServer, backlog: usize) {
    let mut rows = vec![
        codex_row("s1", "Loved Fantasy One", &["Fantasy", "Adventure"], 10, 0),
        codex_row("s2", "Loved Fantasy Two", &["Fantasy"], 8, 0),
        codex_row("s3", "Disliked Horror", &["Horror"], 5, 0),
        codex_row("s4", "Unread Horror", &["Horror"], 6, 6),
        codex_row("s5", "Unread Fantasy", &["Fantasy"], 7, 7),
    ];
    for i in 0..backlog {
        let id = format!("s{}", 100 + i);
        rows.push(codex_row(
            &id,
            &format!("Unread Romance {i}"),
            &["Romance"],
            4,
            4,
        ));
    }
    let total = rows.len();
    Mock::given(method("GET"))
        .and(path("/api/v1/series/full"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": rows, "page": 1, "pageSize": 100, "total": total, "totalPages": 1
        })))
        .mount(codex)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user/ratings"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ratings": [
                { "id": "r1", "seriesId": "s1", "rating": 92, "notes": null },
                { "id": "r2", "seriesId": "s2", "rating": 88, "notes": null },
                { "id": "r3", "seriesId": "s3", "rating": 20, "notes": null },
            ]})),
        )
        .mount(codex)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "feed_empty")))
        .mount(tsundoku)
        .await;
}

#[tokio::test]
async fn library_pool_ranks_by_affinity_with_anchors() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    mount_fantasy_reader(&codex, &tsundoku, 0).await;

    let set = get_candidates(
        &services(&codex, &tsundoku, fresh_db().await),
        &user(),
        Pool::Library,
        &Filters::default(),
    )
    .await
    .expect("library pool");

    let titles: Vec<&str> = set.candidates.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, vec!["Unread Fantasy", "Unread Horror"]);
    let top = &set.candidates[0];
    assert!(top.score.unwrap() > set.candidates[1].score.unwrap());
    let anchor = top.because.first().expect("an anchor");
    assert!(anchor.title.starts_with("Loved Fantasy"));
    assert!(anchor.your_rating.unwrap() >= 88);
    assert_eq!(anchor.shared, vec!["Fantasy"]);
    assert!(
        set.candidates[1].because.is_empty(),
        "nothing the reader liked shares Horror"
    );
}

#[tokio::test]
async fn library_pool_pages_with_offset() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    mount_fantasy_reader(&codex, &tsundoku, 20).await;
    let services = services(&codex, &tsundoku, fresh_db().await);

    let first = get_candidates(
        &services,
        &user(),
        Pool::Library,
        &Filters {
            limit: Some(5),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let second = get_candidates(
        &services,
        &user(),
        Pool::Library,
        &Filters {
            limit: Some(5),
            offset: Some(5),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(first.candidates[0].title, "Unread Fantasy");
    assert!(first.truncated && second.truncated);
    let first_titles: Vec<&str> = first.candidates.iter().map(|c| c.title.as_str()).collect();
    for c in &second.candidates {
        assert!(
            !first_titles.contains(&c.title.as_str()),
            "pages overlap on {}",
            c.title
        );
    }
    assert_eq!(second.candidates.len(), 5);
}

#[tokio::test]
async fn payload_of_forty_bloated_candidates_stays_compact() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    let long = "A sentence about the plot that goes on for a while. ".repeat(30);
    let tags: Vec<serde_json::Value> = (0..30)
        .map(|i| serde_json::json!({ "id": format!("t-{i}"), "name": format!("Tag Number {i}") }))
        .collect();
    let rows: Vec<serde_json::Value> = (0..40)
        .map(|i| {
            let id = format!("s{}", 500 + i);
            let mut row = codex_row(&id, &format!("Bloated Series {i}"), &["Fantasy"], 5, 5);
            row["metadata"]["summary"] = long.clone().into();
            row["tags"] = tags.clone().into();
            row["externalIds"] = serde_json::json!([
                { "id": "a", "seriesId": id, "source": "plugin:mangabaka", "externalId": format!("{}", 500 + i), "externalUrl": null },
                { "id": "b", "seriesId": id, "source": "plugin:myanimelist", "externalId": format!("{}", 900 + i), "externalUrl": null },
                { "id": "c", "seriesId": id, "source": "plugin:anilist", "externalId": format!("{}", 700 + i), "externalUrl": null },
                { "id": "d", "seriesId": id, "source": "plugin:mangadex", "externalId": format!("uuid-{i}"), "externalUrl": null },
            ]);
            row
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/v1/series/full"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": rows, "page": 1, "pageSize": 100, "total": 40, "totalPages": 1
        })))
        .mount(&codex)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user/ratings"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ratings": [] })),
        )
        .mount(&codex)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "feed_empty")))
        .mount(&tsundoku)
        .await;

    let set = get_candidates(
        &services(&codex, &tsundoku, fresh_db().await),
        &user(),
        Pool::Library,
        &Filters::default(),
    )
    .await
    .unwrap();

    assert_eq!(set.candidates.len(), 40);
    let bytes = serde_json::to_string(&set).unwrap().len();
    assert!(bytes < 24 * 1024, "payload is {bytes} bytes");
    let first = &set.candidates[0];
    assert!(first.tags.len() <= 8);
    assert!(first.external_ids.len() <= 2);
    assert_eq!(first.external_ids[0], "mangabaka:500");
    assert!(first.description.as_deref().unwrap().ends_with('…'));
}

/// An acquirable catalog of two unowned picks whose MangaBaka rows are
/// pornographic (77701) and erotica (77702).
async fn mount_explicit_catalog(codex: &MockServer, tsundoku: &MockServer) {
    mount_codex_library(codex).await;
    let item = |id: i64, title: &str| {
        let mut row = fixture("tsundoku", "acquirable_page")["items"][0].clone();
        row["id"] = id.into();
        row["canonicalTitle"] = title.into();
        row
    };
    Mock::given(method("GET"))
        .and(path("/api/v1/series"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [item(401, "Explicit Pick"), item(402, "Erotica Pick")],
            "page": 0, "pageSize": 50, "total": 2,
        })))
        .mount(tsundoku)
        .await;
    for (id, mangabaka, title) in [
        (401, "77701", "Explicit Pick"),
        (402, "77702", "Erotica Pick"),
    ] {
        let mut detail = fixture("tsundoku", "acquirable_detail_301");
        detail["id"] = id.into();
        detail["canonicalTitle"] = title.into();
        detail["externalIds"] = serde_json::json!([
            { "provider": "mangabaka", "externalId": mangabaka, "fetchedAt": 1765200000 }
        ]);
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/series/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(detail))
            .mount(tsundoku)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/v1/series/feed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("tsundoku", "feed_empty")))
        .mount(tsundoku)
        .await;
}

#[tokio::test]
async fn content_rating_gate_drops_pornographic_by_default() {
    let codex = MockServer::start().await;
    let tsundoku = MockServer::start().await;
    mount_explicit_catalog(&codex, &tsundoku).await;
    let http = reqwest::Client::new();
    let services = Services::new(
        CodexClient::new(&codex.uri(), http.clone()),
        TsundokuClient::new(&tsundoku.uri(), Secret::new("admin-token"), http),
        mangabaka_with_dump().await,
        fresh_db().await,
        &CacheConfig::default(),
        false,
    );

    let default = get_candidates(&services, &user(), Pool::Acquirable, &Filters::default())
        .await
        .unwrap();
    let titles: Vec<&str> = default
        .candidates
        .iter()
        .map(|c| c.title.as_str())
        .collect();
    assert_eq!(
        titles,
        vec!["Erotica Pick"],
        "erotica passes, pornographic does not"
    );
    assert_eq!(
        default.candidates[0].content_rating.as_deref(),
        Some("erotica")
    );
    assert_eq!(default.candidates[0].authors, vec!["Someone Else"]);

    let opted_in = get_candidates(
        &services,
        &user(),
        Pool::Acquirable,
        &Filters {
            allow_explicit: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(opted_in.candidates.len(), 2);
}
