//! Per-user tuning: parameters come from each user's own library, are stored
//! per user, and are what that user's candidates are ranked with.

use migration::{Migrator, MigratorTrait};
use sea_orm::{Database, DatabaseConnection};
use shisho_ids::ExternalId;
use shisho_services::affinity::TasteParams;
use shisho_services::library_snapshot::{LibrarySnapshot, SnapshotSeries};
use shisho_services::read_state::ReadState;
use shisho_services::tuning::{self, MARGIN, MIN_RATINGS};

async fn fresh_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.expect("connect");
    Migrator::up(&db, None).await.expect("migrate");
    db
}

fn series(i: usize, genre: &str, state: ReadState, rating: Option<i32>) -> SnapshotSeries {
    SnapshotSeries {
        id: format!("s{i}"),
        library_id: "lib".into(),
        library_name: "Library".into(),
        title: format!("series {i}"),
        summary: None,
        status: None,
        year: None,
        genres: vec![genre.into()],
        tags: Vec::new(),
        authors: Vec::new(),
        book_count: 5,
        unread_count: Some(if state == ReadState::Completed { 0 } else { 5 }),
        read_state: state,
        external_ids: vec![ExternalId::from_tsundoku(&format!("mal:{i}")).unwrap()],
        rating,
        rating_notes: None,
        rating_updated_at: None,
    }
}

/// A harsh rater with a wide scale: a dozen loved "Loved" series, many
/// middling "Meh" ones, a few hated "Hated" ones, and a backlog of Meh.
/// One standard deviation below their mean, Meh reads as liked and buries
/// the favourites; at the mean it reads as neutral.
fn harsh_rater() -> LibrarySnapshot {
    let mut all = Vec::new();
    let mut i = 0;
    let mut push = |genre: &str, state: ReadState, rating: Option<i32>| {
        all.push(series(i, genre, state, rating));
        i += 1;
    };
    for r in 0..12 {
        push("Loved", ReadState::Completed, Some(85 + r % 10));
    }
    for r in 0..30 {
        push("Meh", ReadState::Completed, Some(45 + r % 7));
    }
    for r in 0..10 {
        push("Hated", ReadState::Completed, Some(10 + 2 * r));
    }
    for _ in 0..20 {
        push("Meh", ReadState::Untouched, None);
    }
    LibrarySnapshot::from_series(all)
}

#[test]
fn a_library_that_disagrees_with_the_defaults_is_tuned_to_its_own_data() {
    let t = tuning::tune(&harsh_rater());
    assert!(t.tuned, "{t:?}");
    assert_eq!(t.params.origin_sd, 0.0, "{:?}", t.params);
    let (base, best) = (t.baseline.unwrap(), t.best.unwrap());
    assert!(best.mean_rank_fraction <= base.mean_rank_fraction - MARGIN);
}

#[test]
fn too_few_ratings_keep_the_defaults() {
    let few: Vec<SnapshotSeries> = (0..MIN_RATINGS - 1)
        .map(|i| series(i, "Loved", ReadState::Completed, Some(90)))
        .collect();
    let t = tuning::tune(&LibrarySnapshot::from_series(few));
    assert!(!t.tuned);
    assert_eq!(t.params, TasteParams::default());
}

#[tokio::test]
async fn tuned_parameters_are_stored_and_read_per_user() {
    let db = fresh_db().await;
    let tuned = tuning::tune(&harsh_rater());
    tuning::store(&db, "harsh", &tuned).await.unwrap();

    assert_eq!(tuning::params_for(&db, "harsh").await, tuned.params);
    assert_eq!(
        tuning::params_for(&db, "someone-else").await,
        TasteParams::default(),
        "another user's tuning never applies"
    );

    // A fresh tuning is not recomputed.
    let snapshot = std::sync::Arc::new(harsh_rater());
    assert!(
        tuning::retune_if_stale(&db, "harsh", snapshot.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        tuning::retune_if_stale(&db, "new-user", snapshot)
            .await
            .unwrap()
            .is_some()
    );
}
