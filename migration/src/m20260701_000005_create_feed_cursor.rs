//! `feed_cursor` — the Tsundoku feed position, the only cross-system state
//! Shisho persists. Keyed per feed ("tsundoku" today) so a second feed never
//! needs a migration.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(FeedCursor::Table)
                    .if_not_exists()
                    .col(string(FeedCursor::Id).primary_key())
                    .col(text(FeedCursor::Cursor))
                    .col(
                        timestamp_with_time_zone(FeedCursor::UpdatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(FeedCursor::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum FeedCursor {
    Table,
    Id,
    Cursor,
    UpdatedAt,
}
