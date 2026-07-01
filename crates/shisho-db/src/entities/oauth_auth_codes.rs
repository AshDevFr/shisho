//! `oauth_auth_codes` table. A single-use, short-lived authorization code bound
//! to a client, redirect URI, PKCE challenge, user, and scope set. `code_hash`
//! is the SHA-256 hex of the code (the code itself is never stored). `scopes` is
//! a JSON string array. Timestamps are TEXT ISO 8601.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "oauth_auth_codes")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub code_hash: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub user_id: String,
    pub scopes: String,
    pub expires_at: String,
    pub used: bool,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
