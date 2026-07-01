//! Link the shell's `users` row to a Codex identity: the minted `codex_user_sub`
//! and the envelope-sealed minted key. Additive and nullable — a fresh user has
//! no Codex identity until first sign-in mints one. This does not touch the
//! shell's original `users` migration.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Codex user id (the linkage), nullable until first mint.
        manager
            .alter_table(
                Table::alter()
                    .table(Users::Table)
                    .add_column(string_null(Users::CodexUserSub))
                    .to_owned(),
            )
            .await?;

        // The minted key sealed with the DEK-derived at-rest key (nonce
        // embedded in the blob). Never a plaintext column.
        manager
            .alter_table(
                Table::alter()
                    .table(Users::Table)
                    .add_column(ColumnDef::new(Users::CodexKeyCiphertext).blob())
                    .to_owned(),
            )
            .await?;

        // KEK generation that sealed the blob, for a future re-seal.
        manager
            .alter_table(
                Table::alter()
                    .table(Users::Table)
                    .add_column(integer_null(Users::CodexKeyVersion))
                    .to_owned(),
            )
            .await?;

        // Reverse lookup: minted-key resolution finds the user by codex_user_sub.
        manager
            .create_index(
                Index::create()
                    .name("idx_users_codex_user_sub")
                    .table(Users::Table)
                    .col(Users::CodexUserSub)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name("idx_users_codex_user_sub")
                    .table(Users::Table)
                    .to_owned(),
            )
            .await?;
        for col in [
            Users::CodexUserSub,
            Users::CodexKeyCiphertext,
            Users::CodexKeyVersion,
        ] {
            manager
                .alter_table(
                    Table::alter()
                        .table(Users::Table)
                        .drop_column(col)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Users {
    Table,
    CodexUserSub,
    CodexKeyCiphertext,
    CodexKeyVersion,
}
