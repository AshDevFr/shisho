//! `record_feedback`: the one write path both the MCP tool and the REST
//! outcome endpoint use. Covers idempotent re-recording, outcome
//! normalization, and the shared series token.

use migration::{Migrator, MigratorTrait};
use sea_orm::{Database, DatabaseConnection};
use shisho_db::repos::verdicts::{self, verdict_series_token};
use shisho_ids::ExternalId;
use shisho_services::verdicts::{Outcome, record_feedback};

async fn fresh_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.expect("connect");
    Migrator::up(&db, None).await.expect("migrate");
    db
}

fn frieren() -> ExternalId {
    ExternalId::from_tsundoku("mangabaka:112092").unwrap()
}

#[tokio::test]
async fn re_recording_the_same_feedback_appends_one_row() {
    let db = fresh_db().await;

    let first = record_feedback(&db, "user-1", &frieren(), "loved", Some("the art".into()))
        .await
        .expect("first");
    let second = record_feedback(&db, "user-1", &frieren(), "Loved", Some("the art".into()))
        .await
        .expect("second");

    assert!(!first.deduplicated);
    assert!(second.deduplicated, "same outcome + notes is a no-op");
    assert_eq!(first.row.id, second.row.id);
    let history = verdicts::history(&db, "user-1", [&frieren()])
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].outcome.as_deref(), Some("loved"));
}

#[tokio::test]
async fn a_different_outcome_appends_a_new_row() {
    let db = fresh_db().await;
    record_feedback(&db, "user-1", &frieren(), "liked", None)
        .await
        .unwrap();
    let later = record_feedback(&db, "user-1", &frieren(), "dropped", None)
        .await
        .unwrap();
    assert!(!later.deduplicated);
    let history = verdicts::history(&db, "user-1", [&frieren()])
        .await
        .unwrap();
    assert_eq!(history.len(), 2);
}

#[test]
fn outcomes_normalize_to_the_ui_vocabulary() {
    assert_eq!(Outcome::parse("Did Not Finish"), Outcome::Dnf);
    assert_eq!(Outcome::parse("DNF"), Outcome::Dnf);
    assert_eq!(Outcome::parse(" loved "), Outcome::Loved);
    assert_eq!(Outcome::parse("Dropped"), Outcome::Dropped);
    assert_eq!(Outcome::parse("meh"), Outcome::Other);
}

#[tokio::test]
async fn unknown_outcome_text_is_kept_in_the_notes() {
    let db = fresh_db().await;
    let ack = record_feedback(&db, "user-1", &frieren(), "meh", Some("slow start".into()))
        .await
        .unwrap();
    assert_eq!(ack.row.outcome.as_deref(), Some("other"));
    let notes = ack.row.notes.as_deref().unwrap_or_default();
    assert!(
        notes.contains("meh") && notes.contains("slow start"),
        "{notes}"
    );
}

#[tokio::test]
async fn series_token_prefers_the_mangabaka_id() {
    let db = fresh_db().await;
    let ack = record_feedback(&db, "user-1", &frieren(), "liked", None)
        .await
        .unwrap();
    assert_eq!(
        verdict_series_token(&ack.row).as_deref(),
        Some("mangabaka:112092")
    );
}
