//! First-run key generation: a fresh instance with no configured key and no
//! stored keys gets an admin key generated and printed once.

use sea_orm::{DatabaseConnection, EntityTrait, PaginatorTrait};
use shisho_config::AppConfig;

use crate::keys::Scope;
use crate::repo;
use shisho_db::entities::api_keys;

/// If no `auth.api_key` is configured and the `api_keys` table is empty,
/// create the local user, store an admin-equivalent key named `bootstrap`
/// (the full granular scope set, including `keys:manage`), and return the
/// plaintext for the caller to print. Returns `None` (and does nothing) in
/// every other case, so restarts never mint a second key.
///
/// The plaintext is returned rather than printed here so tests never depend
/// on stdout capture.
pub async fn ensure_first_run_key(
    db: &DatabaseConnection,
    cfg: &AppConfig,
) -> anyhow::Result<Option<String>> {
    if cfg.auth.api_key.is_some() {
        return Ok(None);
    }
    if api_keys::Entity::find().count(db).await? > 0 {
        return Ok(None);
    }

    let user_id = repo::ensure_local_user(db).await?;
    let (_, plaintext) =
        repo::create_key(db, Some(user_id), "bootstrap", &Scope::all(), None).await?;
    Ok(Some(plaintext))
}

#[cfg(test)]
mod tests {
    use sea_orm::EntityTrait;

    use super::*;
    use crate::keys::{Scope, scopes_from_json, verify_key};
    use crate::test_util::{seed_key, test_db};
    use shisho_db::entities::{api_keys, users};

    #[tokio::test]
    async fn generates_key_on_empty_db() {
        let db = test_db().await;
        let cfg = AppConfig::default();

        let plaintext = ensure_first_run_key(&db, &cfg)
            .await
            .unwrap()
            .expect("fresh db must generate a key");
        assert!(plaintext.starts_with("shisho_"));

        let user_count = users::Entity::find().all(&db).await.unwrap().len();
        assert_eq!(user_count, 1, "exactly one local user row");

        let rows = api_keys::Entity::find().all(&db).await.unwrap();
        assert_eq!(rows.len(), 1, "exactly one bootstrap key row");
        let row = &rows[0];
        assert_eq!(row.name, "bootstrap");
        assert!(row.user_id.is_some(), "key attached to the local user");
        assert_eq!(
            scopes_from_json(&row.scopes).unwrap(),
            Scope::all(),
            "bootstrap key holds the full granular scope set"
        );
        assert!(
            scopes_from_json(&row.scopes)
                .unwrap()
                .contains(&Scope::KEYS_MANAGE),
            "admin-equivalent bootstrap key can manage keys"
        );
        assert!(
            verify_key(&plaintext, &row.key_hash),
            "returned plaintext must verify against the stored hash"
        );
    }

    #[tokio::test]
    async fn idempotent_across_restarts() {
        let db = test_db().await;
        let cfg = AppConfig::default();

        assert!(ensure_first_run_key(&db, &cfg).await.unwrap().is_some());
        assert!(
            ensure_first_run_key(&db, &cfg).await.unwrap().is_none(),
            "a second run must not mint another key"
        );

        let rows = api_keys::Entity::find().all(&db).await.unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn skipped_when_config_key_set() {
        let db = test_db().await;
        let mut cfg = AppConfig::default();
        cfg.auth.api_key = Some("shisho_configured".into());

        assert!(ensure_first_run_key(&db, &cfg).await.unwrap().is_none());
        assert!(api_keys::Entity::find().all(&db).await.unwrap().is_empty());
        assert!(users::Entity::find().all(&db).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn skipped_when_keys_exist() {
        let db = test_db().await;
        seed_key(&db, "existing", &[Scope::VERDICTS_READ], None).await;

        let cfg = AppConfig::default();
        assert!(ensure_first_run_key(&db, &cfg).await.unwrap().is_none());
        assert_eq!(api_keys::Entity::find().all(&db).await.unwrap().len(), 1);
    }
}
