//! Outcomes inferred from Codex book progress: a recommended series whose
//! read stalled gets `dnf`, and resuming withdraws it. Codex is wiremock;
//! the verdict log is in-memory SQLite.

use std::sync::Arc;

use chrono::{Duration, Utc};
use migration::{Migrator, MigratorTrait};
use sea_orm::{Database, DatabaseConnection};
use shisho_codex::{CodexClient, CodexCredential};
use shisho_config::{CacheConfig, Secret};
use shisho_db::Dek;
use shisho_db::repos::users;
use shisho_db::repos::verdicts::{self, NewVerdict, REOPENED};
use shisho_ids::ExternalId;
use shisho_services::codex_identity::CodexIdentity;
use shisho_services::library_snapshot::{LibrarySnapshot, SnapshotSeries};
use shisho_services::read_state::ReadState;
use shisho_services::reconciliation::{AUTO_FEEDBACK_SESSION, CodexAccess, reconcile_user};
use shisho_services::sweeps::{self, StoredKeys};
use shisho_services::{Services, UserCtx};
use shisho_tsundoku::TsundokuClient;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn fresh_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.expect("connect");
    Migrator::up(&db, None).await.expect("migrate");
    db
}

fn credential() -> CodexCredential {
    CodexCredential::ApiKey("test-key".into())
}

async fn recommend(db: &DatabaseConnection) {
    verdicts::insert(
        db,
        "user-1",
        NewVerdict {
            external_id: Some(ExternalId::from_tsundoku("mangabaka:1").unwrap()),
            session_id: "s1".into(),
            tier: "A".into(),
            rationale: "fits the profile".into(),
            verified: true,
            retracted: false,
            retraction_reason: None,
            outcome: None,
            notes: None,
        },
    )
    .await
    .unwrap();
}

/// The recommended series as the caller's library holds it.
fn owned(read_state: ReadState, unread: i64) -> LibrarySnapshot {
    LibrarySnapshot::from_series(vec![SnapshotSeries {
        id: "s-1".into(),
        library_id: "lib".into(),
        library_name: "Manga".into(),
        title: "Recommended".into(),
        summary: None,
        status: None,
        year: None,
        genres: Vec::new(),
        tags: Vec::new(),
        authors: Vec::new(),
        book_count: 10,
        unread_count: Some(unread),
        read_state,
        external_ids: vec![ExternalId::from_tsundoku("mangabaka:1").unwrap()],
        rating: None,
        rating_notes: None,
        rating_updated_at: None,
    }])
}

/// Codex answering the books call with progress last moved `days_ago`.
async fn books_last_read(server: &MockServer, days_ago: i64) {
    let at = (Utc::now() - Duration::days(days_ago)).to_rfc3339();
    Mock::given(method("GET"))
        .and(path("/api/v1/series/s-1/books"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "id": "b-1", "readProgress": { "completed": true, "updatedAt": at } },
            { "id": "b-2" }
        ])))
        .mount(server)
        .await;
}

async fn inferred(db: &DatabaseConnection) -> Vec<String> {
    verdicts::list_for_user(db, "user-1")
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.session_id == AUTO_FEEDBACK_SESSION)
        .filter_map(|r| r.outcome)
        .collect()
}

async fn pass(db: &DatabaseConnection, server: &MockServer, snapshot: &LibrarySnapshot) -> usize {
    let client = CodexClient::new(&server.uri(), reqwest::Client::new());
    let credential = credential();
    let access = CodexAccess {
        client: &client,
        credential: &credential,
    };
    reconcile_user(db, "user-1", Some(snapshot), Some(access))
        .await
        .unwrap()
        .inferred
}

#[tokio::test]
async fn a_read_stalled_for_a_hundred_days_is_recorded_as_dnf() {
    let (db, codex) = (fresh_db().await, MockServer::start().await);
    recommend(&db).await;
    books_last_read(&codex, 100).await;

    assert_eq!(pass(&db, &codex, &owned(ReadState::InProgress, 9)).await, 1);
    assert_eq!(inferred(&db).await, vec!["dnf"]);
    assert_eq!(pass(&db, &codex, &owned(ReadState::InProgress, 9)).await, 0);
}

#[tokio::test]
async fn recent_progress_is_not_a_stall() {
    let (db, codex) = (fresh_db().await, MockServer::start().await);
    recommend(&db).await;
    books_last_read(&codex, 10).await;

    assert_eq!(pass(&db, &codex, &owned(ReadState::InProgress, 9)).await, 0);
    assert!(inferred(&db).await.is_empty());
}

