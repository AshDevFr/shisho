//! Issued access/refresh token pairs and their lookup, used by `/token` and by
//! the bearer-auth path that authenticates `/mcp` and the REST API.

use sea_orm::{ActiveModelTrait, DatabaseConnection, DbErr, EntityTrait, Set};
use shisho_db::entities::oauth_tokens;

/// Everything pinned onto a freshly issued token pair.
pub struct NewTokens<'a> {
    /// SHA-256 hex of the access token.
    pub access_hash: &'a str,
    /// SHA-256 hex of the refresh token.
    pub refresh_hash: &'a str,
    pub client_id: &'a str,
    pub user_id: &'a str,
    /// JSON array of scope strings.
    pub scopes: &'a str,
    /// ISO 8601 access-token expiry.
    pub access_expires_at: &'a str,
    /// ISO 8601 refresh-token expiry.
    pub refresh_expires_at: &'a str,
}

/// Persist a new token pair.
pub async fn issue(db: &DatabaseConnection, tokens: NewTokens<'_>) -> Result<(), DbErr> {
    let model = oauth_tokens::ActiveModel {
        access_hash: Set(tokens.access_hash.to_string()),
        refresh_hash: Set(tokens.refresh_hash.to_string()),
        client_id: Set(tokens.client_id.to_string()),
        user_id: Set(tokens.user_id.to_string()),
        scopes: Set(tokens.scopes.to_string()),
        access_expires_at: Set(tokens.access_expires_at.to_string()),
        refresh_expires_at: Set(tokens.refresh_expires_at.to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
    };
    model.insert(db).await.map(|_| ())
}

/// Look up a token row by the SHA-256 hex of its access token.
pub async fn find_by_access_hash(
    db: &DatabaseConnection,
    access_hash: &str,
) -> Result<Option<oauth_tokens::Model>, DbErr> {
    oauth_tokens::Entity::find_by_id(access_hash).one(db).await
}

/// Look up a token row by the SHA-256 hex of its refresh token.
pub async fn find_by_refresh_hash(
    db: &DatabaseConnection,
    refresh_hash: &str,
) -> Result<Option<oauth_tokens::Model>, DbErr> {
    use sea_orm::{ColumnTrait, QueryFilter};
    oauth_tokens::Entity::find()
        .filter(oauth_tokens::Column::RefreshHash.eq(refresh_hash))
        .one(db)
        .await
}

/// Delete a token row by its access hash. Used on refresh (the old pair is
/// rotated out) and on revocation.
pub async fn delete_by_access_hash(
    db: &DatabaseConnection,
    access_hash: &str,
) -> Result<(), DbErr> {
    oauth_tokens::Entity::delete_by_id(access_hash)
        .exec(db)
        .await
        .map(|_| ())
}

/// Every token row owned by a user, newest first. Backs the connected-apps
/// view: the human sees which clients hold a live grant on their account.
pub async fn list_by_user(
    db: &DatabaseConnection,
    user_id: &str,
) -> Result<Vec<oauth_tokens::Model>, DbErr> {
    use sea_orm::{ColumnTrait, QueryFilter, QueryOrder};
    oauth_tokens::Entity::find()
        .filter(oauth_tokens::Column::UserId.eq(user_id))
        .order_by_desc(oauth_tokens::Column::CreatedAt)
        .all(db)
        .await
}

/// Revoke every grant a user holds for one client ("disconnect this app"). A
/// re-authorization or an in-flight refresh can leave several live rows for the
/// same client, so revocation deletes them all. Returns the rows removed, so
/// the caller can 404 a client the user never connected.
pub async fn delete_by_user_and_client(
    db: &DatabaseConnection,
    user_id: &str,
    client_id: &str,
) -> Result<u64, DbErr> {
    use sea_orm::{ColumnTrait, QueryFilter};
    let result = oauth_tokens::Entity::delete_many()
        .filter(oauth_tokens::Column::UserId.eq(user_id))
        .filter(oauth_tokens::Column::ClientId.eq(client_id))
        .exec(db)
        .await?;
    Ok(result.rows_affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shisho_db::conn::connect_in_memory;

    async fn seed(db: &DatabaseConnection, access: &str, refresh: &str, user: &str, client: &str) {
        issue(
            db,
            NewTokens {
                access_hash: access,
                refresh_hash: refresh,
                client_id: client,
                user_id: user,
                scopes: r#"["verdicts:read"]"#,
                access_expires_at: "2999-01-01T00:00:00+00:00",
                refresh_expires_at: "2999-01-01T00:00:00+00:00",
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn lookup_by_access_and_refresh_hash() {
        let db = connect_in_memory().await;
        seed(&db, "a1", "r1", "alice", "claude").await;

        assert!(find_by_access_hash(&db, "a1").await.unwrap().is_some());
        assert!(find_by_refresh_hash(&db, "r1").await.unwrap().is_some());
        assert!(find_by_access_hash(&db, "nope").await.unwrap().is_none());

        delete_by_access_hash(&db, "a1").await.unwrap();
        assert!(find_by_access_hash(&db, "a1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_by_user_returns_only_that_users_rows() {
        let db = connect_in_memory().await;
        seed(&db, "a1", "r1", "alice", "claude").await;
        seed(&db, "a2", "r2", "bob", "claude").await;

        let rows = list_by_user(&db, "alice").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].user_id, "alice");
    }

    #[tokio::test]
    async fn delete_by_user_and_client_removes_all_matching_grants() {
        let db = connect_in_memory().await;
        // Two live grants for the same client (a re-authorization), plus an
        // unrelated client that must survive.
        seed(&db, "a1", "r1", "alice", "claude").await;
        seed(&db, "a2", "r2", "alice", "claude").await;
        seed(&db, "a3", "r3", "alice", "other").await;

        let removed = delete_by_user_and_client(&db, "alice", "claude")
            .await
            .unwrap();
        assert_eq!(removed, 2);

        let rest = list_by_user(&db, "alice").await.unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].client_id, "other");
    }

    #[tokio::test]
    async fn delete_for_unconnected_client_removes_nothing() {
        let db = connect_in_memory().await;
        seed(&db, "a1", "r1", "alice", "claude").await;

        let removed = delete_by_user_and_client(&db, "alice", "ghost")
            .await
            .unwrap();
        assert_eq!(removed, 0);
    }
}
