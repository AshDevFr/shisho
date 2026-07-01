//! `oauth_clients` table. A registered OAuth client (e.g. an MCP connector),
//! created via dynamic client registration. Public clients only: no secret is
//! stored. `redirect_uris` is a JSON string array. Timestamps are TEXT ISO 8601.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "oauth_clients")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub client_id: String,
    pub redirect_uris: String,
    pub client_name: Option<String>,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
