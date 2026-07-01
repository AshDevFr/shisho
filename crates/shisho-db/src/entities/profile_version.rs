//! Full history of profile changes with change summaries.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "profile_versions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_sub: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub version: i64,
    pub content: String,
    pub change_summary: String,
    /// Workflow state: `pending` | `applied` | `rejected` | `superseded`.
    /// Content is append-only; status is the one mutable column.
    pub status: String,
    /// The profile version this row was drafted against; approval refuses
    /// a proposal whose base is no longer the current version.
    pub base_version: i64,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
