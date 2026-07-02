//! Tsundoku query cache + feed-cursor invalidation: scripted feed pages
//! (wiremock fixtures) against the real `feed_cursor` table on in-memory
//! SQLite.

use std::time::Duration;

use migration::{Migrator, MigratorTrait};
use sea_orm::{ActiveValue::Set, Database, DatabaseConnection, EntityTrait};
use shisho_cache::SwrConfig;
use shisho_config::Secret;
use shisho_db::entities::feed_cursor;
use shisho_services::tsundoku_cache::TsundokuCache;
use shisho_tsundoku::{SeriesListQuery, TsundokuClient};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/../../tests/fixtures/tsundoku/{name}.json",
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

fn cache(server: &MockServer, db: DatabaseConnection) -> TsundokuCache {
    TsundokuCache::new(
        TsundokuClient::new(
            &server.uri(),
            Secret::new("admin-token"),
            reqwest::Client::new(),
        ),
        db,
        SwrConfig {
            revalidate_after: Duration::from_secs(300),
            build_budget: Duration::from_secs(30),
        },
    )
}

async fn stored_cursor(db: &DatabaseConnection) -> Option<String> {
    feed_cursor::Entity::find_by_id("tsundoku")
        .one(db)
        .await
        .expect("query cursor")
        .map(|row| row.cursor)
}

#[tokio::test]
async fn empty_feed_page_extends_lifetimes_without_invalidation() {
    let server = MockServer::start().await;
    let db = fresh_db().await;

    // expect(1) on both query endpoints: the post-poll reads below must be
    // served from cache, proving nothing was invalidated.
    Mock::given(method("GET"))
        .and(path("/api/v1/series"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("series_page")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/101"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("series_detail")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/feed"))
        .and(query_param_is_missing("cursor"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("feed_empty")))
        .expect(1)
        .mount(&server)
        .await;

    let cache = cache(&server, db.clone());
    let query = SeriesListQuery {
        kind: Some("manga".into()),
        ..Default::default()
    };
    cache.list(&query).await.expect("warm list");
    cache.detail(101).await.expect("warm detail");

    let outcome = cache.poll_feed_once().await.expect("poll");
    assert!(outcome.unchanged);
    assert_eq!(outcome.changed, 0);
    assert_eq!(outcome.rehydrated, 0);

    // Still served from cache after the poll.
    cache.list(&query).await.expect("cached list");
    cache.detail(101).await.expect("cached detail");

    // The empty feed carried no cursor; nothing to persist.
    assert_eq!(stored_cursor(&db).await, None);
}

#[tokio::test]
async fn non_empty_page_invalidates_lists_rehydrates_warm_details_and_persists_cursor() {
    let server = MockServer::start().await;
    let db = fresh_db().await;

    // Detail 101 is warm: initial load + feed re-hydration = 2 calls.
    Mock::given(method("GET"))
        .and(path("/api/v1/series/101"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("series_detail")))
        .expect(2)
        .mount(&server)
        .await;
    // Lists are invalidated wholesale: initial load + post-poll rebuild.
    Mock::given(method("GET"))
        .and(path("/api/v1/series"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("series_page")))
        .expect(2)
        .mount(&server)
        .await;
    // Feed: page 1 carries the change for series 101 and a next cursor;
    // the follow-up page (with that cursor) is empty and terminates.
    Mock::given(method("GET"))
        .and(path("/api/v1/series/feed"))
        .and(query_param_is_missing("cursor"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("feed_page1")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/feed"))
        .and(query_param("cursor", "eyJ2IjoxLCJpZCI6MTAxfQ"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("feed_empty")))
        .expect(1)
        .mount(&server)
        .await;

    let cache = cache(&server, db.clone());
    let query = SeriesListQuery {
        kind: Some("manga".into()),
        ..Default::default()
    };
    cache.list(&query).await.expect("warm list");
    cache.detail(101).await.expect("warm detail");

    let outcome = cache.poll_feed_once().await.expect("poll");
    assert!(!outcome.unchanged);
    assert_eq!(outcome.changed, 1);
    assert_eq!(outcome.rehydrated, 1, "warm detail re-hydrated");

    // Cursor advance committed after processing.
    assert_eq!(
        stored_cursor(&db).await.as_deref(),
        Some("eyJ2IjoxLCJpZCI6MTAxfQ")
    );

    // The re-hydrated detail serves from cache (still 2 endpoint calls);
    // the list was invalidated and rebuilds on demand (2nd list call).
    cache.detail(101).await.expect("rehydrated detail");
    cache.list(&query).await.expect("rebuilt list");
}

#[tokio::test]
async fn poll_resumes_from_the_stored_cursor_after_restart() {
    let server = MockServer::start().await;
    let db = fresh_db().await;

    // A previous process run left a cursor behind.
    feed_cursor::Entity::insert(feed_cursor::ActiveModel {
        id: Set("tsundoku".into()),
        cursor: Set("stored-cursor-42".into()),
        updated_at: Set(chrono::Utc::now().into()),
    })
    .exec(&db)
    .await
    .expect("seed cursor");

    // The mock only matches a feed request carrying the stored cursor:
    // expect(1) proves the restarted poller resumed from it.
    Mock::given(method("GET"))
        .and(path("/api/v1/series/feed"))
        .and(query_param("cursor", "stored-cursor-42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("feed_empty")))
        .expect(1)
        .mount(&server)
        .await;

    let cache = cache(&server, db.clone());
    let outcome = cache.poll_feed_once().await.expect("poll");
    assert!(outcome.unchanged);
    assert_eq!(
        stored_cursor(&db).await.as_deref(),
        Some("stored-cursor-42")
    );
}
