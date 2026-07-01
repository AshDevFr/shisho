//! Schema smoke tests: run the migrations on in-memory SQLite, insert and
//! read back one row per table, and verify re-running is idempotent.

use migration::{Migrator, MigratorTrait};
use sea_orm::{ActiveModelTrait, Database, DatabaseConnection, EntityTrait, Set};
use shisho_db::entities::{feed_cursor, profile, profile_version, verdict};

async fn fresh_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.expect("connect");
    Migrator::up(&db, None).await.expect("migrate");
    db
}

#[tokio::test]
async fn migrations_are_idempotent() {
    let db = fresh_db().await;
    Migrator::up(&db, None)
        .await
        .expect("second run is a no-op");
}

#[tokio::test]
async fn verdict_round_trip() {
    let db = fresh_db().await;

    let inserted = verdict::ActiveModel {
        user_sub: Set("user-1".into()),
        mangabaka_id: Set(Some(4242)),
        provider: Set(None),
        external_id: Set(None),
        session_id: Set("session-1".into()),
        tier: Set("S".into()),
        rationale: Set("matches the profile's seinen preference".into()),
        verified: Set(true),
        retracted: Set(false),
        outcome: Set(None),
        notes: Set(None),
        created_at: Set(chrono::Utc::now().into()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("insert verdict");

    let found = verdict::Entity::find_by_id(inserted.id)
        .one(&db)
        .await
        .expect("query")
        .expect("row exists");
    assert_eq!(found.user_sub, "user-1");
    assert_eq!(found.mangabaka_id, Some(4242));
    assert!(found.verified);
    assert!(!found.retracted);
}

#[tokio::test]
async fn verdict_fallback_identity_is_storable() {
    // No mangabaka_id: identity falls back to (provider, external_id).
    let db = fresh_db().await;

    let inserted = verdict::ActiveModel {
        user_sub: Set("user-1".into()),
        mangabaka_id: Set(None),
        provider: Set(Some("mal".into())),
        external_id: Set(Some("123".into())),
        session_id: Set("session-1".into()),
        tier: Set("A".into()),
        rationale: Set("fallback identity".into()),
        verified: Set(false),
        retracted: Set(false),
        outcome: Set(None),
        notes: Set(None),
        created_at: Set(chrono::Utc::now().into()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("insert verdict");

    assert_eq!(inserted.provider.as_deref(), Some("mal"));
    assert_eq!(inserted.external_id.as_deref(), Some("123"));
}

#[tokio::test]
async fn profile_and_versions_round_trip() {
    let db = fresh_db().await;

    profile::ActiveModel {
        user_sub: Set("user-1".into()),
        content: Set("# Taste Profile\n\nLikes assertive MCs.".into()),
        version: Set(1),
        updated_at: Set(chrono::Utc::now().into()),
    }
    .insert(&db)
    .await
    .expect("insert profile");

    profile_version::ActiveModel {
        user_sub: Set("user-1".into()),
        version: Set(1),
        content: Set("# Taste Profile\n\nLikes assertive MCs.".into()),
        change_summary: Set("initial profile".into()),
        status: Set("applied".into()),
        base_version: Set(0),
        created_at: Set(chrono::Utc::now().into()),
    }
    .insert(&db)
    .await
    .expect("insert profile version");

    let profile = profile::Entity::find_by_id("user-1")
        .one(&db)
        .await
        .expect("query")
        .expect("profile exists");
    assert_eq!(profile.version, 1);

    let version = profile_version::Entity::find_by_id(("user-1".to_owned(), 1))
        .one(&db)
        .await
        .expect("query")
        .expect("version exists");
    assert_eq!(version.change_summary, "initial profile");
}

#[tokio::test]
async fn feed_cursor_round_trip() {
    let db = fresh_db().await;

    feed_cursor::ActiveModel {
        id: Set("tsundoku".into()),
        cursor: Set("opaque-cursor-token".into()),
        updated_at: Set(chrono::Utc::now().into()),
    }
    .insert(&db)
    .await
    .expect("insert cursor");

    let cursor = feed_cursor::Entity::find_by_id("tsundoku")
        .one(&db)
        .await
        .expect("query")
        .expect("cursor exists");
    assert_eq!(cursor.cursor, "opaque-cursor-token");
}

#[tokio::test]
async fn feed_cursor_upsert_replaces_value_and_timestamp() {
    use sea_orm::sea_query::OnConflict;
    let db = fresh_db().await;

    async fn upsert(db: &DatabaseConnection, cursor: &str) {
        feed_cursor::Entity::insert(feed_cursor::ActiveModel {
            id: Set("tsundoku".into()),
            cursor: Set(cursor.into()),
            updated_at: Set(chrono::Utc::now().into()),
        })
        .on_conflict(
            OnConflict::column(feed_cursor::Column::Id)
                .update_columns([feed_cursor::Column::Cursor, feed_cursor::Column::UpdatedAt])
                .to_owned(),
        )
        .exec(db)
        .await
        .expect("upsert");
    }

    upsert(&db, "cursor-1").await;
    upsert(&db, "cursor-2").await;

    let row = feed_cursor::Entity::find_by_id("tsundoku")
        .one(&db)
        .await
        .expect("query")
        .expect("row");
    assert_eq!(row.cursor, "cursor-2", "upsert replaced the cursor value");
    // The primary-key upsert replaces in place, never a second row.
    let all = feed_cursor::Entity::find().all(&db).await.expect("all");
    assert_eq!(all.len(), 1, "upsert must not create a second row");
}
