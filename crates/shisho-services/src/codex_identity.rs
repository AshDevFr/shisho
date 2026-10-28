//! The Codex identity broker.
//!
//! At sign-in Shisho mints a **dedicated least-privilege** Codex key for the
//! user and stores it sealed, so every later Codex call uses that stored key.
//! The user's IdP token (OIDC path) or pasted `api-keys:write` key (API-key
//! path) authorizes the mint **once** and is never persisted — only the minted
//! key is, encrypted. This is the whole point of the rewrite: Shisho holds a
//! narrow, revocable credential instead of forwarding the user's token per
//! request.

use std::time::Duration;

use shisho_cache::{Freshness, SwrCache, SwrConfig};
use shisho_codex::{
    CodexClient, CodexCredential, CodexError, SHISHO_KEY_SCOPES, SHISHO_READ_ONLY_SCOPES, UserCtx,
};
use shisho_db::repos::users;
use shisho_db::repos::users::CodexCredentialError;
use shisho_db::{DatabaseConnection, DbErr, Dek};

/// Minting, persisting, or resolving a Codex credential failed. The credential
/// itself is never included in the error (not stored or logged).
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("codex rejected the mint or /me call: {0}")]
    Codex(#[from] CodexError),
    #[error("persisting the minted codex credential failed: {0}")]
    Db(#[from] DbErr),
    #[error("no user row to attach the codex credential to (id {0})")]
    UnknownUser(String),
    #[error("reading the stored codex credential failed: {0}")]
    StoredCredential(#[from] CodexCredentialError),
    #[error("no codex credential is stored for this user; sign in to mint one")]
    NoCredential,
    #[error("the stored codex key was revoked or expired; re-authenticate at the Shisho UI")]
    CredentialsExpired,
}

impl IdentityError {
    /// The caller-facing message to relay verbatim (MCP re-auth error / Status)
    /// when the stored credential is no longer valid. `None` for errors that are
    /// not the user's to fix by re-authenticating.
    pub fn reauth_message(&self) -> Option<&'static str> {
        matches!(self, IdentityError::CredentialsExpired).then_some(
            "Your Codex authorization expired or was revoked. \
             Re-authenticate at the Shisho UI to restore access.",
        )
    }
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
    // 1. Mint the narrow key. The scope set is fixed (`SHISHO_KEY_SCOPES`);
    //    it is never derived from the caller's permissions. Codex refuses the
    //    whole mint when the authorizing credential cannot grant a write scope
    //    (403) or does not know it (400, before 2.9.0); the writes are
    //    optional extras, so retry read-only rather than fail sign-in over
    //    them.
    let minted = match codex.mint_api_key(authz, key_name, SHISHO_KEY_SCOPES).await {
        Ok(minted) => minted,
        Err(CodexError::Api { status, body, .. }) if matches!(status.as_u16(), 400 | 403) => {
            tracing::warn!(
                user_id,
                %status,
                body,
                "Codex refused the write scopes; minting a read-only key, so rating and want-to-read writes are unavailable"
            );
            codex
                .mint_api_key(authz, key_name, SHISHO_READ_ONLY_SCOPES)
                .await?
        }
        Err(e) => return Err(e.into()),
    };

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

/// The per-request Codex identity resolver: turns a signed-in shell user into a
/// [`UserCtx`] (its `codex_user_sub` + the stored key) for outbound Codex calls,
/// caching `/me` validation so a live check runs at most once per
/// `revalidate_after`. Cheap to clone (shared client + cache).
#[derive(Clone)]
pub struct CodexIdentity {
    codex: CodexClient,
    /// `sha256(minted key)` → `codex_user_sub`. Keying on the key hash (not the
    /// user id) means a rotated or revoked key misses immediately and forces a
    /// fresh `/me`, so revocation propagates within one `revalidate_after`.
    validation: SwrCache<String, String>,
}

impl CodexIdentity {
    /// Build the resolver over a Codex client, validating each stored key via
    /// `/me` at most once per `revalidate_after` (≈5 min in production).
    pub fn new(codex: CodexClient, revalidate_after: Duration) -> Self {
        Self {
            codex,
            validation: SwrCache::new(SwrConfig {
                revalidate_after,
                build_budget: Duration::from_secs(10),
            }),
        }
    }

    /// The shared Codex client, for callers that make their own per-user calls.
    pub fn codex(&self) -> &CodexClient {
        &self.codex
    }

    /// Resolve `user_id` to a [`UserCtx`]. Decrypts the stored key, then
    /// validates it against Codex `/me` unless a fresh validation is cached. A
    /// Codex 401 becomes [`IdentityError::CredentialsExpired`] — never a silent
    /// fallthrough. The `/me`-reported id is authoritative for `user_sub`.
    pub async fn resolve(
        &self,
        db: &DatabaseConnection,
        dek: &Dek,
        user_id: &str,
    ) -> Result<UserCtx, IdentityError> {
        let cred = users::get_codex_credential(db, user_id, dek)
            .await?
            .ok_or(IdentityError::NoCredential)?;
        let key_hash = sha256_hex(&cred.key);

        if let Some(cached) = self.validation.peek(&key_hash)
            && cached.freshness == Freshness::Fresh
        {
            return Ok(UserCtx {
                user_sub: cached.value,
                credential: CodexCredential::ApiKey(cred.key),
            });
        }

        match self
            .codex
            .me(&CodexCredential::ApiKey(cred.key.clone()))
            .await
        {
            Ok(user) => {
                self.validation.insert(key_hash, user.id.clone());
                Ok(UserCtx {
                    user_sub: user.id,
                    credential: CodexCredential::ApiKey(cred.key),
                })
            }
            Err(CodexError::Unauthorized { .. }) => Err(IdentityError::CredentialsExpired),
            Err(e) => Err(IdentityError::Codex(e)),
        }
    }

    /// Mint a replacement key and swap it in atomically. Mint first, then write
    /// the new ciphertext (a single UPDATE via [`mint_and_store`]), then drop the
    /// old validation-cache entry. The old key is never cleared before the new
    /// one is stored, so a partial failure never leaves the user with no usable
    /// key. `authz` is a fresh mint authorization (the stored read-only key
    /// cannot mint), so callers pass the IdP bearer or a pasted `api-keys:write`
    /// key. Returns the resolved `codex_user_sub`.
    pub async fn regenerate(
        &self,
        db: &DatabaseConnection,
        dek: &Dek,
        kek_version: u32,
        user_id: &str,
        authz: &CodexCredential,
        key_name: &str,
    ) -> Result<String, IdentityError> {
        // Capture the outgoing key's cache key before the swap so we can drop it.
        let old_hash = users::get_codex_credential(db, user_id, dek)
            .await
            .ok()
            .flatten()
            .map(|c| sha256_hex(&c.key));

        let sub =
            mint_and_store(&self.codex, db, dek, kek_version, user_id, authz, key_name).await?;

        if let Some(hash) = old_hash {
            self.validation.invalidate(&hash);
        }
        Ok(sub)
    }

    /// Ensure the user has a usable Codex key, minting one only when none is
    /// stored or the stored one is revoked/expired — so a normal sign-in does
    /// not orphan a fresh key in Codex on every visit, while a revoked key still
    /// recovers. A transient Codex/DB error is propagated (the caller decides
    /// whether to treat sign-in mint as best-effort).
    pub async fn ensure_key(
        &self,
        db: &DatabaseConnection,
        dek: &Dek,
        kek_version: u32,
        user_id: &str,
        authz: &CodexCredential,
        key_name: &str,
    ) -> Result<KeyOutcome, IdentityError> {
        match self.resolve(db, dek, user_id).await {
            Ok(_) => Ok(KeyOutcome::Kept),
            Err(IdentityError::NoCredential | IdentityError::CredentialsExpired) => {
                let sub = self
                    .regenerate(db, dek, kek_version, user_id, authz, key_name)
                    .await?;
                Ok(KeyOutcome::Minted(sub))
            }
            Err(other) => Err(other),
        }
    }
}

/// Outcome of [`CodexIdentity::ensure_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// A valid key was already stored; nothing was minted.
    Kept,
    /// A new key was minted and stored; carries the resolved `codex_user_sub`.
    Minted(String),
}

/// Hex-encoded SHA-256 of a string, used as the validation-cache key so the
/// plaintext key is never a map key.
fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shisho_db::connect_in_memory;
    use shisho_db::repos::users::{
        OidcIdentity, find_or_create, get_codex_credential, set_codex_credential,
    };
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

    fn asks_for_ratings_write(req: &wiremock::Request) -> bool {
        serde_json::from_slice::<serde_json::Value>(&req.body)
            .ok()
            .and_then(|b| b["permissions"].as_array().cloned())
            .is_some_and(|p| p.iter().any(|v| v == "ratings:write"))
    }

    #[tokio::test]
    async fn a_refused_write_scope_falls_back_to_a_read_only_key() {
        let server = MockServer::start().await;
        // The authorizing key cannot grant ratings:write...
        Mock::given(method("POST"))
            .and(path("/api/v1/api-keys"))
            .and(asks_for_ratings_write)
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string("You don't have permission to grant: ratings:write"),
            )
            .expect(1)
            .mount(&server)
            .await;
        // ...so the retry asks for the read-only set, and sign-in succeeds.
        Mock::given(method("POST"))
            .and(path("/api/v1/api-keys"))
            .and(|req: &wiremock::Request| !asks_for_ratings_write(req))
            .respond_with(ResponseTemplate::new(201).set_body_json(mint_body()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/auth/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(me_body()))
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
            &CodexCredential::ApiKey("read-only-onboarding-key".into()),
            "shisho",
        )
        .await
        .expect("mint falls back");
        assert_eq!(sub, "codex-user-42");
    }

    #[tokio::test]
    async fn a_rejected_authorization_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/api-keys"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;

        let db = connect_in_memory().await;
        let dek = shisho_db::Dek::from_bytes([7u8; 32]);
        let user_id = a_user(&db).await;
        let err = mint_and_store(
            &codex(&server),
            &db,
            &dek,
            1,
            &user_id,
            &CodexCredential::ApiKey("revoked".into()),
            "shisho",
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, IdentityError::Codex(CodexError::Unauthorized { .. })),
            "{err:?}"
        );
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

    /// Store a validated Codex credential for `user_id`.
    async fn store_key(db: &shisho_db::DatabaseConnection, dek: &shisho_db::Dek, user_id: &str) {
        set_codex_credential(db, user_id, "codex-user-42", "codex_dead_beef", dek, 1)
            .await
            .unwrap();
    }

    fn me_ok() -> wiremock::Mock {
        Mock::given(method("GET"))
            .and(path("/api/v1/auth/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(me_body()))
    }

    #[tokio::test]
    async fn resolve_validates_via_me_once_within_ttl() {
        let server = MockServer::start().await;
        me_ok().expect(1).mount(&server).await;
        let db = connect_in_memory().await;
        let dek = shisho_db::Dek::from_bytes([3u8; 32]);
        let user_id = a_user(&db).await;
        store_key(&db, &dek, &user_id).await;

        let identity = CodexIdentity::new(codex(&server), Duration::from_secs(300));
        let first = identity.resolve(&db, &dek, &user_id).await.unwrap();
        assert_eq!(first.user_sub, "codex-user-42");
        assert!(
            matches!(first.credential, CodexCredential::ApiKey(ref k) if k == "codex_dead_beef")
        );

        // Second resolve within the TTL is served from cache: no second /me
        // (the mock's expect(1) is asserted on drop).
        let second = identity.resolve(&db, &dek, &user_id).await.unwrap();
        assert_eq!(second.user_sub, "codex-user-42");
    }

    #[tokio::test]
    async fn resolve_revalidates_when_not_fresh() {
        let server = MockServer::start().await;
        // revalidate_after = 0 ⇒ every entry is immediately stale ⇒ each resolve
        // re-validates against /me.
        me_ok().expect(2).mount(&server).await;
        let db = connect_in_memory().await;
        let dek = shisho_db::Dek::from_bytes([3u8; 32]);
        let user_id = a_user(&db).await;
        store_key(&db, &dek, &user_id).await;

        let identity = CodexIdentity::new(codex(&server), Duration::ZERO);
        identity.resolve(&db, &dek, &user_id).await.unwrap();
        identity.resolve(&db, &dek, &user_id).await.unwrap();
        // expect(2) asserts the re-validation on drop.
    }

    #[tokio::test]
    async fn resolve_maps_401_to_credentials_expired() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/auth/me"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let db = connect_in_memory().await;
        let dek = shisho_db::Dek::from_bytes([3u8; 32]);
        let user_id = a_user(&db).await;
        store_key(&db, &dek, &user_id).await;

        let identity = CodexIdentity::new(codex(&server), Duration::from_secs(300));
        let err = identity.resolve(&db, &dek, &user_id).await.unwrap_err();
        assert!(matches!(err, IdentityError::CredentialsExpired));
        assert!(err.reauth_message().is_some(), "surfaces a re-auth message");
    }

    #[tokio::test]
    async fn resolve_without_a_stored_credential_is_no_credential() {
        let server = MockServer::start().await;
        let db = connect_in_memory().await;
        let dek = shisho_db::Dek::from_bytes([3u8; 32]);
        let user_id = a_user(&db).await;

        let identity = CodexIdentity::new(codex(&server), Duration::from_secs(300));
        let err = identity.resolve(&db, &dek, &user_id).await.unwrap_err();
        assert!(matches!(err, IdentityError::NoCredential));
        assert!(err.reauth_message().is_none());
    }

    fn mint_ok() -> wiremock::Mock {
        Mock::given(method("POST"))
            .and(path("/api/v1/api-keys"))
            .respond_with(ResponseTemplate::new(201).set_body_json(mint_body()))
    }

    #[tokio::test]
    async fn regenerate_mints_and_swaps_the_stored_key() {
        let server = MockServer::start().await;
        mint_ok().mount(&server).await;
        me_ok().mount(&server).await;
        let db = connect_in_memory().await;
        let dek = shisho_db::Dek::from_bytes([4u8; 32]);
        let user_id = a_user(&db).await;
        set_codex_credential(&db, &user_id, "codex-user-42", "old_key", &dek, 1)
            .await
            .unwrap();

        let identity = CodexIdentity::new(codex(&server), Duration::from_secs(300));
        let sub = identity
            .regenerate(
                &db,
                &dek,
                1,
                &user_id,
                &CodexCredential::Bearer("idp-token".into()),
                "shisho",
            )
            .await
            .unwrap();
        assert_eq!(sub, "codex-user-42");

        // The stored ciphertext is the newly minted key, not the old one.
        let cred = get_codex_credential(&db, &user_id, &dek)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cred.key, "codex_dead_beef");
    }

    #[tokio::test]
    async fn ensure_key_keeps_a_valid_stored_key() {
        let server = MockServer::start().await;
        // Only /me is stubbed: any mint attempt would 404 and fail, so a `Kept`
        // outcome proves no re-mint happened.
        me_ok().mount(&server).await;
        let db = connect_in_memory().await;
        let dek = shisho_db::Dek::from_bytes([4u8; 32]);
        let user_id = a_user(&db).await;
        set_codex_credential(&db, &user_id, "codex-user-42", "codex_dead_beef", &dek, 1)
            .await
            .unwrap();

        let identity = CodexIdentity::new(codex(&server), Duration::from_secs(300));
        let outcome = identity
            .ensure_key(
                &db,
                &dek,
                1,
                &user_id,
                &CodexCredential::Bearer("idp-token".into()),
                "shisho",
            )
            .await
            .unwrap();
        assert_eq!(outcome, KeyOutcome::Kept);
    }

    #[tokio::test]
    async fn ensure_key_mints_when_none_is_stored() {
        let server = MockServer::start().await;
        mint_ok().mount(&server).await;
        me_ok().mount(&server).await;
        let db = connect_in_memory().await;
        let dek = shisho_db::Dek::from_bytes([4u8; 32]);
        let user_id = a_user(&db).await;

        let identity = CodexIdentity::new(codex(&server), Duration::from_secs(300));
        let outcome = identity
            .ensure_key(
                &db,
                &dek,
                1,
                &user_id,
                &CodexCredential::Bearer("idp-token".into()),
                "shisho",
            )
            .await
            .unwrap();
        assert_eq!(outcome, KeyOutcome::Minted("codex-user-42".to_owned()));
        let cred = get_codex_credential(&db, &user_id, &dek)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cred.key, "codex_dead_beef");
    }
}
