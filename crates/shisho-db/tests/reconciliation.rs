//! Reconciliation-flag repository tests: a flag is raised exactly once per
//! verdict, resolution closes pending flags only, and queries are scoped
//! to one user.

use migration::{Migrator, MigratorTrait};
use sea_orm::{Database, DatabaseConnection};

use shisho_db::repos::reconciliation;
use shisho_db::repos::verdicts::{self, NewVerdict};
use shisho_ids::ExternalId;

async fn fresh_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.expect("connect");
    Migrator::up(&db, None).await.expect("migrate");
    db
}

async fn seed_verdict(db: &DatabaseConnection, user_sub: &str) -> i64 {
    verdicts::insert(
        db,
        user_sub,
        NewVerdict {
            external_id: Some(ExternalId::from_tsundoku("mangabaka:112092").unwrap()),
            session_id: "s1".into(),
            tier: "S".into(),
            rationale: "matches the profile".into(),
            verified: true,
            retracted: false,
            retraction_reason: None,
            outcome: None,
            notes: None,
        },
    )
    .await
    .expect("insert verdict")
    .id
}

#[tokio::test]
async fn a_flag_is_raised_exactly_once_per_verdict() {
    let db = fresh_db().await;
    let verdict_id = seed_verdict(&db, "user-1").await;

    let first = reconciliation::raise(&db, "user-1", verdict_id)
        .await
        .expect("raise");
    assert!(first.is_some(), "first raise creates the flag");

    let second = reconciliation::raise(&db, "user-1", verdict_id)
        .await
        .expect("raise again");
    assert!(second.is_none(), "second raise is a no-op");

    let pending = reconciliation::pending(&db, "user-1")
        .await
        .expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].verdict_id, verdict_id);
    assert!(pending[0].resolved_at.is_none());
}

#[tokio::test]
async fn resolution_closes_pending_flags_and_is_idempotent() {
    let db = fresh_db().await;
    let verdict_id = seed_verdict(&db, "user-1").await;
    reconciliation::raise(&db, "user-1", verdict_id)
        .await
        .expect("raise");

    let resolved = reconciliation::resolve_for_verdicts(&db, "user-1", &[verdict_id])
        .await
        .expect("resolve");
    assert_eq!(resolved, 1);
    assert_eq!(
        reconciliation::pending_count(&db, "user-1")
            .await
            .expect("count"),
        0
    );

    // Resolving again touches nothing.
    let resolved = reconciliation::resolve_for_verdicts(&db, "user-1", &[verdict_id])
        .await
        .expect("resolve again");
    assert_eq!(resolved, 0);

    // A resolved flag still blocks re-raising: the question was answered.
    let re_raised = reconciliation::raise(&db, "user-1", verdict_id)
        .await
        .expect("raise after resolve");
    assert!(re_raised.is_none());
}

#[tokio::test]
async fn resolve_for_series_closes_flags_by_series_identity() {
    let db = fresh_db().await;
    let verdict_id = seed_verdict(&db, "user-1").await;
    reconciliation::raise(&db, "user-1", verdict_id)
        .await
        .expect("raise");

    let id = ExternalId::from_tsundoku("mangabaka:112092").unwrap();
    let resolved = reconciliation::resolve_for_series(&db, "user-1", [&id])
        .await
        .expect("resolve by series");
    assert_eq!(resolved, 1);

    let joined = reconciliation::pending_with_verdicts(&db, "user-1")
        .await
        .expect("join");
    assert!(joined.is_empty());
}

#[tokio::test]
async fn pending_with_verdicts_joins_the_flag_to_its_verdict_row() {
    let db = fresh_db().await;
    let verdict_id = seed_verdict(&db, "user-1").await;
    reconciliation::raise(&db, "user-1", verdict_id)
        .await
        .expect("raise");

    let joined = reconciliation::pending_with_verdicts(&db, "user-1")
        .await
        .expect("join");
    assert_eq!(joined.len(), 1);
    let (flag, verdict) = &joined[0];
    assert_eq!(flag.verdict_id, verdict_id);
    assert_eq!(verdict.id, verdict_id);
    assert_eq!(verdict.tier, "S");
}

#[tokio::test]
async fn pending_queries_are_scoped_per_user() {
    let db = fresh_db().await;
    let mine = seed_verdict(&db, "user-1").await;
    let theirs = seed_verdict(&db, "user-2").await;
    reconciliation::raise(&db, "user-1", mine)
        .await
        .expect("raise");
    reconciliation::raise(&db, "user-2", theirs)
        .await
        .expect("raise");

    let pending = reconciliation::pending(&db, "user-1")
        .await
        .expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].verdict_id, mine);

    // Resolving with the wrong user touches nothing.
    let resolved = reconciliation::resolve_for_verdicts(&db, "user-1", &[theirs])
        .await
        .expect("cross-user resolve");
    assert_eq!(resolved, 0);
    assert_eq!(
        reconciliation::pending_count(&db, "user-2")
            .await
            .expect("count"),
        1
    );
}
