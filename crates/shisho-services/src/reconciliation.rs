//! Outcome reconciliation: detect verdicts with no recorded outcome whose
//! series now shows read progress, and flag them for the next
//! consolidation ("you started X after it was recommended — outcome?").
//! Where Codex already answers the question, record the outcome instead:
//! a rating set after the recommendation, or a read that stalled (unread
//! books left and no progress for [`STALL_AFTER`]) as `dnf`. Inferred
//! outcomes are revised when the evidence changes and never follow an
//! outcome the user recorded.
//!
//! Detection needs per-user Codex read state. It runs against the library
//! snapshot **when one is warm for that user** — from the weekly sweep over
//! cached snapshots, and lazily on every snapshot build (the user's next
//! authenticated touch). Stall checks need the user's credential, so only
//! the snapshot-build path runs them. No warm snapshot means no flags, and
//! never a false one.
//!
//! Resolution is snapshot-independent: a series whose verdict fold carries
//! an outcome has answered the question, so its pending flags close. The
//! direct path is the feedback write itself; this module's resolve pass is
//! the catch-up for outcomes recorded while a flag query was in flight.

use std::collections::{HashMap, HashSet};

use sea_orm::DatabaseConnection;

use crate::affinity::RatingScale;
use crate::library_snapshot::{LibrarySnapshot, SnapshotSeries};
use crate::read_state::ReadState;
use crate::verdicts::{FEEDBACK_SESSION, Outcome};
use chrono::{DateTime, Utc};
use shisho_codex::{CodexClient, CodexCredential};
use shisho_db::entities::verdict;
use shisho_db::repos::verdicts::{NewVerdict, REOPENED};
use shisho_db::repos::{reconciliation, verdicts};
use shisho_ids::{ExternalId, Provider};

/// What one reconciliation pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileOutcome {
    /// Flags newly raised.
    pub flagged: usize,
    /// Pending flags closed because their series now has an outcome.
    pub resolved: usize,
    /// Outcomes recorded (or revised) from what Codex shows.
    pub inferred: usize,
}

/// Session id of outcomes Shisho inferred from Codex rather than heard from
/// the user. Any other session's outcome is the user's own and final.
pub const AUTO_FEEDBACK_SESSION: &str = "codex-auto";

/// How long a read with unread books left may go without progress before it
/// counts as abandoned.
pub const STALL_AFTER: chrono::TimeDelta = chrono::TimeDelta::days(90);

/// Codex access for the checks that need a live call (book progress).
#[derive(Clone, Copy)]
pub struct CodexAccess<'a> {
    pub client: &'a CodexClient,
    pub credential: &'a CodexCredential,
}

/// One reconciliation pass for one user. `snapshot` is the warm library
/// snapshot when one exists; without it only the resolve pass runs.
pub async fn reconcile_user(
    db: &DatabaseConnection,
    user_sub: &str,
    snapshot: Option<&LibrarySnapshot>,
    codex: Option<CodexAccess<'_>>,
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

    let scale = snapshot.map(RatingScale::of_snapshot);
    let mut outcome = ReconcileOutcome::default();
    for (key, rows) in &mut groups {
        let mut state = verdicts::fold(rows);
        let latest_outcome = rows.iter().rev().find(|r| r.outcome.is_some());
        let user_answered = latest_outcome.is_some_and(|r| r.session_id != AUTO_FEEDBACK_SESSION);
        let standing = (state.has_verdict && !state.retracted)
            .then(|| rows.iter().rev().find(|r| r.outcome.is_none()))
            .flatten()
            .map(|r| (r.id, r.created_at.to_utc()));

        // Inference: only for a standing recommendation the user has not
        // answered themselves, and only from evidence newer than it.
        if let (Some(snapshot), Some(scale), Some((_, recommended_at)), false) =
            (snapshot, scale, standing, user_answered)
        {
            let identities = identity_keys(rows);
            let copies: Vec<&SnapshotSeries> = matching_series(snapshot, &identities).collect();
            let rated = copies
                .iter()
                .filter_map(|s| Some((s.rating?, s.rating_updated_at?)))
                .filter(|(_, at)| *at > recommended_at)
                .max_by_key(|(_, at)| *at);
            let current = state.outcome.as_deref();
            let decision = match rated {
                Some((rating, at)) => Some((
                    scale.outcome(rating).as_str(),
                    format!("Rated {rating}/100 in Codex on {}", at.format("%Y-%m-%d")),
                )),
                None => read_state_decision(&copies, current, codex).await,
            }
            .filter(|(inferred, _)| current != Some(*inferred));

            if let Some((inferred, notes)) = decision {
                let row = verdicts::insert(
                    db,
                    user_sub,
                    NewVerdict {
                        external_id: ExternalId::from_tsundoku(&key.token()).ok(),
                        session_id: AUTO_FEEDBACK_SESSION.into(),
                        tier: FEEDBACK_SESSION.into(),
                        rationale: "inferred from Codex".into(),
                        verified: false,
                        retracted: false,
                        retraction_reason: None,
                        outcome: Some(inferred.to_owned()),
                        notes: Some(notes),
                    },
                )
                .await?;
                rows.push(row);
                state = verdicts::fold(rows);
                outcome.inferred += 1;
            }
        }

        if state.outcome.is_some() {
            // Question answered: close any pending flag on this series.
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            outcome.resolved +=
                reconciliation::resolve_for_verdicts(db, user_sub, &ids).await? as usize;
            continue;
        }

        // Detection: a standing (non-retracted) recommendation with no
        // outcome, whose series shows read progress in the snapshot.
        let (Some(snapshot), Some((standing_id, _))) = (snapshot, standing) else {
            continue;
        };
        let identities = identity_keys(rows);
        let started = matching_series(snapshot, &identities)
            .any(|series| series.read_state != ReadState::Untouched);
        if started
            && reconciliation::raise(db, user_sub, standing_id)
                .await?
                .is_some()
        {
            outcome.flagged += 1;
        }
    }
    Ok(outcome)
}

