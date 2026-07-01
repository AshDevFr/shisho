//! Test fixtures shared with the api and mcp crates (via the `test-util`
//! feature): a migrated in-memory database and API-key seeding.

use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};

use crate::keys::{self, Scope};
use shisho_db::entities::api_keys;

pub use shisho_db::conn::connect_in_memory as test_db;

/// Insert an API key row and return `(plaintext, id)`. The plaintext is what
/// a client would send; only the hash is stored.
pub async fn seed_key(
    db: &DatabaseConnection,
    name: &str,
    scopes: &[Scope],
    expires_at: Option<String>,
) -> (String, String) {
    let plaintext = keys::generate_key();
    let id = uuid::Uuid::new_v4().to_string();
    api_keys::ActiveModel {
        id: Set(id.clone()),
        user_id: Set(None),
        name: Set(name.to_string()),
        key_hash: Set(keys::hash_key(&plaintext).unwrap()),
        scopes: Set(keys::scopes_to_json(scopes)),
        expires_at: Set(expires_at),
        last_used: Set(None),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
    }
    .insert(db)
    .await
    .unwrap();
    (plaintext, id)
}
