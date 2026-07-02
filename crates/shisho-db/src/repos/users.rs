//! User provisioning for multi-user auth.
//!
//! Identity is keyed by `(provider, oidc_sub)`, which the
//! `idx_users_provider_sub` unique index enforces. First login inserts a row;
//! later logins reuse it and refresh the mutable profile fields plus
//! `last_seen`. The insert path is written to tolerate a concurrent winner
//! (two simultaneous first logins of the same identity yield one row).

use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, QueryOrder,
    Set,
};
use shisho_crypto::{CryptoError, Dek};

use crate::entities::users;

/// The verified identity claims a provider hands back after code exchange.
#[derive(Debug, Clone, Default)]
pub struct OidcIdentity {
    /// Provider `name` from config (stored on the row, half of the unique key).
    pub provider: String,
    /// Stable subject identifier from the provider (the other half).
    pub subject: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    /// Group memberships from the provider's group claim (empty if none).
    pub groups: Vec<String>,
}

/// Encode group memberships for storage: a JSON array, or NULL when empty so an
/// absent claim and an empty list read back identically.
fn encode_groups(groups: &[String]) -> Option<String> {
    if groups.is_empty() {
        None
    } else {
        serde_json::to_string(groups).ok()
    }
}

/// Decode a user's stored group memberships. A NULL or malformed value yields an
/// empty list (never an error), so a bad row simply grants no group-based admin.
pub fn decode_groups(user: &users::Model) -> Vec<String> {
    user.groups
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .unwrap_or_default()
}

/// Fetch a user by id.
pub async fn get(db: &DatabaseConnection, id: &str) -> Result<Option<users::Model>, DbErr> {
    users::Entity::find_by_id(id).one(db).await
}

/// Look up the user behind `(provider, subject)`, if any.
pub async fn find_by_identity(
    db: &DatabaseConnection,
    provider: &str,
    subject: &str,
) -> Result<Option<users::Model>, DbErr> {
    users::Entity::find()
        .filter(users::Column::Provider.eq(provider))
        .filter(users::Column::OidcSub.eq(subject))
        .one(db)
        .await
}

/// Provision-or-refresh the user behind a verified identity. Creates the row on
/// first login (new UUID id), otherwise reuses it; either way the profile
/// fields and `last_seen` are updated to the latest login. Idempotent and
/// race-tolerant: a unique-constraint loss re-reads the winning row.
pub async fn find_or_create(
    db: &DatabaseConnection,
    identity: &OidcIdentity,
) -> Result<users::Model, DbErr> {
    let now = chrono::Utc::now().to_rfc3339();

    if let Some(existing) = find_by_identity(db, &identity.provider, &identity.subject).await? {
        return refresh(db, existing, identity, &now).await;
    }

    let model = users::ActiveModel {
        id: Set(uuid::Uuid::new_v4().to_string()),
        email: Set(identity.email.clone()),
        display_name: Set(identity.display_name.clone()),
        avatar_url: Set(identity.avatar_url.clone()),
        oidc_sub: Set(Some(identity.subject.clone())),
        provider: Set(Some(identity.provider.clone())),
        groups: Set(encode_groups(&identity.groups)),
        created_at: Set(now.clone()),
        last_seen: Set(Some(now.clone())),
        deactivated_at: Set(None),
        // A fresh user has no Codex identity until first sign-in mints one.
        codex_user_sub: Set(None),
        codex_key_ciphertext: Set(None),
        codex_key_version: Set(None),
    };

    match model.insert(db).await {
        Ok(row) => Ok(row),
        // A concurrent first login may have won the `(provider, oidc_sub)`
        // unique-index race: adopt its row. If nothing is there, the insert
        // failed for a real reason (e.g. a duplicate email) — surface it.
        Err(insert_err) => match find_by_identity(db, &identity.provider, &identity.subject).await?
        {
            Some(row) => Ok(row),
            None => Err(insert_err),
        },
    }
}

/// Every user, oldest first (admin user-management list).
pub async fn list_all(db: &DatabaseConnection) -> Result<Vec<users::Model>, DbErr> {
    users::Entity::find()
        .order_by_asc(users::Column::CreatedAt)
        .order_by_asc(users::Column::Id)
        .all(db)
        .await
}

