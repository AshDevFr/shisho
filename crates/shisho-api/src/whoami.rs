//! `GET /api/v1/whoami`: echo the authenticated identity. The simplest
//! protected endpoint — it requires authentication but no particular scope, so
//! any valid credential (API key, session cookie, or OAuth access token) can
//! confirm "who am I and what can I do". A handy first call for a new client.

use axum::Json;
use axum::extract::Extension;
use serde::Serialize;
use utoipa::ToSchema;

use crate::middleware::AuthContext;
use shisho_auth::keys::Scope;

/// The caller's verified identity. `keyId`/`userId` are null when the static
/// config key authenticated (it lives outside the database).
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WhoAmI {
    pub key_id: Option<String>,
    pub user_id: Option<String>,
    pub scopes: Vec<Scope>,
}

/// Return the authenticated caller's key id, user id, and granted scopes.
#[utoipa::path(
    get,
    path = "/api/v1/whoami",
    tag = "shisho",
    responses(
        (status = 200, body = WhoAmI),
        (status = 401, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn whoami(Extension(ctx): Extension<AuthContext>) -> Json<WhoAmI> {
    Json(WhoAmI {
        key_id: ctx.key_id,
        user_id: ctx.user_id,
        scopes: ctx.scopes,
    })
}
