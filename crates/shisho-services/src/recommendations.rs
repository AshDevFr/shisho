//! The caller's current recommendations: the verdict log re-evaluated per
//! series against itself and the live library, which is what Home renders.
//!
//! One entry per series, across every session: the latest standing verdict.
//! A series drops off once it has an outcome (recorded or inferred), when its
//! latest verdict is `pass`, or when it was retracted. A standing series that
//! shows read progress in Codex is *reading* (it waits for an outcome) rather
//! than *recommended*. The grouping and the fold are the ones candidacy uses,
//! so Home and the candidate pools never disagree about what is settled.

use std::cmp::Reverse;
use std::collections::HashMap;

use sea_orm::{DatabaseConnection, DbErr};

use crate::library_snapshot::LibrarySnapshot;
use crate::read_state::ReadState;
use crate::reconciliation::{identity_keys, matching_series};
use shisho_db::entities::verdict;
use shisho_db::repos::verdicts::{self, SeriesKey};

/// The tier a session gives a series it decided not to recommend.
const PASS: &str = "pass";

/// One standing recommendation.
#[derive(Debug, Clone)]
pub struct Current {
    /// The latest non-retracted verdict row: its id is what an outcome is
    /// recorded against, its tier and rationale are what Home shows.
    pub verdict: verdict::Model,
    /// Verdict rows (not feedback) the series has across all sessions.
    pub times_recommended: usize,
    /// Whether the caller owns a copy (an acquirable pick bought since).
    pub in_library: bool,
    /// Books read and total across owned copies, when the snapshot has them.
    pub books_read: Option<i64>,
    pub book_count: Option<i64>,
}

/// The re-evaluated list, already ordered for display.
#[derive(Debug, Clone, Default)]
pub struct CurrentRecommendations {
    /// Standing and not started.
    pub recommended: Vec<Current>,
    /// Standing, with read progress in Codex, and no outcome yet.
    pub reading: Vec<Current>,
}

/// Re-evaluate `user_sub`'s log. Without a `snapshot` nothing is known about
/// ownership or read progress, so every standing series is `recommended`.
pub async fn current(
    db: &DatabaseConnection,
    user_sub: &str,
    snapshot: Option<&LibrarySnapshot>,
) -> Result<CurrentRecommendations, DbErr> {
    let rows = verdicts::list_for_user(db, user_sub).await?;
    let mut groups: HashMap<SeriesKey, Vec<verdict::Model>> = HashMap::new();
    for row in rows {
        if let Some(key) = SeriesKey::of(&row) {
            groups.entry(key).or_default().push(row);
        }
    }

    let mut out = CurrentRecommendations::default();
    for rows in groups.into_values() {
        let state = verdicts::fold(&rows);
        if state.outcome.is_some() || !state.has_verdict || state.retracted {
            continue;
        }
        if state
            .last_tier
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(PASS))
        {
            continue;
        }
        let Some(standing) = rows
            .iter()
            .rev()
            .find(|r| r.outcome.is_none() && !r.retracted)
            .cloned()
        else {
            continue;
        };
        let times_recommended = rows.iter().filter(|r| r.outcome.is_none()).count();

        let mut entry = Current {
            verdict: standing,
            times_recommended,
            in_library: false,
            books_read: None,
            book_count: None,
        };
        let mut started = false;
        if let Some(snapshot) = snapshot {
            let identities = identity_keys(&rows);
            for copy in matching_series(snapshot, &identities) {
                entry.in_library = true;
                started |= copy.read_state != ReadState::Untouched;
                let read = copy.unread_count.map(|unread| copy.book_count - unread);
                // Several copies: report the one furthest along.
                if read > entry.books_read {
                    entry.books_read = read;
                    entry.book_count = Some(copy.book_count);
                } else if entry.book_count.is_none() {
                    entry.book_count = Some(copy.book_count);
                }
            }
        }
        if started {
            out.reading.push(entry);
        } else {
            out.recommended.push(entry);
        }
    }

    out.recommended.sort_by_key(display_order);
    out.reading
        .sort_by_key(|c| Reverse((c.verdict.created_at, c.verdict.id)));
    Ok(out)
}