#[tokio::test]
async fn a_caught_up_series_is_never_stalled() {
    let (db, codex) = (fresh_db().await, MockServer::start().await);
    recommend(&db).await;
    books_last_read(&codex, 400).await;

    // Every owned book read: waiting for new volumes, not abandoned.
    assert_eq!(pass(&db, &codex, &owned(ReadState::InProgress, 0)).await, 0);
    assert!(inferred(&db).await.is_empty());
}

#[tokio::test]
async fn resuming_withdraws_an_inferred_dnf() {
    let (db, stalled) = (fresh_db().await, MockServer::start().await);
    recommend(&db).await;
    books_last_read(&stalled, 100).await;
    pass(&db, &stalled, &owned(ReadState::InProgress, 9)).await;

    let resumed = MockServer::start().await;
    books_last_read(&resumed, 2).await;
    assert_eq!(
        pass(&db, &resumed, &owned(ReadState::InProgress, 7)).await,
        1
    );
    assert_eq!(inferred(&db).await, vec!["dnf", REOPENED]);

    let rows = verdicts::list_for_user(&db, "user-1").await.unwrap();
    assert_eq!(verdicts::fold(&rows).outcome, None, "open again");
}

#[tokio::test]
async fn a_codex_failure_writes_nothing() {
    let (db, codex) = (fresh_db().await, MockServer::start().await);
    recommend(&db).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/s-1/books"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&codex)
        .await;

    assert_eq!(pass(&db, &codex, &owned(ReadState::InProgress, 9)).await, 0);
    assert!(inferred(&db).await.is_empty());
}

#[tokio::test]
async fn a_snapshot_build_runs_the_stall_check() {
    let (db, codex) = (fresh_db().await, MockServer::start().await);
    recommend(&db).await;
    library_mocks(&codex).await;
    books_last_read(&codex, 100).await;

    let services = services_for(&codex, &db);
    let user = UserCtx {
        user_sub: "user-1".into(),
        credential: credential(),
    };
    services
        .snapshots
        .get(&user.user_sub, &user.credential)
        .await
        .expect("snapshot");

    // The hook runs in the background; give it a moment.
    for _ in 0..50 {
        if !inferred(&db).await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(inferred(&db).await, vec!["dnf"]);
}

/// Codex serving a one-series library (the recommended series, 9 of 10
/// books unread, no ratings).
async fn library_mocks(codex: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/v1/user/ratings"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ratings": [] })),
        )
        .mount(codex)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/full"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{
                "id": "s-1", "libraryId": "lib", "libraryName": "Manga",
                "bookCount": 10, "unreadCount": 9,
                "metadata": { "title": "Recommended", "summary": null, "status": "ongoing",
                              "year": 2020, "publisher": null, "totalVolumeCount": 10,
                              "totalChapterCount": null },
                "genres": [], "tags": [],
                "externalIds": [{ "id": "x-1", "seriesId": "s-1", "source": "plugin:mangabaka",
                                  "externalId": "1", "externalUrl": null }]
            }],
            "page": 1, "pageSize": 100, "total": 1, "totalPages": 1
        })))
        .mount(codex)
        .await;
}

fn services_for(codex: &MockServer, db: &DatabaseConnection) -> Services {
    let http = reqwest::Client::new();
    Services::new(
        CodexClient::new(&codex.uri(), http.clone()),
        TsundokuClient::new("", Secret::default(), http),
        inert_mangabaka(),
        db.clone(),
        &CacheConfig::default(),
        false,
    )
}

/// An account linked to Codex user `sub` with `key` stored, as sign-in
/// leaves it.
async fn account_with_key(db: &DatabaseConnection, dek: &Dek, sub: &str, key: &str) {
    let row = users::find_or_create(
        db,
        &users::OidcIdentity {
            provider: "test".into(),
            subject: sub.into(),
            email: None,
            display_name: None,
            avatar_url: None,
            groups: Vec::new(),
        },
    )
    .await
    .unwrap();
    users::set_codex_credential(db, &row.id, sub, key, dek, 1)
        .await
        .unwrap();
}

