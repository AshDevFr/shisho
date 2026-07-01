//! Reconciliation flags over the verdict log: "you started X after it was
//! recommended — outcome?". A flag points at the standing verdict row of a
//! series whose snapshot read state shows progress while no outcome is
//! recorded.
//!
//! Lifecycle: raised at most once per verdict (resolved or not — an
//! answered question is not re-asked), resolved when an outcome row for
//! the series arrives. Detection lives in `shisho-services` (it needs the
//! library snapshot); this module owns only the persistence.

use std::collections::HashMap;

use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder,
};

use crate::entities::{reconciliation_flag, verdict};
use shisho_ids::ExternalId;

/// Raise a flag for `verdict_id` unless one already exists (pending or
/// resolved). Returns the new flag, or `None` when it was already raised.
pub async fn raise(
    db: &DatabaseConnection,
    user_sub: &str,
    verdict_id: i64,
) -> Result<Option<reconciliation_flag::Model>, sea_orm::DbErr> {
    let existing = reconciliation_flag::Entity::find()
        .filter(reconciliation_flag::Column::VerdictId.eq(verdict_id))
        .one(db)
        .await?;
    if existing.is_some() {
        return Ok(None);
    }

    reconciliation_flag::ActiveModel {
        user_sub: Set(user_sub.to_owned()),
        verdict_id: Set(verdict_id),
        detected_at: Set(chrono::Utc::now().into()),
        resolved_at: Set(None),
        ..Default::default()
    }
    .insert(db)
    .await
    .map(Some)
}

/// Resolve the caller's pending flags pointing at any of `verdict_ids`
/// (an outcome arrived for their series). Returns how many were resolved.
pub async fn resolve_for_verdicts(
    db: &DatabaseConnection,
    user_sub: &str,
    verdict_ids: &[i64],
) -> Result<u64, sea_orm::DbErr> {
    if verdict_ids.is_empty() {
        return Ok(0);
    }
    let result = reconciliation_flag::Entity::update_many()
        .col_expr(
            reconciliation_flag::Column::ResolvedAt,
            Expr::value(chrono::Utc::now()),
        )
        .filter(reconciliation_flag::Column::UserSub.eq(user_sub))
        .filter(reconciliation_flag::Column::ResolvedAt.is_null())
        .filter(reconciliation_flag::Column::VerdictId.is_in(verdict_ids.iter().copied()))
        .exec(db)
        .await?;
    Ok(result.rows_affected)
}

/// Close pending flags for the series identified by `ids` — the call
/// every outcome write makes right after appending the feedback row.
pub async fn resolve_for_series<'a>(
    db: &DatabaseConnection,
    user_sub: &str,
    ids: impl IntoIterator<Item = &'a ExternalId>,
) -> Result<u64, sea_orm::DbErr> {
    let rows = super::verdicts::history(db, user_sub, ids).await?;
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    resolve_for_verdicts(db, user_sub, &ids).await
}

/// The caller's pending flags joined with the verdict rows they point at
/// (identity, tier, recommendation time) — what a consolidation or weekly
/// report session needs to phrase "you started X — outcome?".
pub async fn pending_with_verdicts(
    db: &DatabaseConnection,
    user_sub: &str,
) -> Result<Vec<(reconciliation_flag::Model, verdict::Model)>, sea_orm::DbErr> {
    let flags = pending(db, user_sub).await?;
    if flags.is_empty() {
        return Ok(Vec::new());
    }
    let rows = verdict::Entity::find()
        .filter(verdict::Column::Id.is_in(flags.iter().map(|f| f.verdict_id)))
        .all(db)
        .await?;
    let mut by_id: HashMap<i64, verdict::Model> =
        rows.into_iter().map(|row| (row.id, row)).collect();
    Ok(flags
        .into_iter()
        .filter_map(|flag| by_id.remove(&flag.verdict_id).map(|row| (flag, row)))
        .collect())
}

/// The caller's pending flags, oldest first — the open reconciliation
/// questions a consolidation (or weekly report) session should ask.
pub async fn pending(
    db: &DatabaseConnection,
    user_sub: &str,
) -> Result<Vec<reconciliation_flag::Model>, sea_orm::DbErr> {
    reconciliation_flag::Entity::find()
        .filter(reconciliation_flag::Column::UserSub.eq(user_sub))
        .filter(reconciliation_flag::Column::ResolvedAt.is_null())
        .order_by_asc(reconciliation_flag::Column::DetectedAt)
        .order_by_asc(reconciliation_flag::Column::Id)
        .all(db)
        .await
}

/// The caller's pending-flag count, for `status()`.
pub async fn pending_count(db: &DatabaseConnection, user_sub: &str) -> Result<u64, sea_orm::DbErr> {
    reconciliation_flag::Entity::find()
        .filter(reconciliation_flag::Column::UserSub.eq(user_sub))
        .filter(reconciliation_flag::Column::ResolvedAt.is_null())
        .count(db)
        .await
}
