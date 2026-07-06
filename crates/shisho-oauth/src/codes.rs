//! Authorization codes: short-lived, single-use grants minted at the consent
//! step and exchanged for tokens at `/token`.

use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, Set,
};
use shisho_db::entities::oauth_auth_codes;

/// Everything pinned onto a freshly minted code.
pub struct NewCode<'a> {
    /// SHA-256 hex of the code (see [`crate::secret`]).
    pub code_hash: &'a str,
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub code_challenge: &'a str,
    pub user_id: &'a str,
    /// JSON array of scope strings.
    pub scopes: &'a str,
    /// ISO 8601 expiry.
    pub expires_at: &'a str,
}

/// Persist a new authorization code.
pub async fn issue(db: &DatabaseConnection, code: NewCode<'_>) -> Result<(), DbErr> {
    let model = oauth_auth_codes::ActiveModel {
        code_hash: Set(code.code_hash.to_string()),
        client_id: Set(code.client_id.to_string()),
        redirect_uri: Set(code.redirect_uri.to_string()),
        code_challenge: Set(code.code_challenge.to_string()),
        user_id: Set(code.user_id.to_string()),
        scopes: Set(code.scopes.to_string()),
        expires_at: Set(code.expires_at.to_string()),
        used: Set(false),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
    };
    model.insert(db).await.map(|_| ())
}

/// Atomically consume a code: returns the row only if it exists, is unused, and
/// is unexpired, marking it used in the same step. A replayed or expired code
/// yields `None`. The marking is a guarded update (`used = true WHERE used =
/// false`), so two concurrent exchanges of one code cannot both win.
pub async fn consume(
    db: &DatabaseConnection,
    code_hash: &str,
    now: &str,
) -> Result<Option<oauth_auth_codes::Model>, DbErr> {
    let Some(model) = oauth_auth_codes::Entity::find_by_id(code_hash)
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    if model.used || model.expires_at.as_str() <= now {
        return Ok(None);
    }
    let res = oauth_auth_codes::Entity::update_many()
        .col_expr(
            oauth_auth_codes::Column::Used,
            sea_orm::sea_query::Expr::value(true),
        )
        .filter(oauth_auth_codes::Column::CodeHash.eq(code_hash))
        .filter(oauth_auth_codes::Column::Used.eq(false))
        .exec(db)
        .await?;
    if res.rows_affected == 1 {
        Ok(Some(model))
    } else {
        // Lost the race to a concurrent exchange.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shisho_db::conn::connect_in_memory;

    async fn seed(db: &DatabaseConnection, code_hash: &str, expires_at: &str) {
        issue(
            db,
            NewCode {
                code_hash,
                client_id: "client-1",
                redirect_uri: "https://app.example/cb",
                code_challenge: "challenge",
                user_id: "alice",
                scopes: r#"["verdicts:read"]"#,
                expires_at,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn consume_succeeds_once_then_rejects_replay() {
        let db = connect_in_memory().await;
        seed(&db, "h1", "2999-01-01T00:00:00+00:00").await;

        let first = consume(&db, "h1", "2026-06-17T00:00:00+00:00")
            .await
            .unwrap();
        assert!(first.is_some(), "first consume wins");

        let replay = consume(&db, "h1", "2026-06-17T00:00:00+00:00")
            .await
            .unwrap();
        assert!(replay.is_none(), "a used code cannot be replayed");
    }

    #[tokio::test]
    async fn expired_code_is_rejected() {
        let db = connect_in_memory().await;
        seed(&db, "h2", "2000-01-01T00:00:00+00:00").await;
        let got = consume(&db, "h2", "2026-06-17T00:00:00+00:00")
            .await
            .unwrap();
        assert!(got.is_none(), "an expired code is not consumable");
    }

    #[tokio::test]
    async fn unknown_code_is_none() {
        let db = connect_in_memory().await;
        assert!(
            consume(&db, "missing", "2026-06-17T00:00:00+00:00")
                .await
                .unwrap()
                .is_none()
        );
    }
}
