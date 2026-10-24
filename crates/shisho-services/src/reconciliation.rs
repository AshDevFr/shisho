//! Outcome reconciliation: detect verdicts with no recorded outcome whose
//! series now shows read progress, and flag them for the next
//! consolidation ("you started X after it was recommended — outcome?").
//!
//! Detection needs per-user Codex read state, which Shisho cannot
//! originate (it stores no credentials). It therefore runs against the
//! library snapshot **when one is warm for that user** — from the weekly
//! sweep over cached snapshots, and lazily on every snapshot build (the
//! user's next authenticated touch). No warm snapshot means no flags, and
//! never a false one.
//!
//! Resolution is snapshot-independent: a series whose verdict fold carries
//! an outcome has answered the question, so its pending flags close. The
//! direct path is the feedback write itself; this module's resolve pass is
//! the catch-up for outcomes recorded while a flag query was in flight.

use std::collections::{HashMap, HashSet};

use sea_orm::DatabaseConnection;

use crate::library_snapshot::LibrarySnapshot;
use crate::read_state::ReadState;
use shisho_db::entities::verdict;
use shisho_db::repos::{reconciliation, verdicts};
use shisho_ids::Provider;

/// What one reconciliation pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileOutcome {
    /// Flags newly raised.
    pub flagged: usize,
    /// Pending flags closed because their series now has an outcome.
    pub resolved: usize,
}

/// One reconciliation pass for one user. `snapshot` is the warm library
/// snapshot when one exists; without it only the resolve pass runs.
pub async fn reconcile_user(
    db: &DatabaseConnection,
    user_sub: &str,
    snapshot: Option<&LibrarySnapshot>,
) -> Result<ReconcileOutcome, sea_orm::DbErr> {
    let rows = verdicts::list_for_user(db, user_sub).await?;

    // Group rows into series the same way exclusion does; identity-less
    // rows belong to no series and cannot reconcile.
    let mut groups: HashMap<verdicts::SeriesKey, Vec<verdict::Model>> = HashMap::new();
    for row in rows {
        let Some(key) = verdicts::SeriesKey::of(&row) else {
            continue;
        };
        groups.entry(key).or_default().push(row);
    }

    let mut outcome = ReconcileOutcome::default();
    for rows in groups.values() {
        let state = verdicts::fold(rows);

        if state.outcome.is_some() {
            // Question answered: close any pending flag on this series.
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            outcome.resolved +=
                reconciliation::resolve_for_verdicts(db, user_sub, &ids).await? as usize;
            continue;
        }

        // Detection: a standing (non-retracted) recommendation with no
        // outcome, whose series shows read progress in the snapshot.
        if !state.has_verdict || state.retracted {
            continue;
        }
        let Some(snapshot) = snapshot else {
            continue;
        };
        // The fold says the latest verdict row is non-retracted, so the
        // latest non-feedback row is the standing recommendation.
        let Some(standing) = rows.iter().rev().find(|r| r.outcome.is_none()) else {
            continue;
        };
        let identities = identity_keys(rows);
        let started = snapshot.series.iter().any(|series| {
            series
                .external_ids
                .iter()
                .any(|id| identities.contains(&(id.provider.clone(), id.id.clone())))
                && series.read_state != ReadState::Untouched
        });
        if started
            && reconciliation::raise(db, user_sub, standing.id)
                .await?
                .is_some()
        {
            outcome.flagged += 1;
        }
    }
    Ok(outcome)
}