/// What the read state of a recommended series says, short of a rating:
/// `dnf` for a stalled read, `reopened` when an inferred `dnf` is
/// contradicted (finished, caught up, or reading again). `None` when there
/// is no evidence either way, including when Codex could not be asked.
async fn read_state_decision(
    copies: &[&SnapshotSeries],
    current: Option<&str>,
    codex: Option<CodexAccess<'_>>,
) -> Option<(&'static str, String)> {
    let inferred_dnf = current == Some(Outcome::Dnf.as_str());
    let caught_up = copies.iter().any(|s| {
        s.read_state == ReadState::Completed
            || (s.read_state == ReadState::InProgress && s.unread_count == Some(0))
    });
    if caught_up {
        return inferred_dnf.then(|| (REOPENED, "Finished or caught up in Codex".to_owned()));
    }
    let reading: Vec<&&SnapshotSeries> = copies
        .iter()
        .filter(|s| s.read_state == ReadState::InProgress && s.unread_count.is_some_and(|n| n > 0))
        .collect();
    if reading.is_empty() {
        return None;
    }
    let last = last_progress(codex?, &reading).await?;
    if Utc::now() - last >= STALL_AFTER {
        Some((
            Outcome::Dnf.as_str(),
            format!(
                "No reading progress in Codex since {}",
                last.format("%Y-%m-%d")
            ),
        ))
    } else {
        inferred_dnf.then(|| {
            (
                REOPENED,
                format!("Reading resumed in Codex on {}", last.format("%Y-%m-%d")),
            )
        })
    }
}

/// When progress last moved on any of `series`, or `None` when Codex could
/// not say (a failed call, or no progress recorded at all).
async fn last_progress(
    codex: CodexAccess<'_>,
    series: &[&&SnapshotSeries],
) -> Option<DateTime<Utc>> {
    let mut last: Option<DateTime<Utc>> = None;
    for s in series {
        let books = match codex.client.series_books(codex.credential, &s.id).await {
            Ok(books) => books,
            Err(err) => {
                tracing::warn!(series = %s.id, error = %err, "book progress unavailable; skipping the stall check");
                return None;
            }
        };
        let newest = books
            .iter()
            .filter_map(|b| b.read_progress.as_ref().map(|p| p.updated_at))
            .max();
        last = last.max(newest);
    }
    last
}

