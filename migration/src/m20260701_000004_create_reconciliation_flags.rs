//! `reconciliation_flags` — one open "did you finish X?" per verdict, raised
//! when a recommended series shows read progress without a recorded outcome.
//! `resolved_at` is the one mutable workflow column; rows are never deleted.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(ReconciliationFlags::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ReconciliationFlags::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(string(ReconciliationFlags::UserSub))
                    // The standing verdict row the flag asks about. One flag
                    // per verdict, ever: a resolved flag is not re-asked.
                    .col(big_integer(ReconciliationFlags::VerdictId))
                    .col(
                        timestamp_with_time_zone(ReconciliationFlags::DetectedAt)
                            .default(Expr::current_timestamp()),
                    )
                    // Set when an outcome for the series arrives; NULL = pending.
                    .col(timestamp_with_time_zone_null(
                        ReconciliationFlags::ResolvedAt,
                    ))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_reconciliation_flags_verdict_id")
                    .table(ReconciliationFlags::Table)
                    .col(ReconciliationFlags::VerdictId)
                    .unique()
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_reconciliation_flags_user_sub_resolved_at")
                    .table(ReconciliationFlags::Table)
                    .col(ReconciliationFlags::UserSub)
                    .col(ReconciliationFlags::ResolvedAt)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(ReconciliationFlags::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum ReconciliationFlags {
    Table,
    Id,
    UserSub,
    VerdictId,
    DetectedAt,
    ResolvedAt,
}