/// Codex `/me` answering for `key` as user `sub`.
async fn me(codex: &MockServer, key: &str, sub: &str) {
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .and(header("X-API-Key", key))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": sub, "username": sub, "email": format!("{sub}@example.test"),
            "role": "user", "emailVerified": true, "permissions": []
        })))
        .mount(codex)
        .await;
}

fn stored_keys(codex: &MockServer) -> StoredKeys {
    StoredKeys {
        identity: Arc::new(CodexIdentity::new(
            CodexClient::new(&codex.uri(), reqwest::Client::new()),
            std::time::Duration::from_secs(300),
        )),
        dek: Dek::from_bytes([3u8; 32]),
    }
}

#[tokio::test]
async fn the_sweep_rereads_an_idle_users_library_and_records_the_stall() {
    let (db, codex) = (fresh_db().await, MockServer::start().await);
    let keys = stored_keys(&codex);
    account_with_key(&db, &keys.dek, "user-1", "key-1").await;
    me(&codex, "key-1", "user-1").await;
    recommend(&db).await;
    library_mocks(&codex).await;
    books_last_read(&codex, 100).await;
    let services = services_for(&codex, &db);

    // Nothing cached: only the stored key can reach this user's library.
    assert!(services.snapshots.peek("user-1").is_none());
    let report = sweeps::run_reconcile_sweep(&services, Some(&keys))
        .await
        .unwrap();

    assert_eq!(
        (report.refreshed, report.skipped, report.inferred),
        (1, 0, 1)
    );
    assert_eq!(inferred(&db).await, vec!["dnf"]);
    assert!(
        services.snapshots.peek("user-1").is_some(),
        "snapshot cached"
    );
}

#[tokio::test]
async fn a_refused_key_is_skipped_and_the_sweep_goes_on() {
    let (db, codex) = (fresh_db().await, MockServer::start().await);
    let keys = stored_keys(&codex);
    // user-0 sorts first; Codex no longer accepts its key.
    account_with_key(&db, &keys.dek, "user-0", "revoked").await;
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .and(header("X-API-Key", "revoked"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&codex)
        .await;
    verdicts::insert(
        &db,
        "user-0",
        NewVerdict {
            external_id: Some(ExternalId::from_tsundoku("mangabaka:2").unwrap()),
            session_id: "s0".into(),
            tier: "A".into(),
            rationale: "fits".into(),
            verified: true,
            retracted: false,
            retraction_reason: None,
            outcome: None,
            notes: None,
        },
    )
    .await
    .unwrap();
    account_with_key(&db, &keys.dek, "user-1", "key-1").await;
    me(&codex, "key-1", "user-1").await;
    recommend(&db).await;
    library_mocks(&codex).await;
    books_last_read(&codex, 100).await;
    let services = services_for(&codex, &db);

    let report = sweeps::run_reconcile_sweep(&services, Some(&keys))
        .await
        .unwrap();

    assert_eq!((report.users, report.refreshed, report.skipped), (2, 1, 1));
    assert_eq!(inferred(&db).await, vec!["dnf"]);
}

#[tokio::test]
async fn without_keys_the_sweep_reads_only_cached_snapshots() {
    let (db, codex) = (fresh_db().await, MockServer::start().await);
    let keys = stored_keys(&codex);
    account_with_key(&db, &keys.dek, "user-1", "key-1").await;
    me(&codex, "key-1", "user-1").await;
    recommend(&db).await;
    library_mocks(&codex).await;
    books_last_read(&codex, 100).await;
    let services = services_for(&codex, &db);

    let report = sweeps::run_reconcile_sweep(&services, None).await.unwrap();

    assert_eq!((report.refreshed, report.with_snapshot), (0, 0));
    assert!(inferred(&db).await.is_empty());
}

#[tokio::test]
async fn concurrent_passes_record_a_stall_once() {
    let (db, codex) = (fresh_db().await, MockServer::start().await);
    recommend(&db).await;
    library_mocks(&codex).await;
    books_last_read(&codex, 100).await;
    let services = services_for(&codex, &db);
    let credential = credential();

    let (a, b) = tokio::join!(
        services.reconcile_now("user-1", &credential),
        services.reconcile_now("user-1", &credential),
    );
    let total = a.unwrap().1.reconcile.inferred + b.unwrap().1.reconcile.inferred;

    assert_eq!(total, 1);
    assert_eq!(inferred(&db).await, vec!["dnf"]);
}

fn inert_mangabaka() -> std::sync::Arc<shisho_mangabaka::MangabakaProvider> {
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
