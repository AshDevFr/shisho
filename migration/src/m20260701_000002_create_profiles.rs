//! `profiles` — the current taste profile per user (Markdown). History lives
//! in `profile_versions`.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Profiles::Table)
                    .if_not_exists()
                    .col(string(Profiles::UserSub).primary_key())
                    .col(text(Profiles::Content))
                    .col(big_integer(Profiles::Version))
                    .col(
                        timestamp_with_time_zone(Profiles::UpdatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Profiles::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Profiles {
    Table,
    UserSub,
    Content,
    Version,
    UpdatedAt,
}
