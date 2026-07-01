//! Reconciliation flags: one per verdict, raised when a recommended series
//! shows read progress without a recorded outcome. `resolved_at` is the one
//! mutable workflow column; rows are never deleted.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "reconciliation_flags")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Codex user ID — same key as the verdict log.
    pub user_sub: String,
    /// The standing verdict row the flag asks about (unique: a flag is
    /// raised at most once per verdict, resolved or not).
    pub verdict_id: i64,
    pub detected_at: DateTimeWithTimeZone,
    /// Set when an outcome for the series arrives; `NULL` = pending.
    pub resolved_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
