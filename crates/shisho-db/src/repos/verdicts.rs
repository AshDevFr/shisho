//! The verdict log and its effective-state fold — the system's memory
//! model (every candidate pool, `lookup`, the UI, and reconciliation read
//! it through this module).
//!
//! # Lifecycle
//!
//! The log is append-only: retraction and feedback are new rows; nothing
//! here updates or deletes (and no such function may be added). A series'
//! effective state for a user is a fold over its rows in insertion order:
//!
//! - **Verdict rows** (`outcome = NULL`): the latest one wins. Its
//!   `retracted` flag is the effective retraction state; `tier` and
//!   `verified` come from the latest *non-retracted* verdict row.
//! - **Retraction rows** are verdict rows with `retracted = true` and a
//!   [`RetractionReason`]: `failed_verification` means the series was
//!   verified and rejected; `withdrawn` means a session's surplus was
//!   taken back. Rows predating the reason column read as `withdrawn`
//!   (the semantics retraction had then).
//! - **Feedback rows** (`outcome` set) record the post-read reaction; the
//!   latest outcome wins.
//!
//! # Exclusion (anti-re-litigation)
//!
//! A series is excluded from candidate pools iff its effective state says
//! the question is settled:
//!
//! - any recorded outcome excludes (the user read it);
//! - otherwise a standing (non-retracted) latest verdict excludes
//!   (already recommended);
//! - otherwise a latest verdict retracted as `failed_verification`
//!   excludes (verified-and-rejected is never re-litigated);
//! - a `withdrawn` retraction restores candidacy.
//!
//! Exclusion never hides history: `history` returns every row regardless.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use sea_orm::sea_query::Condition;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder,
};
use serde::{Deserialize, Serialize};

use crate::entities::verdict;
use shisho_ids::{ExternalId, Provider};

/// Why a retraction was recorded. The distinction drives exclusion: a
/// failed verification settles the series for good, a withdrawal does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RetractionReason {
    /// Verified and rejected (allergen, red flag): stays excluded.
    FailedVerification,
    /// A session's surplus taken back: returns to candidacy.
    Withdrawn,
}

impl RetractionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RetractionReason::FailedVerification => "failed_verification",
            RetractionReason::Withdrawn => "withdrawn",
        }
    }

    /// Decode a stored value. `NULL` and unknown values read as
    /// `Withdrawn`: rows predating the column carried no reason, and
    /// candidacy-restoring is the conservative reading (a wrongly restored
    /// series gets re-verified; a wrongly excluded one is lost silently).
    pub fn from_db(raw: Option<&str>) -> Self {
        match raw {
            Some("failed_verification") => RetractionReason::FailedVerification,
            _ => RetractionReason::Withdrawn,
        }
    }
}

/// One verdict to append. Identity is the canonical external ID; the
/// MangaBaka numeric ID is derived from it when applicable.
#[derive(Debug, Clone)]
pub struct NewVerdict {
    pub external_id: Option<ExternalId>,
    pub session_id: String,
    pub tier: String,
    pub rationale: String,
    pub verified: bool,
    pub retracted: bool,
    pub retraction_reason: Option<RetractionReason>,
    pub outcome: Option<String>,
    pub notes: Option<String>,
}

pub async fn insert(
    db: &DatabaseConnection,
    user_sub: &str,
    new: NewVerdict,
) -> Result<verdict::Model, sea_orm::DbErr> {
    let mangabaka_id = new.external_id.as_ref().and_then(|id| {
        (id.provider == Provider::Mangabaka)
            .then(|| id.id.parse::<i64>().ok())
            .flatten()
    });

    verdict::ActiveModel {
        user_sub: Set(user_sub.to_owned()),
        mangabaka_id: Set(mangabaka_id),
        provider: Set(new.external_id.as_ref().map(|id| id.provider.to_string())),
        external_id: Set(new.external_id.as_ref().map(|id| id.id.clone())),
        session_id: Set(new.session_id),
        tier: Set(new.tier),
        rationale: Set(new.rationale),
        verified: Set(new.verified),
        retracted: Set(new.retracted),
        retraction_reason: Set(new.retraction_reason.map(|r| r.as_str().to_owned())),
        outcome: Set(new.outcome),
        notes: Set(new.notes),
        created_at: Set(chrono::Utc::now().into()),
        ..Default::default()
    }
    .insert(db)
    .await
}

