//! OAuth 2.0 authorization-server tables: `oauth_clients`, `oauth_auth_codes`,
//! and `oauth_tokens`.
//!
//! These back the authorization server that lets MCP clients (e.g. the
//! claude.ai web connector) obtain tokens for `/mcp` when a static API key is
//! not usable. The server issues opaque high-entropy secrets and stores only
//! their SHA-256 hashes; the hex hash is the primary key, so verification is an
//! O(1) lookup rather than the per-row argon2 scan `api_keys` needs. Timestamps
//! are TEXT ISO 8601 to match the rest of the schema.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum OauthClients {
    Table,
    /// Public client identifier handed out at registration (DCR, RFC 7591).
    ClientId,
    /// JSON array of permitted redirect URIs.
    RedirectUris,
    ClientName,
    CreatedAt,
}

#[derive(DeriveIden)]
enum OauthAuthCodes {
    Table,
    /// SHA-256 hex of the authorization code (the code itself is never stored).
    CodeHash,
    ClientId,
    RedirectUri,
    /// PKCE `code_challenge` (S256). Verified against `code_verifier` at /token.
    CodeChallenge,
    /// The user this code authorizes, pinned at the consent step.
    UserId,
    /// JSON array of granted scope strings.
    Scopes,
    ExpiresAt,
    /// Single-use marker: a code that has minted a token cannot be replayed.
    Used,
    CreatedAt,
}

#[derive(DeriveIden)]
enum OauthTokens {
    Table,
    /// SHA-256 hex of the access token.
    AccessHash,
    /// SHA-256 hex of the refresh token.
    RefreshHash,
    ClientId,
    UserId,
    Scopes,
    AccessExpiresAt,
    RefreshExpiresAt,
    CreatedAt,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(OauthClients::Table)
                    .col(
                        ColumnDef::new(OauthClients::ClientId)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(OauthClients::RedirectUris).text().not_null())
                    .col(ColumnDef::new(OauthClients::ClientName).text())
                    .col(ColumnDef::new(OauthClients::CreatedAt).text().not_null())
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(OauthAuthCodes::Table)
                    .col(
                        ColumnDef::new(OauthAuthCodes::CodeHash)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(OauthAuthCodes::ClientId).text().not_null())
                    .col(
                        ColumnDef::new(OauthAuthCodes::RedirectUri)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(OauthAuthCodes::CodeChallenge)
                            .text()
                            .not_null(),
                    )
                    .col(ColumnDef::new(OauthAuthCodes::UserId).text().not_null())
                    .col(ColumnDef::new(OauthAuthCodes::Scopes).text().not_null())
                    .col(ColumnDef::new(OauthAuthCodes::ExpiresAt).text().not_null())
                    .col(
                        ColumnDef::new(OauthAuthCodes::Used)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(ColumnDef::new(OauthAuthCodes::CreatedAt).text().not_null())
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(OauthTokens::Table)
                    .col(
                        ColumnDef::new(OauthTokens::AccessHash)
                            .text()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(OauthTokens::RefreshHash)
                            .text()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(OauthTokens::ClientId).text().not_null())
                    .col(ColumnDef::new(OauthTokens::UserId).text().not_null())
                    .col(ColumnDef::new(OauthTokens::Scopes).text().not_null())
                    .col(
                        ColumnDef::new(OauthTokens::AccessExpiresAt)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(OauthTokens::RefreshExpiresAt)
                            .text()
                            .not_null(),
                    )
                    .col(ColumnDef::new(OauthTokens::CreatedAt).text().not_null())
                    .to_owned(),
            )
            .await?;

        // Token lookups during /token refresh and the user's connected-app list
        // hit user_id; the access/refresh hashes are already PK/unique.
        manager
            .create_index(
                Index::create()
                    .name("idx_oauth_tokens_user_id")
                    .table(OauthTokens::Table)
                    .col(OauthTokens::UserId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(OauthTokens::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(OauthAuthCodes::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(OauthClients::Table).to_owned())
            .await
    }
}
