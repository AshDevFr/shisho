//! Connected-application management, `/api/v1/connections`. A signed-in user
//! sees which OAuth clients (e.g. an MCP connector) hold a live grant on their
//! account, and revokes one.
//!
//! Authorization mirrors key management: the router gates on `keys:manage`. A
//! browser session carries the full scope set, so the human reaches it, while
//! an OAuth connector's own access token (connector scopes only, no
//! `keys:manage`) cannot enumerate or revoke grants it was never consented to
//! manage.
//!
//! A connection is one *client*, not one token row: a re-authorization or an
//! in-flight refresh can leave several live rows for the same client, so the
//! list groups by client and revocation deletes every row for it.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router, middleware};
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::error::AppError;
use crate::middleware::{CurrentScope, require_keys_manage};
use shisho_auth::keys::{Scope, scopes_from_json};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_connections))
        .route("/{client_id}", delete(revoke_connection))
        .layer(middleware::from_fn(require_keys_manage))
}

/// A client holding a live grant on the current user's account.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionResponse {
    pub client_id: String,
    /// Display name from dynamic registration, if the client provided one.
    pub client_name: Option<String>,
    pub scopes: Vec<Scope>,
    /// When this client was first authorized (ISO 8601, earliest live grant).
    pub connected_at: String,
    /// When the connection lapses if the client stops refreshing: the latest
    /// refresh-token expiry across its live grants (ISO 8601). Not the
    /// short-lived access-token expiry, which the client renews automatically;
    /// the refresh token (rotated on each use) is the grant's real lifetime.
    pub expires_at: String,
}

