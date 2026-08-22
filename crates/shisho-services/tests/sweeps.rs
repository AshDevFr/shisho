//! Sweep tests: lazy reconciliation on snapshot build, the in-process
//! sweeps over warm snapshots, and the scheduler firing on a short
//! interval. Codex is replayed from fixtures through wiremock; the
//! database is in-memory SQLite.

use std::time::Duration;

use migration::{Migrator, MigratorTrait};
use sea_orm::{Database, DatabaseConnection};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use shisho_codex::{CodexClient, CodexCredential};
use shisho_config::{CacheConfig, Secret};
use shisho_db::repos::reconciliation;
use shisho_db::repos::verdicts::{self, NewVerdict};
use shisho_ids::ExternalId;
use shisho_services::{Services, sweeps};
use shisho_tsundoku::TsundokuClient;

fn fixture(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/../../tests/fixtures/codex/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
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

async fn fresh_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.expect("connect");
    Migrator::up(&db, None).await.expect("migrate");
    db
}

fn services(codex_base: &str, db: DatabaseConnection) -> Services {
    Services::new(
        CodexClient::new(codex_base, reqwest::Client::new()),
        TsundokuClient::new("", Secret::new(""), reqwest::Client::new()),
        mangabaka_inert(),
        db,
        &CacheConfig::default(),
        false,
    )
}

/// `anilist:86952` is the fixture series with every chapter read.
async fn seed_completed_series_verdict(db: &DatabaseConnection, user_sub: &str) {
    verdicts::insert(
        db,
        user_sub,
        NewVerdict {
            external_id: Some(ExternalId::from_tsundoku("anilist:86952").unwrap()),
            session_id: "s1".into(),
            tier: "S".into(),
            rationale: "fits the profile".into(),
            verified: true,
            retracted: false,
            retraction_reason: None,
            outcome: None,
            notes: None,
        },
    )
    .await
    .expect("insert verdict");
}

