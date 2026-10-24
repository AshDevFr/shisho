//! Repository tests on in-memory SQLite: the append-only verdict log with
//! its effective-state fold, and the governed profile lifecycle.

use migration::{Migrator, MigratorTrait};
use sea_orm::{Database, DatabaseConnection, EntityTrait};
use shisho_db::entities::{profile_version, verdict};
use shisho_db::repos::profiles::{self, ProfileError};
use shisho_db::repos::verdicts::{self, ExclusionIndex, NewVerdict, RetractionReason};
use shisho_ids::ExternalId;

async fn fresh_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.expect("connect");
    Migrator::up(&db, None).await.expect("migrate");
    db
}

fn frieren() -> ExternalId {
    ExternalId::from_tsundoku("mangabaka:112092").expect("known provider")
}

/// A plain verdict row; `retracted`/`reason`/`outcome` model the three row
/// kinds.
fn row(
    id: &ExternalId,
    tier: &str,
    retracted: bool,
    reason: Option<RetractionReason>,
    outcome: Option<&str>,
) -> NewVerdict {
    NewVerdict {
        external_id: Some(id.clone()),
        session_id: "s-1".into(),
        tier: tier.into(),
        rationale: "test".into(),
        verified: !retracted && outcome.is_none(),
        retracted,
        retraction_reason: reason,
        outcome: outcome.map(str::to_owned),
        notes: None,
    }
}

#[tokio::test]
async fn verdicts_are_append_only_and_retraction_is_a_second_row() {
    let db = fresh_db().await;

    let first = verdicts::insert(&db, "user-1", row(&frieren(), "A", false, None, None))
        .await
        .expect("insert");
    assert_eq!(first.user_sub, "user-1");
    assert_eq!(first.mangabaka_id, Some(112092), "mangabaka id derived");
    assert_eq!(first.provider.as_deref(), Some("mangabaka"));
    assert_eq!(first.external_id.as_deref(), Some("112092"));

    // Retraction: a follow-up row, never an update.
    verdicts::insert(
        &db,
        "user-1",
        row(
            &frieren(),
            "A",
            true,
            Some(RetractionReason::FailedVerification),
            None,
        ),
    )
    .await
    .expect("insert retraction");

    let rows = verdict::Entity::find().all(&db).await.expect("all");
    assert_eq!(rows.len(), 2, "both rows persist; nothing ever deletes");
    assert!(!rows[0].retracted);
    assert_eq!(rows[0], first, "the original row is provably untouched");
    assert!(rows[1].retracted);
    assert_eq!(
        rows[1].retraction_reason.as_deref(),
        Some("failed_verification")
    );
}

#[tokio::test]
async fn list_and_find_are_scoped_to_the_user() {
    let db = fresh_db().await;
    let mine = verdicts::insert(&db, "user-1", row(&frieren(), "A", false, None, None))
        .await
        .expect("insert");
    let theirs = verdicts::insert(&db, "user-2", row(&frieren(), "S", false, None, None))
        .await
        .expect("insert");

    let listed = verdicts::list_for_user(&db, "user-1").await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, mine.id);

    // find_for_user refuses a row id belonging to another user.
    let cross = verdicts::find_for_user(&db, "user-1", theirs.id)
        .await
        .expect("find");
    assert!(cross.is_none());
    let own = verdicts::find_for_user(&db, "user-1", mine.id)
        .await
        .expect("find");
    assert_eq!(own.map(|r| r.id), Some(mine.id));
}

#[tokio::test]
async fn non_mangabaka_identity_stores_the_pair_without_a_mangabaka_id() {
    let db = fresh_db().await;
    let mal = ExternalId::from_tsundoku("mal:126287").expect("known");
    let row = verdicts::insert(&db, "user-1", row(&mal, "B", false, None, None))
        .await
        .expect("insert");
    assert_eq!(row.mangabaka_id, None);
    assert_eq!(row.provider.as_deref(), Some("mal"));
}

