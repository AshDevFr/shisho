//! The governed taste profile: a current row per user plus an append-only
//! version history: the taste profile is never silently mutated.
//!
//! # Lifecycle
//!
//! - `propose` records a **pending** version row; the current profile is
//!   untouched. One pending proposal per user: a new proposal supersedes
//!   the previous pending one (latest intent wins; the superseded row
//!   stays in history with status `superseded`).
//! - `approve` makes a pending row current — the only transition that
//!   changes the profile from a proposal. It refuses a stale base: the
//!   proposal must have been drafted against the version that is still
//!   current. `reject` closes a pending row without applying it.
//! - `edit_direct` is the UI path: applies immediately, still versioned.
//!
//! Version content is immutable once written; `status` is the one mutable
//! workflow column (`pending` → `applied` | `rejected` | `superseded`).
//! Approval is deliberately **not** an MCP tool: a chat session can
//! propose but never apply (the review affordance is the web UI, or this
//! repo API for out-of-band administration).

use chrono::{DateTime, Utc};
use sea_orm::sea_query::{Expr, Func};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection,
    EntityTrait, QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
};
use serde::Serialize;

use crate::entities::{profile, profile_version};

/// Proposal workflow states (the `profile_versions.status` column).
pub mod status {
    pub const PENDING: &str = "pending";
    pub const APPLIED: &str = "applied";
    pub const REJECTED: &str = "rejected";
    pub const SUPERSEDED: &str = "superseded";
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    /// The proposal's base is no longer the current version (a direct
    /// edit or another approval landed in between).
    #[error(
        "proposal base version {base} is stale: the current profile is version {current}; \
         re-read the profile and propose against version {current}"
    )]
    StaleBase { base: i64, current: i64 },
    #[error("no profile version {version} exists for this user")]
    NoSuchVersion { version: i64 },
    #[error("profile version {version} is {status}, not pending")]
    NotPending { version: i64, status: String },
    #[error(transparent)]
    Db(#[from] sea_orm::DbErr),
}

/// The current taste profile. New users get the well-defined empty state
/// (version 0, empty content) without a row being created.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileView {
    pub content: String,
    pub version: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// A stored pending proposal.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingProposal {
    /// The version the proposal was stored as (current iff approved).
    pub version: i64,
    /// The version it was drafted against.
    pub base_version: i64,
    /// The pending proposal this one superseded, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_version: Option<i64>,
}

/// One history entry with its rendered diff.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionView {
    pub version: i64,
    pub base_version: i64,
    pub status: String,
    pub change_summary: String,
    pub created_at: DateTime<Utc>,
    /// Unified diff from the base version's content to this version's.
    pub diff: String,
}

pub async fn get_or_default(
    db: &DatabaseConnection,
    user_sub: &str,
) -> Result<ProfileView, sea_orm::DbErr> {
    Ok(match profile::Entity::find_by_id(user_sub).one(db).await? {
        Some(row) => ProfileView {
            content: row.content,
            version: row.version,
            updated_at: Some(row.updated_at.to_utc()),
        },
        None => ProfileView {
            content: String::new(),
            version: 0,
            updated_at: None,
        },
    })
}

/// Record a proposed profile update as a pending version row. The current
/// profile is not touched; an existing pending proposal is superseded.
///
/// `base_version` is the version the proposer drafted against (from
/// `get_profile`); a mismatch with the current version fails cleanly so a
/// proposal built on a stale read is never stored. `None` skips the check
/// and records the proposal against whatever is current.
pub async fn propose(
    db: &DatabaseConnection,
    user_sub: &str,
    content: &str,
    change_summary: &str,
    base_version: Option<i64>,
) -> Result<PendingProposal, ProfileError> {
    let txn = db.begin().await?;

    let current = current_version(&txn, user_sub).await?;
    if let Some(base) = base_version
        && base != current
    {
        return Err(ProfileError::StaleBase { base, current });
    }

    let superseded_version = pending_in(&txn, user_sub).await?.map(|row| row.version);
    profile_version::Entity::update_many()
        .col_expr(
            profile_version::Column::Status,
            Expr::value(status::SUPERSEDED),
        )
        .filter(profile_version::Column::UserSub.eq(user_sub))
        .filter(profile_version::Column::Status.eq(status::PENDING))
        .exec(&txn)
        .await?;

    let version = next_version(&txn, user_sub, current).await?;
    profile_version::ActiveModel {
        user_sub: Set(user_sub.to_owned()),
        version: Set(version),
        content: Set(content.to_owned()),
        change_summary: Set(change_summary.to_owned()),
        status: Set(status::PENDING.to_owned()),
        base_version: Set(current),
        created_at: Set(chrono::Utc::now().into()),
    }
    .insert(&txn)
    .await?;

    txn.commit().await?;
    Ok(PendingProposal {
        version,
        base_version: current,
        superseded_version,
    })
}

