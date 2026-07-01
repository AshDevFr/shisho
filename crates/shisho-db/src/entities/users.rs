//! `users` table. A single row in single-user mode; OIDC columns are filled
//! by multi-user auth. Timestamps are TEXT ISO 8601.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "users")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub oidc_sub: Option<String>,
    pub provider: Option<String>,
    /// JSON-encoded array of the user's OIDC group memberships, refreshed each
    /// login. NULL = none known. Read by group-based admin (`auth.admin_groups`).
    pub groups: Option<String>,
    pub created_at: String,
    pub last_seen: Option<String>,
    /// NULL = active; ISO 8601 timestamp = deactivated at that time. A
    /// deactivated user is locked out (sessions and API keys rejected) but
    /// keeps their data; clearing it restores access.
    pub deactivated_at: Option<String>,
    /// Codex user id this user is linked to, minted at first sign-in. NULL
    /// until a Codex credential is minted. The `user -> codex_user_sub`
    /// linkage is the identity broker's whole job.
    pub codex_user_sub: Option<String>,
    /// The minted Codex API key, sealed with the DEK-derived at-rest key
    /// (`shisho_crypto::seal`, nonce embedded). NULL when no key is stored.
    /// Never a plaintext key column.
    pub codex_key_ciphertext: Option<Vec<u8>>,
    /// KEK generation that sealed `codex_key_ciphertext`, so a future
    /// encryption-scheme change can identify and re-seal old blobs.
    pub codex_key_version: Option<i32>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::api_keys::Entity")]
    ApiKeys,
}

impl Related<super::api_keys::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::ApiKeys.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
