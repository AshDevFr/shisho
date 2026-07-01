//! `profile_versions` — full history of profile changes with change summaries.
//! Content rows are append-only; `status` is the one mutable workflow column.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(ProfileVersions::Table)
                    .if_not_exists()
                    .col(string(ProfileVersions::UserSub))
                    .col(big_integer(ProfileVersions::Version))
                    .col(text(ProfileVersions::Content))
                    .col(text(ProfileVersions::ChangeSummary))
                    // Proposal workflow state: 'pending' | 'applied' |
                    // 'rejected' | 'superseded'. The current profile's version
                    // row is always 'applied'.
                    .col(string(ProfileVersions::Status).default("applied"))
                    // The profile version the proposal was drafted against;
                    // approval refuses a proposal whose base is no longer
                    // current (stale-base protection).
                    .col(big_integer(ProfileVersions::BaseVersion).default(0))
                    .col(
                        timestamp_with_time_zone(ProfileVersions::CreatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .primary_key(
                        Index::create()
                            .col(ProfileVersions::UserSub)
                            .col(ProfileVersions::Version),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(ProfileVersions::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum ProfileVersions {
    Table,
    UserSub,
    Version,
    Content,
    ChangeSummary,
    Status,
    BaseVersion,
    CreatedAt,
}
