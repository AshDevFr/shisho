//! `users` and `api_keys` tables.
//!
//! Identity layer for multi-user auth. A `users` row is keyed by id (a UUID);
//! `(provider, oidc_sub)` is the federated identity, kept unique so first-login
//! provisioning is race-free. In single-user mode there is one local user with
//! NULL provider/oidc_sub. `api_keys` are argon2-hashed bearer credentials with
//! a JSON `scopes` array (`resource:action`); they may belong to a user or be
//! unowned (service/admin keys). Timestamps are TEXT ISO 8601 throughout.
//!
//! The migration seeds no data: the runtime provisions users lazily so
//! migrations stay data-free.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Users {
    Table,
    Id,
    Email,
    DisplayName,
    AvatarUrl,
    OidcSub,
    Provider,
    /// JSON-encoded array of OIDC group memberships, refreshed each login.
    /// Drives group-based admin (`auth.admin_groups`). NULL = none known.
    Groups,
    CreatedAt,
    LastSeen,
    /// NULL = active; ISO 8601 timestamp = reversibly deactivated at that time.
    DeactivatedAt,
}

#[derive(DeriveIden)]
enum ApiKeys {
    Table,
    Id,
    UserId,
    Name,
    KeyHash,
    /// JSON array of granted `resource:action` scope strings. Empty by default;
    /// scopes are assigned explicitly at key creation.
    Scopes,
    ExpiresAt,
    LastUsed,
    CreatedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Users::Table)
                    .col(ColumnDef::new(Users::Id).text().not_null().primary_key())
                    .col(ColumnDef::new(Users::Email).text().unique_key())
                    .col(ColumnDef::new(Users::DisplayName).text())
                    .col(ColumnDef::new(Users::AvatarUrl).text())
                    .col(ColumnDef::new(Users::OidcSub).text())
                    .col(ColumnDef::new(Users::Provider).text())
                    .col(ColumnDef::new(Users::Groups).text())
                    .col(ColumnDef::new(Users::CreatedAt).text().not_null())
                    .col(ColumnDef::new(Users::LastSeen).text())
                    .col(ColumnDef::new(Users::DeactivatedAt).text())
                    .to_owned(),
            )
            .await?;

        // Unique federated identity. SQLite treats NULLs as distinct in a UNIQUE
        // index, so the single-user local user (NULL provider/oidc_sub) and any
        // other NULL-identity rows never collide with each other or with real
        // provider identities.
        manager
            .create_index(
                Index::create()
                    .name("idx_users_provider_sub")
                    .table(Users::Table)
                    .col(Users::Provider)
                    .col(Users::OidcSub)
                    .unique()
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(ApiKeys::Table)
                    .col(ColumnDef::new(ApiKeys::Id).text().not_null().primary_key())
                    .col(ColumnDef::new(ApiKeys::UserId).text())
                    .col(ColumnDef::new(ApiKeys::Name).text().not_null())
                    .col(ColumnDef::new(ApiKeys::KeyHash).text().not_null())
                    .col(
                        ColumnDef::new(ApiKeys::Scopes)
                            .text()
                            .not_null()
                            .default("[]"),
                    )
                    .col(ColumnDef::new(ApiKeys::ExpiresAt).text())
                    .col(ColumnDef::new(ApiKeys::LastUsed).text())
                    .col(ColumnDef::new(ApiKeys::CreatedAt).text().not_null())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_api_keys_user_id")
                            .from(ApiKeys::Table, ApiKeys::UserId)
                            .to(Users::Table, Users::Id),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_api_keys_user_id")
                    .table(ApiKeys::Table)
                    .col(ApiKeys::UserId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // api_keys references users, so it goes first.
        manager
            .drop_table(Table::drop().table(ApiKeys::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Users::Table).to_owned())
            .await
    }
}