/// Property-style check: every interleaving (length ≤ 3) of the four row
/// kinds yields the documented effective state, via both the per-series
/// fold and the exclusion index.
#[tokio::test]
async fn every_lifecycle_interleaving_folds_deterministically() {
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Op {
        Verdict,          // non-retracted, tier "A"
        RetractFailed,    // retracted, failed_verification
        RetractWithdrawn, // retracted, withdrawn
        Feedback,         // outcome "loved"
    }
    use Op::*;
    let ops = [Verdict, RetractFailed, RetractWithdrawn, Feedback];

    // Every sequence of length 0..=3.
    let mut sequences: Vec<Vec<Op>> = vec![vec![]];
    for len in 1..=3usize {
        for n in 0..ops.len().pow(len as u32) {
            let mut seq = Vec::with_capacity(len);
            let mut k = n;
            for _ in 0..len {
                seq.push(ops[k % ops.len()]);
                k /= ops.len();
            }
            sequences.push(seq);
        }
    }

    // One shared DB; each sequence gets its own series identity.
    let db = fresh_db().await;
    for (i, seq) in sequences.iter().enumerate() {
        let id = ExternalId::from_tsundoku(&format!("mangabaka:{}", 1000 + i)).expect("known");
        for op in seq {
            let new = match op {
                Verdict => row(&id, "A", false, None, None),
                RetractFailed => row(
                    &id,
                    "A",
                    true,
                    Some(RetractionReason::FailedVerification),
                    None,
                ),
                RetractWithdrawn => row(&id, "A", true, Some(RetractionReason::Withdrawn), None),
                Feedback => row(&id, "feedback", false, None, Some("loved")),
            };
            verdicts::insert(&db, "user-1", new).await.expect("insert");
        }
    }

    let index = ExclusionIndex::load(&db, "user-1").await.expect("index");
    for (i, seq) in sequences.iter().enumerate() {
        let id = ExternalId::from_tsundoku(&format!("mangabaka:{}", 1000 + i)).expect("known");
        let state = verdicts::effective_state(&db, "user-1", [&id])
            .await
            .expect("state");

        // Reference model, straight from the documented rules.
        let any_feedback = seq.contains(&Feedback);
        let last_verdict_op = seq.iter().rev().find(|op| !matches!(op, Feedback));
        let expect_excluded =
            any_feedback || matches!(last_verdict_op, Some(Verdict) | Some(RetractFailed));
        let expect_tier = seq
            .iter()
            .rev()
            .find(|op| matches!(op, Verdict))
            .map(|_| "A".to_owned());

        assert_eq!(
            state.excludes(),
            expect_excluded,
            "exclusion for {seq:?}: {state:?}"
        );
        assert_eq!(
            index.excludes([&id]),
            expect_excluded,
            "index disagrees with fold for {seq:?}"
        );
        assert_eq!(state.last_tier, expect_tier, "tier for {seq:?}");
        assert_eq!(
            state.outcome,
            any_feedback.then(|| "loved".to_owned()),
            "outcome for {seq:?}"
        );
        assert_eq!(state.history_len, seq.len(), "history_len for {seq:?}");
        assert_eq!(
            state.retracted,
            matches!(
                last_verdict_op,
                Some(RetractFailed) | Some(RetractWithdrawn)
            ),
            "retracted for {seq:?}"
        );
    }
}

#[tokio::test]
async fn legacy_retractions_without_a_reason_read_as_withdrawn() {
    let db = fresh_db().await;
    verdicts::insert(&db, "user-1", row(&frieren(), "A", false, None, None))
        .await
        .expect("insert");
    // A pre-column retraction row: retracted with no reason.
    verdicts::insert(&db, "user-1", row(&frieren(), "A", true, None, None))
        .await
        .expect("insert legacy retraction");

    let state = verdicts::effective_state(&db, "user-1", [&frieren()])
        .await
        .expect("state");
    assert_eq!(state.retraction_reason, Some(RetractionReason::Withdrawn));
    assert!(!state.excludes(), "legacy retraction restores candidacy");
}

#[tokio::test]
async fn exclusion_index_is_per_user_and_keyed_on_both_identities() {
    let db = fresh_db().await;
    let mal = ExternalId::from_tsundoku("mal:126287").expect("known");

    verdicts::insert(&db, "user-1", row(&frieren(), "A", false, None, None))
        .await
        .expect("insert");
    verdicts::insert(&db, "user-1", row(&mal, "B", false, None, None))
        .await
        .expect("insert");

    let index = ExclusionIndex::load(&db, "user-1").await.expect("index");
    assert!(index.excludes([&frieren()]), "mangabaka identity");
    assert!(index.excludes([&mal]), "(provider, id) pair identity");

    let other = ExclusionIndex::load(&db, "user-2").await.expect("index");
    assert!(!other.excludes([&frieren()]), "exclusion is per-user");
}

