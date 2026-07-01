//! `oauth_tokens` table. An issued access/refresh token pair bound to a client,
//! user, and scope set. `access_hash` / `refresh_hash` are the SHA-256 hex of
//! the respective opaque tokens (the tokens themselves are never stored).
//! `scopes` is a JSON string array. Timestamps are TEXT ISO 8601.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "oauth_tokens")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub access_hash: String,
    #[sea_orm(unique)]
    pub refresh_hash: String,
    pub client_id: String,
    pub user_id: String,
    pub scopes: String,
    pub access_expires_at: String,
    pub refresh_expires_at: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