/// The effective state of one series for one user: the fold the module
/// docs define. Deterministic for any interleaving of rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerdictState {
    /// Tier of the latest non-retracted verdict row.
    pub last_tier: Option<String>,
    /// `verified` of the latest non-retracted verdict row.
    pub verified: bool,
    /// Whether the latest verdict row is a retraction.
    pub retracted: bool,
    /// The latest retraction's reason (set iff `retracted`).
    pub retraction_reason: Option<RetractionReason>,
    /// The latest recorded outcome, if any feedback row exists.
    pub outcome: Option<String>,
    /// Session of the latest row of any kind.
    pub last_session: Option<String>,
    /// Total rows folded (retracted and feedback included).
    pub history_len: usize,
    /// Whether any verdict (non-feedback) row exists.
    pub has_verdict: bool,
}

impl VerdictState {
    /// The anti-re-litigation rule (see module docs).
    pub fn excludes(&self) -> bool {
        if self.outcome.is_some() {
            return true;
        }
        if !self.has_verdict {
            return false;
        }
        if !self.retracted {
            return true;
        }
        self.retraction_reason == Some(RetractionReason::FailedVerification)
    }
}

/// Fold rows (insertion order) into the effective state.
pub fn fold(rows: &[verdict::Model]) -> VerdictState {
    let mut state = VerdictState {
        history_len: rows.len(),
        ..Default::default()
    };
    for row in rows {
        state.last_session = Some(row.session_id.clone());
        if row.outcome.is_some() {
            state.outcome = row.outcome.clone();
            continue;
        }
        state.has_verdict = true;
        state.retracted = row.retracted;
        state.retraction_reason = row
            .retracted
            .then(|| RetractionReason::from_db(row.retraction_reason.as_deref()));
        if !row.retracted {
            state.last_tier = Some(row.tier.clone());
            state.verified = row.verified;
        }
    }
    state
}

/// How verdict rows group into a series: by MangaBaka ID when present,
/// else by the `(provider, external_id)` pair. Rows with neither carry no
/// identity and belong to no series. One definition shared by the
/// exclusion index and the UI's per-series state badges.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SeriesKey {
    Mangabaka(i64),
    Pair(String, String),
}

impl SeriesKey {
    pub fn of(row: &verdict::Model) -> Option<Self> {
        match (row.mangabaka_id, &row.provider, &row.external_id) {
            (Some(id), _, _) => Some(SeriesKey::Mangabaka(id)),
            (None, Some(p), Some(e)) => Some(SeriesKey::Pair(p.clone(), e.clone())),
            _ => None,
        }
    }

    /// The `provider:id` token this key renders as on every surface.
    pub fn token(&self) -> String {
        match self {
            SeriesKey::Mangabaka(id) => format!("mangabaka:{id}"),
            SeriesKey::Pair(provider, external) => format!("{provider}:{external}"),
        }
    }
}

/// The series token of a verdict row (`mangabaka:<id>` when the row carries a
/// MangaBaka id, else its `provider:id`), so the MCP tools and the web UI name
/// the same verdict the same way.
pub fn verdict_series_token(row: &verdict::Model) -> Option<String> {
    SeriesKey::of(row).map(|key| key.token())
}

/// The caller's full verdict log, oldest first — the web UI's working set
/// (filtering, session grouping, and per-series folds happen in memory;
/// a single user's log is small by construction).
pub async fn list_for_user(
    db: &DatabaseConnection,
    user_sub: &str,
) -> Result<Vec<verdict::Model>, sea_orm::DbErr> {
    verdict::Entity::find()
        .filter(verdict::Column::UserSub.eq(user_sub))
        .order_by_asc(verdict::Column::CreatedAt)
        .order_by_asc(verdict::Column::Id)
        .all(db)
        .await
}

/// One verdict row — only if it belongs to `user_sub` (the UI's outcome
/// correction must never reach across users).
pub async fn find_for_user(
    db: &DatabaseConnection,
    user_sub: &str,
    id: i64,
) -> Result<Option<verdict::Model>, sea_orm::DbErr> {
    verdict::Entity::find_by_id(id)
        .filter(verdict::Column::UserSub.eq(user_sub))
        .one(db)
        .await
}

/// The effective state of one series, matched on any of its external
/// identities.
pub async fn effective_state<'a>(
    db: &DatabaseConnection,
    user_sub: &str,
    ids: impl IntoIterator<Item = &'a ExternalId>,
) -> Result<VerdictState, sea_orm::DbErr> {
    Ok(fold(&history(db, user_sub, ids).await?))
}