#[tokio::test]
async fn recent_outcomes_filters_to_feedback_rows_since_a_timestamp() {
    let db = fresh_db().await;
    verdicts::insert(&db, "user-1", row(&frieren(), "A", false, None, None))
        .await
        .expect("verdict row");
    let early = verdicts::insert(
        &db,
        "user-1",
        row(&frieren(), "feedback", false, None, Some("loved")),
    )
    .await
    .expect("early feedback");

    let all = verdicts::recent_outcomes(&db, "user-1", None)
        .await
        .expect("all outcomes");
    assert_eq!(all.len(), 1, "verdict rows are not outcomes");
    assert_eq!(all[0].outcome.as_deref(), Some("loved"));

    // Nothing newer than the early row's own timestamp.
    let since = verdicts::recent_outcomes(&db, "user-1", Some(early.created_at.to_utc()))
        .await
        .expect("since");
    assert!(since.is_empty());
}

#[tokio::test]
async fn new_user_profile_is_a_well_defined_empty_default() {
    let db = fresh_db().await;
    let profile = profiles::get_or_default(&db, "user-1").await.expect("get");
    assert_eq!(profile.version, 0);
    assert_eq!(profile.content, "");
    assert!(profile.updated_at.is_none());
}

#[tokio::test]
async fn propose_then_approve_makes_the_proposal_current() {
    let db = fresh_db().await;

    let proposal = profiles::propose(&db, "user-1", "# Taste\n- fantasy", "draft", Some(0))
        .await
        .expect("propose");
    assert_eq!(proposal.version, 1);
    assert_eq!(proposal.base_version, 0);

    // Pending: the current profile is untouched.
    let current = profiles::get_or_default(&db, "user-1").await.expect("get");
    assert_eq!(current.version, 0);
    assert_eq!(current.content, "");

    let approved = profiles::approve(&db, "user-1", 1).await.expect("approve");
    assert_eq!(approved.version, 1);
    assert_eq!(approved.content, "# Taste\n- fantasy");

    let current = profiles::get_or_default(&db, "user-1").await.expect("get");
    assert_eq!(current.version, 1, "approval bumps version and content");
    assert_eq!(current.content, "# Taste\n- fantasy");
    assert!(current.updated_at.is_some());

    let history = profiles::versions(&db, "user-1").await.expect("versions");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, "applied");
}

#[tokio::test]
async fn propose_then_reject_leaves_the_profile_alone() {
    let db = fresh_db().await;
    profiles::propose(&db, "user-1", "content", "draft", None)
        .await
        .expect("propose");
    profiles::reject(&db, "user-1", 1).await.expect("reject");

    let current = profiles::get_or_default(&db, "user-1").await.expect("get");
    assert_eq!(current.version, 0);

    let history = profiles::versions(&db, "user-1").await.expect("versions");
    assert_eq!(history[0].status, "rejected");

    // A closed proposal cannot be approved later.
    let err = profiles::approve(&db, "user-1", 1)
        .await
        .expect_err("not pending");
    assert!(matches!(err, ProfileError::NotPending { .. }), "{err}");
}

#[tokio::test]
async fn stale_base_proposal_is_rejected_naming_the_current_version() {
    let db = fresh_db().await;
    profiles::edit_direct(&db, "user-1", "v1 content", "initial", None)
        .await
        .expect("edit");

    let err = profiles::propose(&db, "user-1", "stale", "built on v0", Some(0))
        .await
        .expect_err("stale base");
    assert!(
        matches!(
            err,
            ProfileError::StaleBase {
                base: 0,
                current: 1
            }
        ),
        "{err}"
    );
    assert!(
        err.to_string().contains("version 1"),
        "the error names the current version: {err}"
    );
}

#[tokio::test]
async fn approval_of_a_proposal_whose_base_moved_is_refused() {
    let db = fresh_db().await;
    profiles::propose(&db, "user-1", "proposal content", "draft", Some(0))
        .await
        .expect("propose");
    // A direct edit lands while the proposal sits in review.
    profiles::edit_direct(&db, "user-1", "edited meanwhile", "direct edit", None)
        .await
        .expect("edit");

    let err = profiles::approve(&db, "user-1", 1)
        .await
        .expect_err("stale");
    assert!(
        matches!(
            err,
            ProfileError::StaleBase {
                base: 0,
                current: 2
            }
        ),
        "{err}"
    );
}