/// The caller's series (every copy) carrying one of `identities`.
fn matching_series<'a>(
    snapshot: &'a LibrarySnapshot,
    identities: &'a HashSet<(Provider, String)>,
) -> impl Iterator<Item = &'a SnapshotSeries> {
    snapshot.series.iter().filter(|series| {
        series
            .external_ids
            .iter()
            .any(|id| identities.contains(&(id.provider.clone(), id.id.clone())))
    })
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
            authors: Vec::new(),
            book_count: 10,
            unread_count: Some(10),
            read_state,
            external_ids: vec![ExternalId::from_tsundoku(raw_id).unwrap()],
            rating: None,
            rating_notes: None,
            rating_updated_at: None,
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

        let first = reconcile_user(&db, "user-1", Some(&snapshot), None)
            .await
            .unwrap();
        assert_eq!(first.flagged, 1);
        let again = reconcile_user(&db, "user-1", Some(&snapshot), None)
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

        let pass = reconcile_user(&db, "user-1", Some(&snapshot), None)
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

        let cold = reconcile_user(&db, "user-1", None, None).await.unwrap();
        assert_eq!(cold.flagged, 0);

        // The next snapshot build (the user's next touch) flags it.
        let snapshot =
            LibrarySnapshot::from_series(vec![series("mangabaka:1", ReadState::Completed)]);
        let warm = reconcile_user(&db, "user-1", Some(&snapshot), None)
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
        reconcile_user(&db, "user-1", Some(&snapshot), None)
            .await
            .unwrap();

        verdicts::insert(&db, "user-1", feedback("mangabaka:1", "loved"))
            .await
            .unwrap();
        let pass = reconcile_user(&db, "user-1", Some(&snapshot), None)
            .await
            .unwrap();
        assert_eq!(pass.resolved, 1);
        assert_eq!(pass.flagged, 0);
        assert_eq!(
            reconciliation::pending_count(&db, "user-1").await.unwrap(),
            0
        );

        // Re-running after resolution neither re-flags nor re-resolves.
        let idle = reconcile_user(&db, "user-1", Some(&snapshot), None)
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

        let pass = reconcile_user(&db, "user-1", Some(&snapshot), None)
            .await
            .unwrap();
        assert_eq!(pass.flagged, 0);
    }

    /// A library of a high rater (76..=92) plus `extra`.
    fn rated_library(extra: Vec<SnapshotSeries>) -> LibrarySnapshot {
        let mut all: Vec<SnapshotSeries> = (0..=16)
            .map(|i| SnapshotSeries {
                rating: Some(76 + i),
                ..series(&format!("mangabaka:{}", 900 + i), ReadState::Completed)
            })
            .collect();
        all.extend(extra);
        LibrarySnapshot::from_series(all)
    }

    fn rated_at(raw_id: &str, rating: i32, at: chrono::DateTime<chrono::Utc>) -> SnapshotSeries {
        SnapshotSeries {
            rating: Some(rating),
            rating_updated_at: Some(at),
            ..series(raw_id, ReadState::Completed)
        }
    }

    async fn outcomes(db: &DatabaseConnection) -> Vec<(Option<String>, String)> {
        verdicts::list_for_user(db, "user-1")
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.outcome.is_some())
            .map(|r| (r.outcome, r.session_id))
            .collect()
    }

    #[tokio::test]
    async fn a_rating_after_the_recommendation_records_the_outcome_once() {
        let db = fresh_db().await;
        verdicts::insert(&db, "user-1", verdict("mangabaka:1"))
            .await
            .unwrap();
        let later = chrono::Utc::now() + chrono::Duration::hours(1);
        let snapshot = rated_library(vec![rated_at("mangabaka:1", 90, later)]);

        let pass = reconcile_user(&db, "user-1", Some(&snapshot), None)
            .await
            .unwrap();
        assert_eq!(pass.inferred, 1);
        assert_eq!(pass.flagged, 0, "an outcome answers the question");
        assert_eq!(
            outcomes(&db).await,
            vec![(Some("loved".into()), AUTO_FEEDBACK_SESSION.into())]
        );

        let again = reconcile_user(&db, "user-1", Some(&snapshot), None)
            .await
            .unwrap();
        assert_eq!(again.inferred, 0);
        assert_eq!(outcomes(&db).await.len(), 1);
    }

    #[tokio::test]
    async fn a_changed_rating_revises_the_inferred_outcome() {
        let db = fresh_db().await;
        verdicts::insert(&db, "user-1", verdict("mangabaka:1"))
            .await
            .unwrap();
        let later = chrono::Utc::now() + chrono::Duration::hours(1);
        for (rating, expected) in [(78, "liked"), (60, "mixed")] {
            let snapshot = rated_library(vec![rated_at("mangabaka:1", rating, later)]);
            reconcile_user(&db, "user-1", Some(&snapshot), None)
                .await
                .unwrap();
            let latest = outcomes(&db).await.pop().unwrap();
            assert_eq!(latest.0.as_deref(), Some(expected), "rated {rating}");
        }
    }

    #[tokio::test]
    async fn a_rating_older_than_the_recommendation_is_not_its_outcome() {
        let db = fresh_db().await;
        verdicts::insert(&db, "user-1", verdict("mangabaka:1"))
            .await
            .unwrap();
        let earlier = chrono::Utc::now() - chrono::Duration::days(30);
        let snapshot = rated_library(vec![rated_at("mangabaka:1", 95, earlier)]);

        reconcile_user(&db, "user-1", Some(&snapshot), None)
            .await
            .unwrap();
        assert!(outcomes(&db).await.is_empty());
    }

    #[tokio::test]
    async fn an_outcome_the_user_recorded_is_never_followed_by_an_inferred_one() {
        let db = fresh_db().await;
        verdicts::insert(&db, "user-1", verdict("mangabaka:1"))
            .await
            .unwrap();
        verdicts::insert(&db, "user-1", feedback("mangabaka:1", "dropped"))
            .await
            .unwrap();
        let later = chrono::Utc::now() + chrono::Duration::hours(1);
        let snapshot = rated_library(vec![rated_at("mangabaka:1", 95, later)]);

        let pass = reconcile_user(&db, "user-1", Some(&snapshot), None)
            .await
            .unwrap();
        assert_eq!(pass.inferred, 0);
        assert_eq!(outcomes(&db).await.len(), 1);
    }
}
