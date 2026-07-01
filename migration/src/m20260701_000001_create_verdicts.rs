//! `verdicts` — the append-only recommendation log. Retraction is a follow-up
//! row (`retracted = true` + a reason); rows are never updated or deleted.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Verdicts::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Verdicts::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    // Codex user id, the canonical identity key (never the key).
                    .col(string(Verdicts::UserSub))
                    // Canonical series identity; (provider, external_id) is the
                    // fallback when no MangaBaka id is known.
                    .col(big_integer_null(Verdicts::MangabakaId))
                    .col(string_null(Verdicts::Provider))
                    .col(string_null(Verdicts::ExternalId))
                    // Groups all verdicts of one `recommend` run.
                    .col(string(Verdicts::SessionId))
                    .col(string(Verdicts::Tier))
                    .col(text(Verdicts::Rationale))
                    .col(boolean(Verdicts::Verified).default(false))
                    .col(boolean(Verdicts::Retracted).default(false))
                    // Why a retraction was recorded: 'failed_verification'
                    // keeps the series excluded from candidate pools (not
                    // re-litigated); 'withdrawn' returns it to candidacy. NULL
                    // = treated as 'withdrawn'.
                    .col(string_null(Verdicts::RetractionReason))
                    .col(string_null(Verdicts::Outcome))
                    .col(text_null(Verdicts::Notes))
                    .col(
                        timestamp_with_time_zone(Verdicts::CreatedAt)
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_verdicts_user_sub_mangabaka_id")
                    .table(Verdicts::Table)
                    .col(Verdicts::UserSub)
                    .col(Verdicts::MangabakaId)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_verdicts_user_sub_session_id")
                    .table(Verdicts::Table)
                    .col(Verdicts::UserSub)
                    .col(Verdicts::SessionId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Verdicts::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Verdicts {
    Table,
    Id,
    UserSub,
    MangabakaId,
    Provider,
    ExternalId,
    SessionId,
    Tier,
    Rationale,
    Verified,
    Retracted,
    RetractionReason,
    Outcome,
    Notes,
    CreatedAt,
}