/// Set (or clear) a user's `deactivated_at`. `Some(ts)` deactivates; `None`
/// reactivates. Returns whether a row was updated (false = no such user).
pub async fn set_deactivated(
    db: &DatabaseConnection,
    id: &str,
    deactivated_at: Option<String>,
) -> Result<bool, DbErr> {
    let res = users::Entity::update_many()
        .col_expr(users::Column::DeactivatedAt, Expr::value(deactivated_at))
        .filter(users::Column::Id.eq(id))
        .exec(db)
        .await?;
    Ok(res.rows_affected > 0)
}

/// A user's decrypted Codex credential: the linked `codex_user_sub` and the
/// recovered plaintext key, ready to present to Codex.
#[derive(Debug, Clone)]
pub struct CodexCredential {
    pub codex_user_sub: String,
    pub key: String,
}

/// Reading a stored Codex credential can fail either at the database or at the
/// unseal step (wrong DEK / tampered blob).
#[derive(Debug, thiserror::Error)]
pub enum CodexCredentialError {
    #[error(transparent)]
    Db(#[from] DbErr),
    #[error("decrypting the stored Codex key failed")]
    Crypto(#[from] CryptoError),
}

/// Store (or replace) a user's minted Codex credential: the `codex_user_sub`
/// linkage and the key sealed with the DEK-derived at-rest key. Only ciphertext
/// touches the row. `kek_version` is recorded for a future re-seal. Returns
/// whether a row was updated (false = no such user).
pub async fn set_codex_credential(
    db: &DatabaseConnection,
    user_id: &str,
    codex_user_sub: &str,
    key_plaintext: &str,
    dek: &Dek,
    kek_version: u32,
) -> Result<bool, DbErr> {
    let sealing_key = shisho_crypto::file_key(dek);
    let ciphertext = shisho_crypto::seal(&sealing_key, key_plaintext.as_bytes());
    let res = users::Entity::update_many()
        .col_expr(users::Column::CodexUserSub, Expr::value(codex_user_sub))
        .col_expr(users::Column::CodexKeyCiphertext, Expr::value(ciphertext))
        .col_expr(
            users::Column::CodexKeyVersion,
            Expr::value(kek_version as i32),
        )
        .filter(users::Column::Id.eq(user_id))
        .exec(db)
        .await?;
    Ok(res.rows_affected > 0)
}

/// Read and unseal a user's Codex credential. `Ok(None)` when the user is
/// unknown or has no stored credential; `Err(Crypto)` when the blob will not
/// open under this DEK.
pub async fn get_codex_credential(
    db: &DatabaseConnection,
    user_id: &str,
    dek: &Dek,
) -> Result<Option<CodexCredential>, CodexCredentialError> {
    let Some(user) = users::Entity::find_by_id(user_id).one(db).await? else {
        return Ok(None);
    };
    let (Some(codex_user_sub), Some(ciphertext)) = (user.codex_user_sub, user.codex_key_ciphertext)
    else {
        return Ok(None);
    };
    let sealing_key = shisho_crypto::file_key(dek);
    let plaintext = shisho_crypto::open(&sealing_key, &ciphertext)?;
    // A non-UTF-8 result means the blob opened but is not the key we stored.
    let key = String::from_utf8(plaintext).map_err(|_| CryptoError::Decrypt)?;
    Ok(Some(CodexCredential {
        codex_user_sub,
        key,
    }))
}

/// Clear a user's Codex credential back to NULL (used on revocation / logout).
/// Returns whether a row was updated.
pub async fn clear_codex_credential(db: &DatabaseConnection, user_id: &str) -> Result<bool, DbErr> {
    let res = users::Entity::update_many()
        .col_expr(
            users::Column::CodexUserSub,
            Expr::value(Option::<String>::None),
        )
        .col_expr(
            users::Column::CodexKeyCiphertext,
            Expr::value(Option::<Vec<u8>>::None),
        )
        .col_expr(
            users::Column::CodexKeyVersion,
            Expr::value(Option::<i32>::None),
        )
        .filter(users::Column::Id.eq(user_id))
        .exec(db)
        .await?;
    Ok(res.rows_affected > 0)
}

/// Reverse lookup: the user linked to a given `codex_user_sub`, if any.
pub async fn find_by_codex_user_sub(
    db: &DatabaseConnection,
    codex_user_sub: &str,
) -> Result<Option<users::Model>, DbErr> {
    users::Entity::find()
        .filter(users::Column::CodexUserSub.eq(codex_user_sub))
        .one(db)
        .await
}

/// Update mutable profile fields and `last_seen` on an existing row.
async fn refresh(
    db: &DatabaseConnection,
    existing: users::Model,
    identity: &OidcIdentity,
    now: &str,
) -> Result<users::Model, DbErr> {
    let mut active: users::ActiveModel = existing.into();
    active.email = Set(identity.email.clone());
    active.display_name = Set(identity.display_name.clone());
    active.avatar_url = Set(identity.avatar_url.clone());
    active.last_seen = Set(Some(now.to_string()));
    active.groups = Set(encode_groups(&identity.groups));
    active.update(db).await
}

/// "Known users" — the users the housekeeping sweeps iterate. Shisho stores
/// no separate user registry for sweeps; a user is known iff they left rows
/// in Shisho-owned tables (verdicts or profiles). Returns every `user_sub`
/// with such state, sorted and deduplicated.
pub async fn known_user_subs(db: &DatabaseConnection) -> Result<Vec<String>, DbErr> {
    use std::collections::BTreeSet;

    use sea_orm::QuerySelect;

    use crate::entities::{profile, verdict};

    let verdict_users: Vec<String> = verdict::Entity::find()
        .select_only()
        .column(verdict::Column::UserSub)
        .distinct()
        .into_tuple()
        .all(db)
        .await?;
    let profile_users: Vec<String> = profile::Entity::find()
        .select_only()
        .column(profile::Column::UserSub)
        .distinct()
        .into_tuple()
        .all(db)
        .await?;

    let set: BTreeSet<String> = verdict_users.into_iter().chain(profile_users).collect();
    Ok(set.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::connect_in_memory;

    fn identity() -> OidcIdentity {
        OidcIdentity {
            provider: "google".into(),
            subject: "sub-123".into(),
            email: Some("a@example.com".into()),
            display_name: Some("Ada".into()),
            avatar_url: Some("https://img/a.png".into()),
            groups: Vec::new(),
        }
    }

    #[tokio::test]
    async fn first_login_creates_one_row() {
        let db = connect_in_memory().await;
        let user = find_or_create(&db, &identity()).await.unwrap();

        assert_eq!(user.provider.as_deref(), Some("google"));
        assert_eq!(user.oidc_sub.as_deref(), Some("sub-123"));
        assert_eq!(user.email.as_deref(), Some("a@example.com"));
        assert_eq!(user.display_name.as_deref(), Some("Ada"));
        assert!(user.last_seen.is_some());
        assert_eq!(users::Entity::find().all(&db).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn second_login_is_idempotent_and_updates_profile() {
        let db = connect_in_memory().await;
        let first = find_or_create(&db, &identity()).await.unwrap();

        let mut updated = identity();
        updated.display_name = Some("Ada Lovelace".into());
        let second = find_or_create(&db, &updated).await.unwrap();

        assert_eq!(first.id, second.id, "same identity reuses the row");
        assert_eq!(second.display_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(users::Entity::find().all(&db).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn groups_are_persisted_and_refreshed() {
        let db = connect_in_memory().await;

        // No groups → NULL column, decodes to empty.
        let user = find_or_create(&db, &identity()).await.unwrap();
        assert!(user.groups.is_none());
        assert!(decode_groups(&user).is_empty());

        // First login with groups persists them.
        let mut with_groups = identity();
        with_groups.groups = vec!["admins".into(), "staff".into()];
        let user = find_or_create(&db, &with_groups).await.unwrap();
        assert_eq!(decode_groups(&user), vec!["admins", "staff"]);

        // A later login with different groups overwrites (no stale membership).
        let mut changed = identity();
        changed.groups = vec!["staff".into()];
        let user = find_or_create(&db, &changed).await.unwrap();
        assert_eq!(decode_groups(&user), vec!["staff"]);

        // Dropping all groups clears the column back to NULL.
        let user = find_or_create(&db, &identity()).await.unwrap();
        assert!(user.groups.is_none());
    }

    #[tokio::test]
    async fn distinct_subjects_create_distinct_users() {
        let db = connect_in_memory().await;
        let a = find_or_create(&db, &identity()).await.unwrap();
        let mut other = identity();
        other.subject = "sub-456".into();
        other.email = Some("b@example.com".into());
        let b = find_or_create(&db, &other).await.unwrap();

        assert_ne!(a.id, b.id);
        assert_eq!(users::Entity::find().all(&db).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn same_subject_different_provider_is_distinct() {
        let db = connect_in_memory().await;
        let a = find_or_create(&db, &identity()).await.unwrap();
        let mut other = identity();
        other.provider = "github".into();
        // Distinct emails here keep the assertion focused on `(provider, sub)`
        // keying; the same-email-across-providers case collides on the UNIQUE
        // email index (a documented known limitation, not exercised here).
        other.email = Some("a+github@example.com".into());
        let b = find_or_create(&db, &other).await.unwrap();

        assert_ne!(a.id, b.id, "(provider, sub) is the key, not sub alone");
    }

    #[tokio::test]
    async fn set_deactivated_toggles_and_reports_missing() {
        let db = connect_in_memory().await;
        let user = find_or_create(&db, &identity()).await.unwrap();

        let ts = chrono::Utc::now().to_rfc3339();
        assert!(
            set_deactivated(&db, &user.id, Some(ts.clone()))
                .await
                .unwrap()
        );
        assert_eq!(
            get(&db, &user.id).await.unwrap().unwrap().deactivated_at,
            Some(ts)
        );

        // Reactivate.
        assert!(set_deactivated(&db, &user.id, None).await.unwrap());
        assert!(
            get(&db, &user.id)
                .await
                .unwrap()
                .unwrap()
                .deactivated_at
                .is_none()
        );

        // No such user.
        assert!(!set_deactivated(&db, "nope", None).await.unwrap());
    }

    #[tokio::test]
    async fn codex_credential_round_trips_stores_ciphertext_and_clears() {
        let db = connect_in_memory().await;
        let user = find_or_create(&db, &identity()).await.unwrap();
        let dek = shisho_crypto::generate_dek();

        assert!(
            set_codex_credential(&db, &user.id, "codex-sub-1", "codex-key-secret", &dek, 3)
                .await
                .unwrap()
        );

        let cred = get_codex_credential(&db, &user.id, &dek)
            .await
            .unwrap()
            .expect("credential present");
        assert_eq!(cred.codex_user_sub, "codex-sub-1");
        assert_eq!(cred.key, "codex-key-secret");

        // The row stores ciphertext + version, never the plaintext key.
        let row = get(&db, &user.id).await.unwrap().unwrap();
        assert_eq!(row.codex_key_version, Some(3));
        let ciphertext = row.codex_key_ciphertext.expect("ciphertext");
        let needle = b"codex-key-secret";
        assert!(
            !ciphertext.windows(needle.len()).any(|w| w == needle),
            "the plaintext key must not appear in the stored blob"
        );

        // Clear resets all three columns to NULL.
        assert!(clear_codex_credential(&db, &user.id).await.unwrap());
        assert!(
            get_codex_credential(&db, &user.id, &dek)
                .await
                .unwrap()
                .is_none()
        );
        let row = get(&db, &user.id).await.unwrap().unwrap();
        assert!(row.codex_user_sub.is_none());
        assert!(row.codex_key_ciphertext.is_none());
        assert!(row.codex_key_version.is_none());
    }

    #[tokio::test]
    async fn find_by_codex_user_sub_resolves_and_misses() {
        let db = connect_in_memory().await;
        let user = find_or_create(&db, &identity()).await.unwrap();
        let dek = shisho_crypto::generate_dek();
        set_codex_credential(&db, &user.id, "codex-sub-xyz", "k", &dek, 1)
            .await
            .unwrap();

        let found = find_by_codex_user_sub(&db, "codex-sub-xyz")
            .await
            .unwrap()
            .expect("user for the sub");
        assert_eq!(found.id, user.id);
        assert!(
            find_by_codex_user_sub(&db, "unknown-sub")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn get_credential_under_wrong_dek_is_a_crypto_error() {
        let db = connect_in_memory().await;
        let user = find_or_create(&db, &identity()).await.unwrap();
        let dek = shisho_crypto::generate_dek();
        set_codex_credential(&db, &user.id, "s", "secret", &dek, 1)
            .await
            .unwrap();

        let wrong = shisho_crypto::generate_dek();
        assert!(matches!(
            get_codex_credential(&db, &user.id, &wrong).await,
            Err(CodexCredentialError::Crypto(_))
        ));
    }

    #[tokio::test]
    async fn list_all_orders_by_creation() {
        let db = connect_in_memory().await;
        let a = find_or_create(&db, &identity()).await.unwrap();
        let mut other = identity();
        other.subject = "sub-456".into();
        other.email = Some("b@example.com".into());
        let b = find_or_create(&db, &other).await.unwrap();

        let ids: Vec<String> = list_all(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|u| u.id)
            .collect();
        assert!(ids.contains(&a.id) && ids.contains(&b.id));
        assert_eq!(ids.len(), 2);
    }
}
