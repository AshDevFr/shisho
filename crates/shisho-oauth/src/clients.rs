//! Registered OAuth clients (dynamic client registration, RFC 7591).
//!
//! Clients are public (PKCE, no secret). Registration is open: the security
//! gate is the user login + consent at `/authorize`, not client secrecy. The
//! client only constrains which `redirect_uri` values are accepted.

use sea_orm::{ActiveModelTrait, DatabaseConnection, DbErr, EntityTrait, Set};
use shisho_db::entities::oauth_clients;

/// Register a new public client and return its row. `client_id` is caller-
/// generated (random/opaque). `redirect_uris` is stored as a JSON array.
pub async fn register(
    db: &DatabaseConnection,
    client_id: &str,
    redirect_uris: &[String],
    client_name: Option<&str>,
) -> Result<oauth_clients::Model, DbErr> {
    let model = oauth_clients::ActiveModel {
        client_id: Set(client_id.to_string()),
        redirect_uris: Set(serde_json::to_string(redirect_uris).unwrap_or_else(|_| "[]".into())),
        client_name: Set(client_name.map(str::to_string)),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
    };
    model.insert(db).await
}

/// Fetch a client by id.
pub async fn get(
    db: &DatabaseConnection,
    client_id: &str,
) -> Result<Option<oauth_clients::Model>, DbErr> {
    oauth_clients::Entity::find_by_id(client_id).one(db).await
}

/// Parse the stored JSON array of redirect URIs. A corrupt value yields an
/// empty list, which makes every redirect_uri fail validation (fail closed).
pub fn redirect_uris(model: &oauth_clients::Model) -> Vec<String> {
    serde_json::from_str(&model.redirect_uris).unwrap_or_default()
}

/// Whether `redirect_uri` is one of the client's registered URIs (exact match,
/// per the OAuth security BCP: no prefix or wildcard matching).
pub fn allows_redirect(model: &oauth_clients::Model, redirect_uri: &str) -> bool {
    redirect_uris(model).iter().any(|u| u == redirect_uri)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shisho_db::conn::connect_in_memory;

    #[tokio::test]
    async fn register_then_get_round_trips_and_validates_redirects() {
        let db = connect_in_memory().await;
        let uris = vec!["https://app.example/cb".to_string()];
        register(&db, "client-1", &uris, Some("Example App"))
            .await
            .unwrap();

        let model = get(&db, "client-1").await.unwrap().expect("client exists");
        assert_eq!(model.client_name.as_deref(), Some("Example App"));
        assert!(allows_redirect(&model, "https://app.example/cb"));
        // Exact match only: no prefix/suffix/wildcard.
        assert!(!allows_redirect(&model, "https://app.example/cb/evil"));
        assert!(!allows_redirect(&model, "https://evil.example/cb"));
    }

    #[tokio::test]
    async fn unknown_client_is_none() {
        let db = connect_in_memory().await;
        assert!(get(&db, "ghost").await.unwrap().is_none());
    }
}
