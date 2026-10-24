//! `taste_tuning` — the ranking parameters chosen for each user from their
//! own library, with the evidence they were chosen on.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(TasteTuning::Table)
                    .if_not_exists()
                    .col(string(TasteTuning::UserSub).primary_key())
                    .col(text(TasteTuning::Params))
                    .col(boolean(TasteTuning::Tuned))
                    .col(integer(TasteTuning::Favourites))
                    .col(double_null(TasteTuning::BaselineRankFraction))
                    .col(double_null(TasteTuning::RankFraction))
                    .col(
                        timestamp_with_time_zone(TasteTuning::ComputedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(TasteTuning::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum TasteTuning {
    Table,
    UserSub,
    Params,
    Tuned,
    Favourites,
    BaselineRankFraction,
    RankFraction,
    ComputedAt,
}