/// List the OAuth clients connected to the caller's account, one row per client.
#[utoipa::path(
    get,
    path = "/api/v1/connections",
    tag = "connections",
    responses(
        (status = 200, body = [ConnectionResponse]),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn list_connections(
    State(state): State<Arc<AppState>>,
    CurrentScope(scope): CurrentScope,
) -> Result<Json<Vec<ConnectionResponse>>, AppError> {
    // No owning user (single-user config key, or an unowned admin/service key):
    // OAuth grants are per-user, so there is nothing personal to list.
    let Some(user_id) = scope.owner() else {
        return Ok(Json(vec![]));
    };

    let rows = shisho_oauth::tokens::list_by_user(&state.db, &user_id).await?;

    // Rows arrive newest-first. Fold them into one entry per client: keep the
    // newest row's scopes, the earliest grant time, and the latest refresh
    // expiry (the grant's real lifetime; the access token is renewed off it).
    struct Group {
        scopes: String,
        connected_at: String,
        expires_at: String,
    }
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Group> = HashMap::new();
    for row in rows {
        match groups.get_mut(&row.client_id) {
            Some(g) => {
                if row.created_at < g.connected_at {
                    g.connected_at = row.created_at;
                }
                if row.refresh_expires_at > g.expires_at {
                    g.expires_at = row.refresh_expires_at;
                }
            }
            None => {
                order.push(row.client_id.clone());
                groups.insert(
                    row.client_id,
                    Group {
                        scopes: row.scopes,
                        connected_at: row.created_at,
                        expires_at: row.refresh_expires_at,
                    },
                );
            }
        }
    }

    let mut out = Vec::with_capacity(order.len());
    for client_id in order {
        let group = groups
            .remove(&client_id)
            .expect("client_id was just inserted");
        let scopes = scopes_from_json(&group.scopes)
            .map_err(|e| anyhow::anyhow!("corrupt scopes on oauth token for {client_id}: {e}"))?;
        let client_name = shisho_oauth::clients::get(&state.db, &client_id)
            .await?
            .and_then(|c| c.client_name);
        out.push(ConnectionResponse {
            client_id,
            client_name,
            scopes,
            connected_at: group.connected_at,
            expires_at: group.expires_at,
        });
    }
    Ok(Json(out))
}

/// Revoke a connected client: delete every token it holds for this user. The
/// connector stops authenticating immediately.
#[utoipa::path(
    delete,
    path = "/api/v1/connections/{client_id}",
    tag = "connections",
    params(("client_id" = String, Path, description = "OAuth client id")),
    responses(
        (status = 204),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 404, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn revoke_connection(
    State(state): State<Arc<AppState>>,
    CurrentScope(scope): CurrentScope,
    Path(client_id): Path<String>,
) -> Result<StatusCode, AppError> {
    let Some(user_id) = scope.owner() else {
        return Err(AppError::NotFound(format!(
            "connection {client_id} not found"
        )));
    };
    let removed =
        shisho_oauth::tokens::delete_by_user_and_client(&state.db, &user_id, &client_id).await?;
    if removed == 0 {
        return Err(AppError::NotFound(format!(
            "connection {client_id} not found"
        )));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate as api;
    use crate::test_util::{TEST_SESSION_SECRET, test_state};
    use shisho_auth::session::SessionKeys;
    use shisho_config::{AppConfig, AuthMode};

    const USER: &str = "user-alice";

    async fn multi_user_app() -> (axum::Router, std::sync::Arc<api::AppState>) {
        let mut cfg = AppConfig::default();
        cfg.auth.mode = AuthMode::MultiUser;
        let state = test_state(cfg).await;
        (api::router(state.clone()), state)
    }

    fn session_cookie(user: &str) -> String {
        let keys = SessionKeys::for_test(TEST_SESSION_SECRET);
        format!("shisho_session={}", keys.issue_session(user).unwrap())
    }

    /// Seed a registered client plus one or more live token rows for a user.
    async fn seed_grant(
        state: &api::AppState,
        client_id: &str,
        client_name: &str,
        user: &str,
        access_hashes: &[&str],
    ) {
        if shisho_oauth::clients::get(&state.db, client_id)
            .await
            .unwrap()
            .is_none()
        {
            shisho_oauth::clients::register(
                &state.db,
                client_id,
                &["https://app.example/cb".to_string()],
                Some(client_name),
            )
            .await
            .unwrap();
        }
        for access in access_hashes {
            shisho_oauth::tokens::issue(
                &state.db,
                shisho_oauth::tokens::NewTokens {
                    access_hash: access,
                    refresh_hash: &format!("{access}-refresh"),
                    client_id,
                    user_id: user,
                    scopes: r#"["verdicts:read","verdicts:write"]"#,
                    // Distinct so the response provably surfaces the refresh
                    // expiry (the grant's lifetime), not the short access one.
                    access_expires_at: "2026-07-06T01:00:00+00:00",
                    refresh_expires_at: "2026-08-05T00:00:00+00:00",
                },
            )
            .await
            .unwrap();
        }
    }

    fn req(method: &str, uri: &str, cookie: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(c) = cookie {
            b = b.header(header::COOKIE, c);
        }
        b.body(Body::empty()).unwrap()
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn lists_connected_clients_grouped() {
        let (app, state) = multi_user_app().await;
        seed_grant(&state, "claude", "Claude", USER, &["h1", "h2"]).await;
        seed_grant(&state, "other", "Other", "user-bob", &["h3"]).await;

        let resp = app
            .oneshot(req(
                "GET",
                "/api/v1/connections",
                Some(&session_cookie(USER)),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list = json_body(resp).await;
        assert_eq!(list.as_array().unwrap().len(), 1, "grouped to one client");
        assert_eq!(list[0]["clientId"], "claude");
        assert_eq!(list[0]["clientName"], "Claude");
        assert_eq!(
            list[0]["scopes"],
            serde_json::json!(["verdicts:read", "verdicts:write"])
        );
        assert!(list[0].get("connectedAt").is_some());
        // "Expires" is the refresh-token lifetime (the grant), not the 1-hour
        // access-token expiry the client renews off.
        assert_eq!(list[0]["expiresAt"], "2026-08-05T00:00:00+00:00");
        assert!(!list.to_string().contains("h1"));
    }

    #[tokio::test]
    async fn revoke_removes_all_grants_for_the_client() {
        let (app, state) = multi_user_app().await;
        seed_grant(&state, "claude", "Claude", USER, &["h1", "h2"]).await;

        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                "/api/v1/connections/claude",
                Some(&session_cookie(USER)),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let remaining = shisho_oauth::tokens::list_by_user(&state.db, USER)
            .await
            .unwrap();
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn connector_scoped_token_cannot_manage_connections() {
        let (app, state) = multi_user_app().await;
        // Exactly the scopes an MCP connector is granted at OAuth consent.
        let scopes = shisho_oauth::granted_scopes();
        let (key, _) = crate::test_util::seed_key(&state.db, "connector", &scopes, None).await;

        for (method, uri) in [
            ("GET", "/api/v1/connections"),
            ("DELETE", "/api/v1/connections/claude"),
        ] {
            let request = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(request).await.unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        }
    }

    #[tokio::test]
    async fn revoke_unknown_client_is_404() {
        let (app, _state) = multi_user_app().await;
        let resp = app
            .oneshot(req(
                "DELETE",
                "/api/v1/connections/ghost",
                Some(&session_cookie(USER)),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn one_users_revoke_does_not_touch_anothers() {
        let (app, state) = multi_user_app().await;
        seed_grant(&state, "claude", "Claude", USER, &["h1"]).await;
        seed_grant(&state, "claude", "Claude", "user-bob", &["h9"]).await;

        let resp = app
            .oneshot(req(
                "DELETE",
                "/api/v1/connections/claude",
                Some(&session_cookie(USER)),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let bob = shisho_oauth::tokens::list_by_user(&state.db, "user-bob")
            .await
            .unwrap();
        assert_eq!(bob.len(), 1, "bob's grant is untouched");
    }
}