async fn wait_for_pending(db: &DatabaseConnection, user_sub: &str, expected: u64) {
    for _ in 0..100 {
        let count = reconciliation::pending_count(db, user_sub)
            .await
            .expect("pending count");
        if count == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("pending flags never reached {expected} for {user_sub}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_build_lazily_flags_started_series() {
    let server = MockServer::start().await;
    mount_happy_path(&server).await;
    let db = fresh_db().await;
    seed_completed_series_verdict(&db, "user-1").await;

    let services = services(&server.uri(), db.clone());
    services
        .snapshots
        .get("user-1", &CodexCredential::ApiKey("test-key".into()))
        .await
        .expect("snapshot build");

    // The build hook reconciles in a spawned task: the verdict for the
    // fully-read fixture series gets flagged without any sweep running.
    wait_for_pending(&db, "user-1", 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_sweep_detects_and_then_resolves() {
    let server = MockServer::start().await;
    mount_happy_path(&server).await;
    let db = fresh_db().await;
    seed_completed_series_verdict(&db, "user-1").await;

    let services = services(&server.uri(), db.clone());
    services
        .snapshots
        .get("user-1", &CodexCredential::ApiKey("test-key".into()))
        .await
        .expect("snapshot build");

    let report = sweeps::run_reconcile_sweep(&services).await.expect("sweep");
    assert_eq!(report.users, 1);
    assert_eq!(report.with_snapshot, 1);
    // Lazy hook and sweep may race for the same flag; the lifecycle
    // guarantee is that it exists exactly once either way.
    wait_for_pending(&db, "user-1", 1).await;
    assert!(services.sweeps.reconcile_last_run().is_some());

    // An outcome settles the series: the next sweep resolves the flag.
    verdicts::insert(
        &db,
        "user-1",
        NewVerdict {
            external_id: Some(ExternalId::from_tsundoku("anilist:86952").unwrap()),
            session_id: "feedback".into(),
            tier: "feedback".into(),
            rationale: "user feedback".into(),
            verified: false,
            retracted: false,
            retraction_reason: None,
            outcome: Some("loved".into()),
            notes: None,
        },
    )
    .await
    .expect("insert outcome");

    let report = sweeps::run_reconcile_sweep(&services).await.expect("sweep");
    assert_eq!(report.resolved, 1);
    assert_eq!(report.flagged, 0);
    assert_eq!(
        reconciliation::pending_count(&db, "user-1").await.unwrap(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unresolvable_sweep_records_tallies_and_is_idempotent() {
    let server = MockServer::start().await;
    mount_happy_path(&server).await;
    let db = fresh_db().await;
    seed_completed_series_verdict(&db, "user-1").await;

    let services = services(&server.uri(), db.clone());

    // Before any snapshot exists: the user is known but unscanned.
    let cold = sweeps::run_unresolvable_sweep(&services)
        .await
        .expect("cold sweep");
    assert_eq!(cold.users, 1);
    assert_eq!(cold.with_snapshot, 0);
    assert!(services.sweeps.unresolvable_last_run().is_some());
    assert!(services.sweeps.unresolvable_for("user-1").is_none());

    services
        .snapshots
        .get("user-1", &CodexCredential::ApiKey("test-key".into()))
        .await
        .expect("snapshot build");

    let warm = sweeps::run_unresolvable_sweep(&services)
        .await
        .expect("warm sweep");
    assert_eq!(warm.with_snapshot, 1);
    assert_eq!(warm.unresolvable, 1, "the fixture has one unresolvable");
    let entry = services
        .sweeps
        .unresolvable_for("user-1")
        .expect("tally recorded");
    assert_eq!(entry.count, 1);

    // Re-run: same tally, fresher timestamp.
    let again = sweeps::run_unresolvable_sweep(&services)
        .await
        .expect("re-run");
    assert_eq!(again.unresolvable, 1);
    let later = services.sweeps.unresolvable_for("user-1").expect("tally");
    assert!(later.swept_at >= entry.swept_at);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_fires_jobs_on_a_short_interval() {
    let db = fresh_db().await;
    let services = services("", db);

    let _scheduler =
        sweeps::spawn_scheduler(services.clone(), "*/1 * * * * *", "*/1 * * * * *", None)
            .await
            .expect("scheduler");

    for _ in 0..60 {
        if services.sweeps.reconcile_last_run().is_some()
            && services.sweeps.unresolvable_last_run().is_some()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("scheduled sweeps never fired");
}

/// The scheduled MangaBaka refresh fires on its cron; a failed refresh
/// (unreachable dump URL) records a visible failed attempt and leaves the
/// provider serving whatever it had (here: nothing — API-only persists).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_fires_the_mangabaka_refresh() {
    let db = fresh_db().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = shisho_config::MangabakaConfig {
        api_fallback: false,
        // Connection-refused fast: the fire fails quickly and records it.
        offline_dump_url: Some("http://127.0.0.1:1/series.sqlite.tar.gz".into()),
        ..Default::default()
    };
    let provider = std::sync::Arc::new(
        shisho_mangabaka::MangabakaProvider::new(&cfg, dir.path().to_path_buf()).expect("provider"),
    );

    let http = reqwest::Client::new();
    let services = Services::new(
        CodexClient::new("", http.clone()),
        TsundokuClient::new("", Secret::new(""), http),
        std::sync::Arc::clone(&provider),
        db,
        &CacheConfig::default(),
        false,
    );

    let _scheduler = sweeps::spawn_scheduler(
        services,
        "0 0 6 * * Mon",
        "0 30 5 * * *",
        Some("*/1 * * * * *"),
    )
    .await
    .expect("scheduler");

    for _ in 0..100 {
        if let Some(attempt) = provider.status().await.last_refresh {
            assert!(attempt.outcome.is_err(), "unreachable URL must fail");
            assert!(!provider.offline_loaded().await, "nothing was promoted");
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("scheduled MangaBaka refresh never fired");
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