/// Make a pending proposal the current profile. The only path from
/// proposal to profile; fails if the proposal's base is no longer the
/// current version (a direct edit or another approval landed in between).
pub async fn approve(
    db: &DatabaseConnection,
    user_sub: &str,
    version: i64,
) -> Result<ProfileView, ProfileError> {
    let txn = db.begin().await?;

    let row = find_version(&txn, user_sub, version).await?;
    require_pending(&row)?;
    let current = current_version(&txn, user_sub).await?;
    if row.base_version != current {
        return Err(ProfileError::StaleBase {
            base: row.base_version,
            current,
        });
    }

    let view = set_current(&txn, user_sub, &row.content, version).await?;
    set_status(&txn, row, status::APPLIED).await?;

    txn.commit().await?;
    Ok(view)
}

/// Close a pending proposal without applying it.
pub async fn reject(
    db: &DatabaseConnection,
    user_sub: &str,
    version: i64,
) -> Result<(), ProfileError> {
    let txn = db.begin().await?;
    let row = find_version(&txn, user_sub, version).await?;
    require_pending(&row)?;
    set_status(&txn, row, status::REJECTED).await?;
    txn.commit().await?;
    Ok(())
}

/// Apply a new profile immediately (the UI's direct-edit path), still
/// recorded as an applied version row.
///
/// `base_version` is the version the editor loaded (stale-base rejection,
/// same contract as [`propose`]); `None` skips the check and applies over
/// whatever is current.
pub async fn edit_direct(
    db: &DatabaseConnection,
    user_sub: &str,
    content: &str,
    change_summary: &str,
    base_version: Option<i64>,
) -> Result<ProfileView, ProfileError> {
    let txn = db.begin().await?;

    let current = current_version(&txn, user_sub).await?;
    if let Some(base) = base_version
        && base != current
    {
        return Err(ProfileError::StaleBase { base, current });
    }
    let version = next_version(&txn, user_sub, current).await?;
    profile_version::ActiveModel {
        user_sub: Set(user_sub.to_owned()),
        version: Set(version),
        content: Set(content.to_owned()),
        change_summary: Set(change_summary.to_owned()),
        status: Set(status::APPLIED.to_owned()),
        base_version: Set(current),
        created_at: Set(chrono::Utc::now().into()),
    }
    .insert(&txn)
    .await?;
    let view = set_current(&txn, user_sub, content, version).await?;

    txn.commit().await?;
    Ok(view)
}

/// The caller's pending proposal, if any (at most one by construction).
pub async fn pending(
    db: &DatabaseConnection,
    user_sub: &str,
) -> Result<Option<profile_version::Model>, sea_orm::DbErr> {
    pending_in(db, user_sub).await
}

/// The caller's version history, newest first (the profile resource and
/// review flows read it).
pub async fn versions(
    db: &DatabaseConnection,
    user_sub: &str,
) -> Result<Vec<profile_version::Model>, sea_orm::DbErr> {
    profile_version::Entity::find()
        .filter(profile_version::Column::UserSub.eq(user_sub))
        .order_by_desc(profile_version::Column::Version)
        .all(db)
        .await
}

/// The version history with rendered unified diffs (each version against
/// its base), newest first.
pub async fn rendered_history(
    db: &DatabaseConnection,
    user_sub: &str,
) -> Result<Vec<VersionView>, sea_orm::DbErr> {
    let rows = versions(db, user_sub).await?;
    let content_by_version: std::collections::HashMap<i64, &str> = rows
        .iter()
        .map(|r| (r.version, r.content.as_str()))
        .collect();

    Ok(rows
        .iter()
        .map(|row| {
            // Base 0 is the well-defined empty profile.
            let base = content_by_version
                .get(&row.base_version)
                .copied()
                .unwrap_or("");
            let diff = similar::TextDiff::from_lines(base, &row.content)
                .unified_diff()
                .header(
                    &format!("v{}", row.base_version),
                    &format!("v{}", row.version),
                )
                .to_string();
            VersionView {
                version: row.version,
                base_version: row.base_version,
                status: row.status.clone(),
                change_summary: row.change_summary.clone(),
                created_at: row.created_at.to_utc(),
                diff,
            }
        })
        .collect())
}