/// Every `(Provider, id)` identity the series' rows were recorded under —
/// the join keys against the snapshot's external IDs.
fn identity_keys(rows: &[verdict::Model]) -> HashSet<(Provider, String)> {
    let mut keys = HashSet::new();
    for row in rows {
        if let Some(id) = row.mangabaka_id {
            keys.insert((Provider::Mangabaka, id.to_string()));
        }
        if let (Some(provider), Some(external_id)) = (&row.provider, &row.external_id) {
            keys.insert((Provider::from_tsundoku_token(provider), external_id.clone()));
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    use migration::{Migrator, MigratorTrait};
    use sea_orm::Database;

    use crate::library_snapshot::SnapshotSeries;
    use shisho_db::repos::verdicts::NewVerdict;
    use shisho_ids::ExternalId;

    async fn fresh_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.expect("connect");
        Migrator::up(&db, None).await.expect("migrate");
        db
    }

    fn series(raw_id: &str, read_state: ReadState) -> SnapshotSeries {
        SnapshotSeries {
            id: format!("uuid-{raw_id}"),
            library_id: "lib-1".into(),
            library_name: "Manga".into(),
            title: format!("Series {raw_id}"),
            summary: None,
            status: None,
            year: None,
            genres: Vec::new(),
            tags: Vec::new(),
            book_count: 10,
            unread_count: Some(10),
            read_state,
            external_ids: vec![ExternalId::from_tsundoku(raw_id).unwrap()],
            rating: None,
            rating_notes: None,
        }
    }

    fn verdict(raw_id: &str) -> NewVerdict {
        NewVerdict {
            external_id: Some(ExternalId::from_tsundoku(raw_id).unwrap()),
            session_id: "s1".into(),
            tier: "S".into(),
            rationale: "fits the profile".into(),
            verified: true,
            retracted: false,
            retraction_reason: None,
            outcome: None,
            notes: None,
        }
    }

    fn feedback(raw_id: &str, outcome: &str) -> NewVerdict {
        NewVerdict {
            outcome: Some(outcome.into()),
            tier: "feedback".into(),
            ..verdict(raw_id)
        }
    }

    #[tokio::test]
    async fn progress_without_outcome_raises_a_flag_exactly_once() {
        let db = fresh_db().await;
        let row = verdicts::insert(&db, "user-1", verdict("mangabaka:1"))
            .await
            .unwrap();
        let snapshot =
            LibrarySnapshot::from_series(vec![series("mangabaka:1", ReadState::InProgress)]);

        let first = reconcile_user(&db, "user-1", Some(&snapshot))
            .await
            .unwrap();
        assert_eq!(first.flagged, 1);
        let again = reconcile_user(&db, "user-1", Some(&snapshot))
            .await
            .unwrap();
        assert_eq!(again.flagged, 0, "re-run never re-flags");

        let pending = reconciliation::pending(&db, "user-1").await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].verdict_id, row.id);
    }

    #[tokio::test]
    async fn untouched_series_and_unknown_series_raise_nothing() {
        let db = fresh_db().await;
        verdicts::insert(&db, "user-1", verdict("mangabaka:1"))
            .await
            .unwrap();
        // An acquirable-pool verdict for a series not (yet) in the library.
        verdicts::insert(&db, "user-1", verdict("mangabaka:2"))
            .await
            .unwrap();
        let snapshot =
            LibrarySnapshot::from_series(vec![series("mangabaka:1", ReadState::Untouched)]);

        let pass = reconcile_user(&db, "user-1", Some(&snapshot))
            .await
            .unwrap();
        assert_eq!(pass, ReconcileOutcome::default());
    }

    #[tokio::test]
    async fn no_warm_snapshot_means_no_flags_until_the_next_build() {
        let db = fresh_db().await;
        verdicts::insert(&db, "user-1", verdict("mangabaka:1"))
            .await
            .unwrap();

        let cold = reconcile_user(&db, "user-1", None).await.unwrap();
        assert_eq!(cold.flagged, 0);

        // The next snapshot build (the user's next touch) flags it.
        let snapshot =
            LibrarySnapshot::from_series(vec![series("mangabaka:1", ReadState::Completed)]);
        let warm = reconcile_user(&db, "user-1", Some(&snapshot))
            .await
            .unwrap();
        assert_eq!(warm.flagged, 1);
    }

    #[tokio::test]
    async fn an_outcome_resolves_the_flag_and_blocks_re_detection() {
        let db = fresh_db().await;
        verdicts::insert(&db, "user-1", verdict("mangabaka:1"))
            .await
            .unwrap();
        let snapshot =
            LibrarySnapshot::from_series(vec![series("mangabaka:1", ReadState::InProgress)]);
        reconcile_user(&db, "user-1", Some(&snapshot))
            .await
            .unwrap();

        verdicts::insert(&db, "user-1", feedback("mangabaka:1", "loved"))
            .await
            .unwrap();
        let pass = reconcile_user(&db, "user-1", Some(&snapshot))
            .await
            .unwrap();
        assert_eq!(pass.resolved, 1);
        assert_eq!(pass.flagged, 0);
        assert_eq!(
            reconciliation::pending_count(&db, "user-1").await.unwrap(),
            0
        );

        // Re-running after resolution neither re-flags nor re-resolves.
        let idle = reconcile_user(&db, "user-1", Some(&snapshot))
            .await
            .unwrap();
        assert_eq!(idle, ReconcileOutcome::default());
    }

    #[tokio::test]
    async fn retracted_verdicts_are_not_flagged() {
        let db = fresh_db().await;
        verdicts::insert(&db, "user-1", verdict("mangabaka:1"))
            .await
            .unwrap();
        verdicts::insert(
            &db,
            "user-1",
            NewVerdict {
                retracted: true,
                retraction_reason: Some(
                    shisho_db::repos::verdicts::RetractionReason::FailedVerification,
                ),
                ..verdict("mangabaka:1")
            },
        )
        .await
        .unwrap();
        let snapshot =
            LibrarySnapshot::from_series(vec![series("mangabaka:1", ReadState::InProgress)]);

        let pass = reconcile_user(&db, "user-1", Some(&snapshot))
            .await
            .unwrap();
        assert_eq!(pass.flagged, 0);
    }
}
