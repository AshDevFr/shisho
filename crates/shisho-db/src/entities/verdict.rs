//! Append-only recommendation log. Retraction is a follow-up row with
//! `retracted = true`; rows are never updated or deleted.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "verdicts")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Codex user ID — the canonical identity key, never the API key.
    pub user_sub: String,
    /// Canonical series identity; `(provider, external_id)` is the fallback
    /// when no MangaBaka id is known.
    pub mangabaka_id: Option<i64>,
    pub provider: Option<String>,
    pub external_id: Option<String>,
    /// Groups all verdicts of one `recommend` run.
    pub session_id: String,
    pub tier: String,
    pub rationale: String,
    pub verified: bool,
    pub retracted: bool,
    /// Why a retraction was recorded: `failed_verification` keeps the
    /// series excluded from candidate pools (verified-and-rejected is not
    /// re-litigated); `withdrawn` returns it to candidacy. `None` only on
    /// rows predating the column (treated as `withdrawn`).
    pub retraction_reason: Option<String>,
    pub outcome: Option<String>,
    pub notes: Option<String>,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