/// A rendered unified diff from `base` to `new` content, matching the format
/// [`rendered_history`] uses. Exposed so a caller (e.g. the profile UI's
/// pending-proposal view) can render "current → proposal" without a version
/// row of its own.
pub fn diff(base: &str, new: &str) -> String {
    similar::TextDiff::from_lines(base, new)
        .unified_diff()
        .header("current", "proposed")
        .to_string()
}

/// The current profile version (0 when no row exists).
async fn current_version<C: ConnectionTrait>(
    conn: &C,
    user_sub: &str,
) -> Result<i64, sea_orm::DbErr> {
    Ok(profile::Entity::find_by_id(user_sub)
        .one(conn)
        .await?
        .map(|p| p.version)
        .unwrap_or(0))
}

/// Next version: one past the highest the user has anywhere (current
/// profile or version history — rejected and superseded rows keep their
/// numbers).
async fn next_version<C: ConnectionTrait>(
    conn: &C,
    user_sub: &str,
    current: i64,
) -> Result<i64, sea_orm::DbErr> {
    let max_version: Option<i64> = profile_version::Entity::find()
        .filter(profile_version::Column::UserSub.eq(user_sub))
        .select_only()
        .column_as(
            Expr::from(Func::max(Expr::col(profile_version::Column::Version))),
            "max",
        )
        .into_tuple()
        .one(conn)
        .await?
        .flatten();
    Ok(max_version.unwrap_or(0).max(current) + 1)
}

async fn pending_in<C: ConnectionTrait>(
    conn: &C,
    user_sub: &str,
) -> Result<Option<profile_version::Model>, sea_orm::DbErr> {
    profile_version::Entity::find()
        .filter(profile_version::Column::UserSub.eq(user_sub))
        .filter(profile_version::Column::Status.eq(status::PENDING))
        .order_by_desc(profile_version::Column::Version)
        .one(conn)
        .await
}

async fn find_version<C: ConnectionTrait>(
    conn: &C,
    user_sub: &str,
    version: i64,
) -> Result<profile_version::Model, ProfileError> {
    profile_version::Entity::find_by_id((user_sub.to_owned(), version))
        .one(conn)
        .await?
        .ok_or(ProfileError::NoSuchVersion { version })
}

fn require_pending(row: &profile_version::Model) -> Result<(), ProfileError> {
    if row.status != status::PENDING {
        return Err(ProfileError::NotPending {
            version: row.version,
            status: row.status.clone(),
        });
    }
    Ok(())
}

async fn set_status<C: ConnectionTrait>(
    conn: &C,
    row: profile_version::Model,
    new_status: &str,
) -> Result<(), sea_orm::DbErr> {
    let mut active: profile_version::ActiveModel = row.into();
    active.status = Set(new_status.to_owned());
    active.update(conn).await?;
    Ok(())
}

/// Point the current profile at `content`/`version` (insert or update).
async fn set_current<C: ConnectionTrait>(
    conn: &C,
    user_sub: &str,
    content: &str,
    version: i64,
) -> Result<ProfileView, sea_orm::DbErr> {
    let now = chrono::Utc::now();
    match profile::Entity::find_by_id(user_sub).one(conn).await? {
        Some(existing) => {
            let mut active: profile::ActiveModel = existing.into();
            active.content = Set(content.to_owned());
            active.version = Set(version);
            active.updated_at = Set(now.into());
            active.update(conn).await?;
        }
        None => {
            profile::ActiveModel {
                user_sub: Set(user_sub.to_owned()),
                content: Set(content.to_owned()),
                version: Set(version),
                updated_at: Set(now.into()),
            }
            .insert(conn)
            .await?;
        }
    }
    Ok(ProfileView {
        content: content.to_owned(),
        version,
        updated_at: Some(now),
    })
}
