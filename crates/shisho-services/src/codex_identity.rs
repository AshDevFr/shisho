//! The Codex identity broker.
//!
//! At sign-in Shisho mints a **dedicated least-privilege** Codex key for the
//! user and stores it sealed, so every later Codex call uses that stored key.
//! The user's IdP token (OIDC path) or pasted `api-keys:write` key (API-key
//! path) authorizes the mint **once** and is never persisted — only the minted
//! key is, encrypted. This is the whole point of the rewrite: Shisho holds a
//! narrow, revocable credential instead of forwarding the user's token per
//! request.

use shisho_codex::{CodexClient, CodexCredential, CodexError, SHISHO_KEY_SCOPES};
use shisho_db::repos::users;
use shisho_db::{DatabaseConnection, DbErr, Dek};

/// Minting the key or persisting it failed. The authorizing credential is never
/// included in the error (it is not stored or logged).
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("codex rejected the mint or /me call: {0}")]
    Codex(#[from] CodexError),
    #[error("persisting the minted codex credential failed: {0}")]
    Db(#[from] DbErr),
    #[error("no user row to attach the codex credential to (id {0})")]
    UnknownUser(String),
}

/// Mint the Shisho-dedicated least-privilege Codex key for `user_id`, resolve
/// the owning `codex_user_sub` via `/me`, and store the key sealed under `dek`.
///
/// `authz` is the one-shot authorization: `CodexCredential::ApiKey` for the
/// pasted-key path or `CodexCredential::Bearer` for the OIDC access token. It is
/// used only to mint and is then dropped by the caller; nothing but the minted
/// key (encrypted) and `codex_user_sub` is persisted. Returns the resolved
/// `codex_user_sub`.
pub async fn mint_and_store(
    codex: &CodexClient,
    db: &DatabaseConnection,
    dek: &Dek,
    kek_version: u32,
    user_id: &str,
    authz: &CodexCredential,
    key_name: &str,
) -> Result<String, IdentityError> {
    // 1. Mint the narrow key. The scope set is fixed (series:read +
    //    libraries:read); it is never derived from the caller's permissions.
    let minted = codex
        .mint_api_key(authz, key_name, SHISHO_KEY_SCOPES)
        .await?;

    // 2. Resolve the Codex identity with the *minted* key (not `authz`), so the
    //    stored sub matches the credential we keep.
    let who = codex
        .me(&CodexCredential::ApiKey(minted.key.clone()))
        .await?;

    // 3. Seal + store. A missing row means the user vanished mid-sign-in.
    let stored =
        users::set_codex_credential(db, user_id, &who.id, &minted.key, dek, kek_version).await?;
    if !stored {
        return Err(IdentityError::UnknownUser(user_id.to_owned()));
    }

    tracing::info!(
        user_id,
        codex_user_sub = %who.id,
        "minted and stored a least-privilege Codex key"
    );
    Ok(who.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shisho_db::connect_in_memory;
    use shisho_db::repos::users::{OidcIdentity, find_or_create, get_codex_credential};
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn codex(server: &MockServer) -> CodexClient {
        CodexClient::new(&server.uri(), reqwest::Client::new())
    }

    fn mint_body() -> serde_json::Value {
        serde_json::json!({
            "key": "codex_dead_beef",
            "id": "codex-key-1",
            "name": "shisho",
            "keyPrefix": "codex_dead",
            "isActive": true,
            "expiresAt": null,
            "createdAt": "2026-07-01T00:00:00Z"
        })
    }

    fn me_body() -> serde_json::Value {
        serde_json::json!({
            "id": "codex-user-42",
            "username": "ada",
            "email": "ada@example.com",
            "role": "user",
            "emailVerified": true,
            "permissions": ["series:read", "libraries:read"]
        })
    }

    async fn a_user(db: &shisho_db::DatabaseConnection) -> String {
        find_or_create(
            db,
            &OidcIdentity {
                provider: "authentik".into(),
                subject: "sub-1".into(),
                email: Some("ada@example.com".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn mints_least_privilege_and_stores_encrypted() {
        let server = MockServer::start().await;
        // The mint must request exactly the least-privilege scope set.
        Mock::given(method("POST"))
            .and(path("/api/v1/api-keys"))
            .and(body_partial_json(serde_json::json!({
                "permissions": ["series:read", "libraries:read"]
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(mint_body()))
            .expect(1)
            .mount(&server)
            .await;
        // /me is resolved with the freshly minted key.
        Mock::given(method("GET"))
            .and(path("/api/v1/auth/me"))
            .and(header("X-API-Key", "codex_dead_beef"))
            .respond_with(ResponseTemplate::new(200).set_body_json(me_body()))
            .expect(1)
            .mount(&server)
            .await;

        let db = connect_in_memory().await;
        let dek = shisho_db::Dek::from_bytes([7u8; 32]);
        let user_id = a_user(&db).await;

        let sub = mint_and_store(
            &codex(&server),
            &db,
            &dek,
            1,
            &user_id,
            &CodexCredential::ApiKey("onboarding-key-with-write".into()),
            "shisho",
        )
        .await
        .expect("mint");

        assert_eq!(sub, "codex-user-42");

        // The minted key is stored (sealed) and recovers; the onboarding key is
        // nowhere on the row.
        let cred = get_codex_credential(&db, &user_id, &dek)
            .await
            .unwrap()
            .expect("stored credential");
        assert_eq!(cred.codex_user_sub, "codex-user-42");
        assert_eq!(cred.key, "codex_dead_beef");
    }

    #[tokio::test]
    async fn oidc_path_forwards_the_bearer_token_exactly_once() {
        let server = MockServer::start().await;
        // The one-shot passthrough: the mint is authorized by the IdP bearer,
        // and it must happen exactly once.
        Mock::given(method("POST"))
            .and(path("/api/v1/api-keys"))
            .and(header("authorization", "Bearer authentik-access-token"))
            .respond_with(ResponseTemplate::new(201).set_body_json(mint_body()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/auth/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(me_body()))
            .expect(1)
            .mount(&server)
            .await;

        let db = connect_in_memory().await;
        let dek = shisho_db::Dek::from_bytes([9u8; 32]);
        let user_id = a_user(&db).await;

        let sub = mint_and_store(
            &codex(&server),
            &db,
            &dek,
            1,
            &user_id,
            &CodexCredential::Bearer("authentik-access-token".into()),
            "shisho",
        )
        .await
        .expect("mint via bearer");
        assert_eq!(sub, "codex-user-42");
        // `expect(1)` on both mocks is asserted on drop: exactly one mint call.
    }

    #[tokio::test]
    async fn unknown_user_is_reported() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/api-keys"))
            .respond_with(ResponseTemplate::new(201).set_body_json(mint_body()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/auth/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(me_body()))
            .mount(&server)
            .await;

        let db = connect_in_memory().await;
        let dek = shisho_db::Dek::from_bytes([1u8; 32]);
        let err = mint_and_store(
            &codex(&server),
            &db,
            &dek,
            1,
            "no-such-user",
            &CodexCredential::ApiKey("k".into()),
            "shisho",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, IdentityError::UnknownUser(_)));
    }
}