/// The caller's full verdict history (retracted included) for one series,
/// matched on any of its external identities, oldest first. Exclusion is
/// computed from this same order; it never hides rows.
pub async fn history<'a>(
    db: &DatabaseConnection,
    user_sub: &str,
    ids: impl IntoIterator<Item = &'a ExternalId>,
) -> Result<Vec<verdict::Model>, sea_orm::DbErr> {
    let mut identity = Condition::any();
    let mut empty = true;
    for id in ids {
        empty = false;
        if id.provider == Provider::Mangabaka
            && let Ok(n) = id.id.parse::<i64>()
        {
            identity = identity.add(verdict::Column::MangabakaId.eq(n));
        }
        identity = identity.add(
            Condition::all()
                .add(verdict::Column::Provider.eq(id.provider.to_string()))
                .add(verdict::Column::ExternalId.eq(id.id.clone())),
        );
    }
    if empty {
        return Ok(Vec::new());
    }

    verdict::Entity::find()
        .filter(verdict::Column::UserSub.eq(user_sub))
        .filter(identity)
        .order_by_asc(verdict::Column::CreatedAt)
        .order_by_asc(verdict::Column::Id)
        .all(db)
        .await
}

/// The caller's excluded series identities — every identity belonging to
/// a series whose effective state excludes it — loaded in one query.
///
/// Rows group into a series by MangaBaka ID when present, else by the
/// `(provider, external_id)` pair. Rows recorded under different
/// providers for the same series only merge when one carries the
/// MangaBaka ID of the other's group; matching at query time is by any
/// identity (`excludes` takes all of a candidate's IDs).
#[derive(Debug, Default)]
pub struct ExclusionIndex {
    mangabaka: HashSet<i64>,
    pairs: HashSet<(Provider, String)>,
}

impl ExclusionIndex {
    pub async fn load(db: &DatabaseConnection, user_sub: &str) -> Result<Self, sea_orm::DbErr> {
        let rows = verdict::Entity::find()
            .filter(verdict::Column::UserSub.eq(user_sub))
            .order_by_asc(verdict::Column::CreatedAt)
            .order_by_asc(verdict::Column::Id)
            .all(db)
            .await?;

        // Group rows into series, preserving insertion order per group.
        // Identity-less rows can't match a candidate; skip them.
        let mut groups: HashMap<SeriesKey, Vec<verdict::Model>> = HashMap::new();
        for row in rows {
            let Some(key) = SeriesKey::of(&row) else {
                continue;
            };
            groups.entry(key).or_default().push(row);
        }

        let mut index = Self::default();
        for rows in groups.values() {
            if !fold(rows).excludes() {
                continue;
            }
            for row in rows {
                if let Some(id) = row.mangabaka_id {
                    index.mangabaka.insert(id);
                }
                if let (Some(provider), Some(external_id)) = (&row.provider, &row.external_id) {
                    index
                        .pairs
                        .insert((Provider::from_tsundoku_token(provider), external_id.clone()));
                }
            }
        }
        Ok(index)
    }

    /// Does any of the series' external IDs belong to an excluded series?
    pub fn excludes<'a>(&self, ids: impl IntoIterator<Item = &'a ExternalId>) -> bool {
        ids.into_iter().any(|id| {
            if id.provider == Provider::Mangabaka
                && let Ok(n) = id.id.parse::<i64>()
                && self.mangabaka.contains(&n)
            {
                return true;
            }
            self.pairs.contains(&(id.provider.clone(), id.id.clone()))
        })
    }
}

/// Feedback rows (recorded outcomes) since `since`, oldest first — the
/// evidence feeding a consolidation session. `None` means all of them
/// (no consolidation has been applied yet).
pub async fn recent_outcomes(
    db: &DatabaseConnection,
    user_sub: &str,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<verdict::Model>, sea_orm::DbErr> {
    let mut query = verdict::Entity::find()
        .filter(verdict::Column::UserSub.eq(user_sub))
        .filter(verdict::Column::Outcome.is_not_null());
    if let Some(since) = since {
        query = query.filter(verdict::Column::CreatedAt.gt(since));
    }
    query
        .order_by_asc(verdict::Column::CreatedAt)
        .order_by_asc(verdict::Column::Id)
        .all(db)
        .await
}