#[tokio::test]
async fn a_second_proposal_supersedes_the_pending_one() {
    let db = fresh_db().await;
    profiles::propose(&db, "user-1", "first", "first draft", None)
        .await
        .expect("propose");
    let second = profiles::propose(&db, "user-1", "second", "second draft", None)
        .await
        .expect("propose again");
    assert_eq!(second.version, 2);
    assert_eq!(second.superseded_version, Some(1));

    let history = profiles::versions(&db, "user-1").await.expect("versions");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].version, 2);
    assert_eq!(history[0].status, "pending");
    assert_eq!(history[1].status, "superseded");

    // Only the latest is approvable.
    let err = profiles::approve(&db, "user-1", 1)
        .await
        .expect_err("superseded");
    assert!(matches!(err, ProfileError::NotPending { .. }), "{err}");
    profiles::approve(&db, "user-1", 2)
        .await
        .expect("approve latest");
    let current = profiles::get_or_default(&db, "user-1").await.expect("get");
    assert_eq!(current.content, "second");
}

#[tokio::test]
async fn direct_edits_are_versioned_and_apply_immediately() {
    let db = fresh_db().await;
    let view = profiles::edit_direct(&db, "user-1", "# Taste", "manual fix", None)
        .await
        .expect("edit");
    assert_eq!(view.version, 1);

    let current = profiles::get_or_default(&db, "user-1").await.expect("get");
    assert_eq!(current.content, "# Taste");

    let history = profiles::versions(&db, "user-1").await.expect("versions");
    assert_eq!(history[0].status, "applied");
    assert_eq!(history[0].change_summary, "manual fix");
}

#[tokio::test]
async fn direct_edit_with_a_stale_base_is_refused() {
    let db = fresh_db().await;
    profiles::edit_direct(&db, "user-1", "v1", "initial", None)
        .await
        .expect("edit");
    // An edit drafted against v0 after v1 landed must not clobber.
    let err = profiles::edit_direct(&db, "user-1", "stale overwrite", "late edit", Some(0))
        .await
        .expect_err("stale base");
    assert!(
        matches!(
            err,
            ProfileError::StaleBase {
                base: 0,
                current: 1
            }
        ),
        "{err}"
    );
    let current = profiles::get_or_default(&db, "user-1").await.expect("get");
    assert_eq!(current.content, "v1", "the stale edit must not apply");
}

#[tokio::test]
async fn rendered_history_shows_unified_diffs_against_the_base() {
    let db = fresh_db().await;
    profiles::edit_direct(&db, "user-1", "- likes fantasy\n", "v1", None)
        .await
        .expect("edit");
    profiles::propose(
        &db,
        "user-1",
        "- likes fantasy\n- slow burn\n",
        "add pacing",
        Some(1),
    )
    .await
    .expect("propose");

    let history = profiles::rendered_history(&db, "user-1")
        .await
        .expect("history");
    assert_eq!(history.len(), 2);
    let proposal = &history[0];
    assert_eq!(proposal.version, 2);
    assert_eq!(proposal.base_version, 1);
    assert!(
        proposal.diff.contains("+- slow burn"),
        "the diff shows the addition: {}",
        proposal.diff
    );
    assert!(
        !proposal.diff.contains("-- likes fantasy"),
        "unchanged lines are not removals: {}",
        proposal.diff
    );
    let initial = &history[1];
    assert!(
        initial.diff.contains("+- likes fantasy"),
        "v1 diffs against the empty v0: {}",
        initial.diff
    );
}

#[tokio::test]
async fn versions_are_per_user() {
    let db = fresh_db().await;
    profiles::propose(&db, "user-1", "a", "draft", None)
        .await
        .expect("propose");
    let other = profiles::propose(&db, "user-2", "b", "other user", None)
        .await
        .expect("propose");
    assert_eq!(other.version, 1);
    assert_eq!(
        profile_version::Entity::find()
            .all(&db)
            .await
            .expect("all")
            .len(),
        2
    );
}

#[tokio::test]
async fn a_reopened_row_clears_the_folded_outcome() {
    let db = fresh_db().await;
    for r in [
        row(&frieren(), "A", false, None, None),
        row(&frieren(), "feedback", false, None, Some("dnf")),
        row(
            &frieren(),
            "feedback",
            false,
            None,
            Some(verdicts::REOPENED),
        ),
    ] {
        verdicts::insert(&db, "user-1", r).await.expect("insert");
    }
    let rows = verdict::Entity::find().all(&db).await.unwrap();
    let state = verdicts::fold(&rows);
    assert_eq!(state.outcome, None);
    assert!(
        state.has_verdict && !state.retracted,
        "the recommendation stands"
    );
}