/// S, A, B, C, D, then any other tier; newest recommendation first within one.
fn display_order(c: &Current) -> (usize, Reverse<(chrono::DateTime<chrono::FixedOffset>, i64)>) {
    const TIERS: [&str; 5] = ["S", "A", "B", "C", "D"];
    let rank = TIERS
        .iter()
        .position(|t| t.eq_ignore_ascii_case(&c.verdict.tier))
        .unwrap_or(TIERS.len());
    (rank, Reverse((c.verdict.created_at, c.verdict.id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use migration::{Migrator, MigratorTrait};
    use sea_orm::Database;

    use crate::library_snapshot::SnapshotSeries;
    use crate::verdicts::record_feedback;
    use shisho_db::repos::verdicts::{NewVerdict, RetractionReason};
    use shisho_ids::ExternalId;

    const USER: &str = "user-1";

    async fn fresh_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.expect("connect");
        Migrator::up(&db, None).await.expect("migrate");
        db
    }

    fn id(raw: &str) -> ExternalId {
        ExternalId::from_tsundoku(raw).unwrap()
    }

    fn verdict(raw: &str, session: &str, tier: &str) -> NewVerdict {
        NewVerdict {
            external_id: Some(id(raw)),
            session_id: session.into(),
            tier: tier.into(),
            rationale: "fits the profile".into(),
            verified: true,
            retracted: false,
            retraction_reason: None,
            outcome: None,
            notes: None,
        }
    }

    async fn add(db: &DatabaseConnection, v: NewVerdict) {
        verdicts::insert(db, USER, v).await.expect("insert");
    }

    fn owned(raw: &str, unread: i64) -> SnapshotSeries {
        SnapshotSeries {
            id: format!("uuid-{raw}"),
            library_id: "lib-1".into(),
            library_name: "Manga".into(),
            title: format!("Series {raw}"),
            summary: None,
            status: None,
            year: None,
            genres: Vec::new(),
            tags: Vec::new(),
            authors: Vec::new(),
            book_count: 10,
            unread_count: Some(unread),
            read_state: ReadState::derive(10, Some(unread)),
            external_ids: vec![id(raw)],
            rating: None,
            rating_notes: None,
            rating_updated_at: None,
        }
    }

    fn tokens(list: &[Current]) -> Vec<String> {
        list.iter()
            .map(|c| verdicts::verdict_series_token(&c.verdict).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn settled_series_drop_off() {
        let db = fresh_db().await;
        add(&db, verdict("mangabaka:1", "s1", "A")).await;
        add(&db, verdict("mangabaka:2", "s1", "pass")).await;
        add(
            &db,
            NewVerdict {
                retracted: true,
                retraction_reason: Some(RetractionReason::FailedVerification),
                ..verdict("mangabaka:3", "s1", "S")
            },
        )
        .await;
        add(&db, verdict("mangabaka:4", "s1", "B")).await;
        record_feedback(&db, USER, &id("mangabaka:4"), "loved", None)
            .await
            .unwrap();

        let current = current(&db, USER, None).await.unwrap();
        assert_eq!(tokens(&current.recommended), ["mangabaka:1"]);
        assert!(current.reading.is_empty());
    }

    #[tokio::test]
    async fn a_series_recommended_twice_appears_once_with_the_newer_tier() {
        let db = fresh_db().await;
        add(&db, verdict("mangabaka:1", "s1", "B")).await;
        add(&db, verdict("mangabaka:1", "s2", "A")).await;

        let current = current(&db, USER, None).await.unwrap();
        assert_eq!(current.recommended.len(), 1);
        assert_eq!(current.recommended[0].verdict.tier, "A");
        assert_eq!(current.recommended[0].times_recommended, 2);
    }

    #[tokio::test]
    async fn started_series_move_to_reading_and_owned_ones_are_flagged() {
        let db = fresh_db().await;
        add(&db, verdict("mangabaka:1", "s1", "A")).await;
        add(&db, verdict("mangabaka:2", "s1", "A")).await;
        add(&db, verdict("mangabaka:3", "s1", "A")).await;
        let snapshot = LibrarySnapshot::from_series(vec![
            owned("mangabaka:1", 7),  // three books read
            owned("mangabaka:2", 10), // bought, untouched
        ]);

        let current = current(&db, USER, Some(&snapshot)).await.unwrap();
        assert_eq!(tokens(&current.reading), ["mangabaka:1"]);
        assert_eq!(current.reading[0].books_read, Some(3));
        assert_eq!(current.reading[0].book_count, Some(10));

        let by_token: HashMap<String, bool> = tokens(&current.recommended)
            .into_iter()
            .zip(current.recommended.iter().map(|c| c.in_library))
            .collect();
        assert_eq!(by_token.get("mangabaka:2"), Some(&true));
        assert_eq!(by_token.get("mangabaka:3"), Some(&false));
    }

    #[tokio::test]
    async fn ordered_by_tier_then_newest() {
        let db = fresh_db().await;
        add(&db, verdict("mangabaka:1", "s1", "B")).await;
        add(&db, verdict("mangabaka:2", "s1", "S")).await;
        add(&db, verdict("mangabaka:3", "s1", "maybe")).await;
        add(&db, verdict("mangabaka:4", "s2", "B")).await;
        add(&db, verdict("mangabaka:5", "s2", "a")).await;

        let current = current(&db, USER, None).await.unwrap();
        assert_eq!(
            tokens(&current.recommended),
            [
                "mangabaka:2",
                "mangabaka:5",
                "mangabaka:4",
                "mangabaka:1",
                "mangabaka:3"
            ]
        );
    }

    #[tokio::test]
    async fn reopening_after_not_interested_restores_the_series() {
        let db = fresh_db().await;
        add(&db, verdict("mangabaka:1", "s1", "A")).await;

        record_feedback(&db, USER, &id("mangabaka:1"), "not interested", None)
            .await
            .unwrap();
        assert!(
            current(&db, USER, None)
                .await
                .unwrap()
                .recommended
                .is_empty()
        );

        let undo = record_feedback(&db, USER, &id("mangabaka:1"), "reopened", None)
            .await
            .unwrap();
        assert_eq!(undo.row.outcome.as_deref(), Some("reopened"));
        let outcomes: Vec<Option<String>> = verdicts::list_for_user(&db, USER)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.outcome)
            .collect();
        assert_eq!(
            outcomes,
            [None, Some("not_interested".into()), Some("reopened".into())]
        );
        assert_eq!(
            tokens(&current(&db, USER, None).await.unwrap().recommended),
            ["mangabaka:1"]
        );
    }
}
