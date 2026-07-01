//! Admin user-management endpoints.
//!
//! All routes gate on [`require_admin`]: in single-user mode the sole operator
//! is the admin; in multi-user mode admin is the `admin_emails`/`admin_groups`
//! allowlists, or the first provisioned user when both are empty.
//!
//! - `GET  /api/v1/admin/users` - list users with last-seen, admin flag, and
//!   deactivation state.
//! - `POST /api/v1/admin/users/{id}/deactivate` - lock a user out (sessions and
//!   API keys stop working); an admin cannot deactivate themselves.
//! - `POST /api/v1/admin/users/{id}/activate` - restore access.
//! - `GET  /api/v1/admin/encryption` - at-rest encryption status (no key material).

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::error::AppError;
use super::middleware::{AuthContext, require_admin};
use shisho_db::entities::users;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin/users", get(list_users))
        .route("/admin/users/{id}/deactivate", post(deactivate_user))
        .route("/admin/users/{id}/activate", post(activate_user))
        .route("/admin/encryption", get(encryption_status))
}

/// Read-only encryption status. Never includes key material: key management is
/// a deployment-layer concern (config + the `shisho encryption` CLI).
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EncryptionStatus {
    /// Whether at-rest encryption is configured.
    pub enabled: bool,
    /// The active KEK version wrapping the data key, if a keyfile exists.
    pub kek_version: Option<u32>,
    /// When the data key was last (re)wrapped (first init or KEK rotation).
    pub last_rotated_at: Option<String>,
}

/// Encryption status (admin only). Reports whether at-rest encryption is on, the
/// active KEK version, and when it was last rotated. Never returns key material.
#[utoipa::path(
    get,
    path = "/api/v1/admin/encryption",
    tag = "admin",
    responses(
        (status = 200, body = EncryptionStatus),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody, description = "Caller is not an admin")
    ),
    security(("bearer" = []))
)]
pub async fn encryption_status(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<EncryptionStatus>, AppError> {
    require_admin(&state, &ctx).await?;
    let st = shisho_db::keys::status(&state.cfg);
    Ok(Json(EncryptionStatus {
        enabled: st.enabled,
        kek_version: st.kek_version,
        last_rotated_at: st.last_rotated_at,
    }))
}

/// One user in the admin list.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminUser {
    pub id: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub provider: Option<String>,
    pub last_seen: Option<String>,
    /// NULL when active; the deactivation timestamp otherwise.
    pub deactivated_at: Option<String>,
    pub is_admin: bool,
}

/// List all users (admin only).
#[utoipa::path(
    get,
    path = "/api/v1/admin/users",
    tag = "admin",
    responses(
        (status = 200, body = [AdminUser]),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody, description = "Caller is not an admin")
    ),
    security(("bearer" = []))
)]
pub async fn list_users(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<Vec<AdminUser>>, AppError> {
    require_admin(&state, &ctx).await?;

    let rows = shisho_db::repos::users::list_all(&state.db)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;

    let mut out = Vec::with_capacity(rows.len());
    for user in rows {
        let is_admin = shisho_auth::admin::is_admin(
            &state.db,
            &state.cfg.auth.admin_emails,
            &state.cfg.auth.admin_groups,
            &user,
        )
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
        out.push(AdminUser {
            id: user.id,
            email: user.email,
            display_name: user.display_name,
            avatar_url: user.avatar_url,
            provider: user.provider,
            last_seen: user.last_seen,
            deactivated_at: user.deactivated_at,
            is_admin,
        });
    }
    Ok(Json(out))
}

/// Load the target user or 404.
async fn target_user(state: &AppState, id: &str) -> Result<users::Model, AppError> {
    shisho_db::repos::users::get(&state.db, id)
        .await
        .map_err(|e| AppError::Internal(e.into()))?
        .ok_or_else(|| AppError::NotFound(format!("user {id} not found")))
}

/// Deactivate a user (admin only). An admin cannot deactivate themselves, which
/// would risk locking the instance out of its only admin.
#[utoipa::path(
    post,
    path = "/api/v1/admin/users/{id}/deactivate",
    tag = "admin",
    params(("id" = String, Path, description = "Target user id")),
    responses(
        (status = 204),
        (status = 400, body = super::error::ErrorBody, description = "Cannot deactivate yourself"),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 404, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn deactivate_user(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let admin = require_admin(&state, &ctx).await?;
    if admin.id == id {
        return Err(AppError::BadRequest(
            "you cannot deactivate your own account".into(),
        ));
    }
    target_user(&state, &id).await?;

    shisho_db::repos::users::set_deactivated(&state.db, &id, Some(chrono::Utc::now().to_rfc3339()))
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Reactivate a user (admin only): clears `deactivated_at`.
#[utoipa::path(
    post,
    path = "/api/v1/admin/users/{id}/activate",
    tag = "admin",
    params(("id" = String, Path, description = "Target user id")),
    responses(
        (status = 204),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 404, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn activate_user(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    require_admin(&state, &ctx).await?;
    target_user(&state, &id).await?;

    shisho_db::repos::users::set_deactivated(&state.db, &id, None)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate as api;
    use crate::test_util::test_state;
    use shisho_auth::keys::Scope;

    const ADMIN_KEY: &str = "shisho_admin_key";

    /// Single-user mode: the config key is the implicit admin.
    async fn admin_app() -> (axum::Router, std::sync::Arc<api::AppState>) {
        let mut cfg = shisho_config::AppConfig::default();
        cfg.auth.api_key = Some(ADMIN_KEY.into());
        let state = test_state(cfg).await;
        (api::router(state.clone()), state)
    }

    fn get(uri: &str, bearer: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn encryption_status_reports_disabled_by_default() {
        let (app, _) = admin_app().await;
        let resp = app
            .oneshot(get("/api/v1/admin/encryption", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["enabled"], false);
    }

    #[tokio::test]
    async fn admin_routes_forbidden_for_non_admin_in_multi_user() {
        // Multi-user mode: an unowned service key is not an admin.
        let mut cfg = shisho_config::AppConfig::default();
        cfg.auth.mode = shisho_config::AuthMode::MultiUser;
        let state = test_state(cfg).await;
        let (key, _) = crate::test_util::seed_key(&state.db, "svc", &Scope::all(), None).await;
        let app = api::router(state);

        let resp = app.oneshot(get("/api/v1/admin/users", &key)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}
