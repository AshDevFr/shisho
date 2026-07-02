//! Persistence for users and API keys.

use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, QueryOrder,
    Set,
};

use crate::keys::{self, Scope};
use shisho_db::entities::{api_keys, users};
use shisho_db::repos::scope::UserScope;

/// Fixed id of the single-user-mode local user. Multi-user mode creates real
/// rows alongside it; the constant keys every single-user resource to the
/// same owner across restarts.
pub const LOCAL_USER_ID: &str = "00000000-0000-4000-8000-000000000001";

/// Get-or-create the local user row (single-user mode owns exactly one).
/// Returns its id.
pub async fn ensure_local_user(db: &DatabaseConnection) -> Result<String, DbErr> {
    if users::Entity::find_by_id(LOCAL_USER_ID)
        .one(db)
        .await?
        .is_none()
    {
        users::ActiveModel {
            id: Set(LOCAL_USER_ID.to_string()),
            email: Set(None),
            display_name: Set(Some("Local".to_string())),
            avatar_url: Set(None),
            oidc_sub: Set(None),
            provider: Set(None),
            groups: Set(None),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            last_seen: Set(None),
            deactivated_at: Set(None),
            codex_user_sub: Set(None),
            codex_key_ciphertext: Set(None),
            codex_key_version: Set(None),
        }
        .insert(db)
        .await?;
    }
    Ok(LOCAL_USER_ID.to_string())
}

/// Stored keys owned by `scope`, newest first. `SingleUser` lists all keys
/// (one tenant / admin); `User` lists only that user's keys.
pub async fn list_keys(
    db: &DatabaseConnection,
    scope: &UserScope,
) -> Result<Vec<api_keys::Model>, DbErr> {
    api_keys::Entity::find()
        .filter(scope.condition(api_keys::Column::UserId))
        .order_by_desc(api_keys::Column::CreatedAt)
        .all(db)
        .await
}

/// Create a key owned by `user_id`. Returns the stored row and the plaintext;
/// the plaintext is never recoverable afterwards.
pub async fn create_key(
    db: &DatabaseConnection,
    user_id: Option<String>,
    name: &str,
    scopes: &[Scope],
    expires_at: Option<String>,
) -> anyhow::Result<(api_keys::Model, String)> {
    let plaintext = keys::generate_key();
    let row = api_keys::ActiveModel {
        id: Set(uuid::Uuid::new_v4().to_string()),
        user_id: Set(user_id),
        name: Set(name.to_string()),
        key_hash: Set(keys::hash_key(&plaintext)?),
        scopes: Set(keys::scopes_to_json(scopes)),
        expires_at: Set(expires_at),
        last_used: Set(None),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
    }
    .insert(db)
    .await?;
    Ok((row, plaintext))
}

/// Hard-delete a key the caller owns. Returns whether a row was removed; a key
/// owned by another user reads as absent (no cross-user revocation).
pub async fn delete_key(
    db: &DatabaseConnection,
    scope: &UserScope,
    id: &str,
) -> Result<bool, DbErr> {
    let result = api_keys::Entity::delete_many()
        .filter(api_keys::Column::Id.eq(id))
        .filter(scope.condition(api_keys::Column::UserId))
        .exec(db)
        .await?;
    Ok(result.rows_affected > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shisho_db::conn::connect_in_memory;

    /// Keys are listable and revocable only by their owner; the single-user
    /// scope sees all of them.
    #[tokio::test]
    async fn keys_are_isolated_per_owner() {
        use sea_orm::Set;
        use shisho_db::entities::users;

        let db = connect_in_memory().await;
        for id in ["user-a", "user-b"] {
            users::ActiveModel {
                id: Set(id.to_string()),
                email: Set(None),
                display_name: Set(None),
                avatar_url: Set(None),
                oidc_sub: Set(None),
                provider: Set(None),
                groups: Set(None),
                created_at: Set(chrono::Utc::now().to_rfc3339()),
                last_seen: Set(None),
                deactivated_at: Set(None),
                codex_user_sub: Set(None),
                codex_key_ciphertext: Set(None),
                codex_key_version: Set(None),
            }
            .insert(&db)
            .await
            .unwrap();
        }
        let (a_row, _) = create_key(&db, Some("user-a".into()), "a", &Scope::all(), None)
            .await
            .unwrap();
        let (b_row, _) = create_key(&db, Some("user-b".into()), "b", &Scope::all(), None)
            .await
            .unwrap();

        let a_scope = UserScope::user("user-a");
        let listed = list_keys(&db, &a_scope).await.unwrap();
        assert_eq!(listed.len(), 1, "A lists only A's key");
        assert_eq!(listed[0].id, a_row.id);

        // A cannot revoke B's key.
        assert!(!delete_key(&db, &a_scope, &b_row.id).await.unwrap());
        assert!(delete_key(&db, &a_scope, &a_row.id).await.unwrap());

        // Single-user scope still sees the remaining key (B's).
        assert_eq!(
            list_keys(&db, &UserScope::SingleUser).await.unwrap().len(),
            1
        );
    }
}
