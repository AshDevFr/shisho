//! API key management, `/api/v1/keys`. Requires the `keys:manage` scope.
//!
//! List responses never contain key material; the plaintext appears exactly
//! once, in the POST response.
//!
//! Scopes are granular `resource:action` strings (see [`shisho_auth::keys`]). On
//! creation the convenience aliases `read`/`write`/`admin` are also accepted and
//! expanded server-side; responses always echo the granular form.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router, middleware};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::error::AppError;
use crate::middleware::{CurrentScope, require_keys_manage};
use shisho_auth::keys::{Scope, VALID_SCOPES, expand_alias};
use shisho_auth::repo;
use shisho_db::entities::api_keys;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_keys).post(create_key))
        .route("/{id}", delete(delete_key))
        .layer(middleware::from_fn(require_keys_manage))
}

/// A stored key, sans any secret material.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyResponse {
    pub id: String,
    pub name: String,
    pub scopes: Vec<Scope>,
    pub expires_at: Option<String>,
    pub last_used: Option<String>,
    pub created_at: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateKeyRequest {
    #[schema(example = "laptop-cli")]
    pub name: String,
    /// Granular `resource:action` scopes (e.g. `example:write`). The aliases
    /// `read`/`write`/`admin` are also accepted and expanded. Defaults to
    /// read-only (`example:read`).
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    /// ISO 8601 expiry; never expires when omitted.
    pub expires_at: Option<String>,
}

/// POST response: the only place the plaintext key ever appears.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreatedKeyResponse {
    pub id: String,
    pub name: String,
    pub scopes: Vec<Scope>,
    /// The full API key. Store it now; it cannot be retrieved again.
    #[schema(example = "shisho_...")]
    pub key: String,
}

/// List API keys (metadata only, never key material).
#[utoipa::path(
    get,
    path = "/api/v1/keys",
    tag = "keys",
    responses(
        (status = 200, body = [ApiKeyResponse]),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn list_keys(
    State(state): State<Arc<AppState>>,
    CurrentScope(scope): CurrentScope,
) -> Result<Json<Vec<ApiKeyResponse>>, AppError> {
    let rows = repo::list_keys(&state.db, &scope).await?;
    let keys = rows
        .into_iter()
        .map(key_response)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(keys))
}

/// Create an API key. The response is the only time the plaintext is shown.
#[utoipa::path(
    post,
    path = "/api/v1/keys",
    tag = "keys",
    request_body = CreateKeyRequest,
    responses(
        (status = 201, body = CreatedKeyResponse),
        (status = 400, body = super::error::ErrorBody, description = "Unknown scope"),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn create_key(
    State(state): State<Arc<AppState>>,
    CurrentScope(scope): CurrentScope,
    Json(body): Json<CreateKeyRequest>,
) -> Result<(StatusCode, Json<CreatedKeyResponse>), AppError> {
    let scopes = parse_scopes(body.scopes)?;
    // The new key is owned by the requesting user (multi-user), or the local
    // user in single-user mode.
    let user_id = match scope.owner() {
        Some(id) => id,
        None => repo::ensure_local_user(&state.db).await?,
    };
    let (row, plaintext) = repo::create_key(
        &state.db,
        Some(user_id),
        &body.name,
        &scopes,
        body.expires_at,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedKeyResponse {
            id: row.id,
            name: row.name,
            scopes,
            key: plaintext,
        }),
    ))
}

/// Validate and expand requested scopes at the API boundary so unknown scopes
/// yield a stable 400 (a serde failure inside the Json extractor would surface
/// as 422). Accepts granular `resource:action` strings and the aliases
/// `read`/`write`/`admin`. Omitted scopes default to read-only; the result is
/// de-duplicated, preserving first-seen order.
fn parse_scopes(names: Option<Vec<String>>) -> Result<Vec<Scope>, AppError> {
    let Some(names) = names else {
        return Ok(vec![Scope::EXAMPLE_READ]);
    };
    if names.is_empty() {
        return Err(AppError::BadRequest("scopes must not be empty".into()));
    }
    let mut out: Vec<Scope> = Vec::new();
    let push = |scope: Scope, out: &mut Vec<Scope>| {
        if !out.contains(&scope) {
            out.push(scope);
        }
    };
    for name in &names {
        if let Some(expanded) = expand_alias(name) {
            for scope in expanded {
                push(scope, &mut out);
            }
        } else {
            let scope = name.parse::<Scope>().map_err(|_| {
                AppError::BadRequest(format!(
                    "unknown scope {name:?} (expected one of: {}, or the alias admin)",
                    VALID_SCOPES.join(", ")
                ))
            })?;
            push(scope, &mut out);
        }
    }
    Ok(out)
}

/// Revoke (hard-delete) an API key. It stops authenticating immediately.
#[utoipa::path(
    delete,
    path = "/api/v1/keys/{id}",
    tag = "keys",
    params(("id" = String, Path, description = "Key id")),
    responses(
        (status = 204),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 404, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn delete_key(
    State(state): State<Arc<AppState>>,
    CurrentScope(scope): CurrentScope,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    if !repo::delete_key(&state.db, &scope, &id).await? {
        return Err(AppError::NotFound(format!("api key {id} not found")));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn key_response(row: api_keys::Model) -> Result<ApiKeyResponse, AppError> {
    let scopes = shisho_auth::keys::scopes_from_json(&row.scopes)
        .map_err(|e| anyhow::anyhow!("corrupt scopes on key {}: {e}", row.id))?;
    Ok(ApiKeyResponse {
        id: row.id,
        name: row.name,
        scopes,
        expires_at: row.expires_at,
        last_used: row.last_used,
        created_at: row.created_at,
    })
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate as api;
    use crate::test_util::{seed_key, test_state};
    use shisho_auth::keys::Scope;

    const ADMIN_KEY: &str = "shisho_test_admin_config_key";

    /// Real router with a config admin key for managing keys in tests.
    async fn admin_app() -> (axum::Router, std::sync::Arc<api::AppState>) {
        let mut cfg = shisho_config::AppConfig::default();
        cfg.auth.api_key = Some(ADMIN_KEY.into());
        let state = test_state(cfg).await;
        (api::router(state.clone()), state)
    }

    fn req(
        method: &str,
        uri: &str,
        bearer: &str,
        body: Option<serde_json::Value>,
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        let body = match body {
            Some(json) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        builder.body(body).unwrap()
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn create_key_returns_plaintext_once() {
        let (app, _) = admin_app().await;

        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/v1/keys",
                ADMIN_KEY,
                Some(serde_json::json!({"name": "laptop", "scopes": ["example:read", "example:write"]})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = json_body(resp).await;
        assert_eq!(body["name"], "laptop");
        assert_eq!(
            body["scopes"],
            serde_json::json!(["example:read", "example:write"])
        );
        let plaintext = body["key"].as_str().unwrap();
        assert!(plaintext.starts_with("shisho_"));

        // The plaintext never appears again: not in the list response.
        let resp = app
            .oneshot(req("GET", "/api/v1/keys", ADMIN_KEY, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list = json_body(resp).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert!(list[0].get("key").is_none());
        assert!(!list.to_string().contains(plaintext));
    }

    #[tokio::test]
    async fn list_keys_hides_secrets() {
        let (app, state) = admin_app().await;
        let (plaintext, _) = seed_key(&state.db, "seeded", &[Scope::EXAMPLE_READ], None).await;

        let resp = app
            .oneshot(req("GET", "/api/v1/keys", ADMIN_KEY, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list = json_body(resp).await;
        let text = list.to_string();
        assert!(!text.contains(&plaintext), "plaintext leaked");
        assert!(!text.contains("argon2"), "hash leaked");
        assert_eq!(list[0]["name"], "seeded");
        assert!(list[0].get("createdAt").is_some());
        assert!(list[0].get("lastUsed").is_some());
        assert!(list[0].get("expiresAt").is_some());
    }

    #[tokio::test]
    async fn revoked_key_stops_authenticating() {
        let (app, state) = admin_app().await;
        let (plaintext, id) = seed_key(&state.db, "victim", &Scope::all(), None).await;

        let resp = app
            .clone()
            .oneshot(req("GET", "/api/v1/keys", &plaintext, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/v1/keys/{id}"),
                ADMIN_KEY,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app
            .oneshot(req("GET", "/api/v1/keys", &plaintext, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn non_admin_cannot_manage_keys() {
        let (app, state) = admin_app().await;
        // A non-admin key: real scopes, but not keys:manage.
        let scopes = vec![Scope::PROFILE_READ, Scope::PROFILE_WRITE];
        let (plaintext, _) = seed_key(&state.db, "rw", &scopes, None).await;

        let cases = [
            req("GET", "/api/v1/keys", &plaintext, None),
            req(
                "POST",
                "/api/v1/keys",
                &plaintext,
                Some(serde_json::json!({"name": "sneaky"})),
            ),
            req("DELETE", "/api/v1/keys/some-id", &plaintext, None),
        ];
        for request in cases {
            let desc = format!("{} {}", request.method(), request.uri());
            let resp = app.clone().oneshot(request).await.unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{desc}");
        }
    }

    #[tokio::test]
    async fn invalid_scope_rejected_400() {
        let (app, _) = admin_app().await;

        let resp = app
            .oneshot(req(
                "POST",
                "/api/v1/keys",
                ADMIN_KEY,
                Some(serde_json::json!({"name": "bad", "scopes": ["root"]})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = json_body(resp).await;
        assert_eq!(body["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn create_with_granular_scopes_stores_them_verbatim() {
        let (app, _) = admin_app().await;
        let resp = app
            .oneshot(req(
                "POST",
                "/api/v1/keys",
                ADMIN_KEY,
                Some(serde_json::json!({"name": "bot", "scopes": ["example:write"]})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = json_body(resp).await;
        assert_eq!(body["scopes"], serde_json::json!(["example:write"]));
    }

    #[tokio::test]
    async fn create_with_admin_alias_expands_to_every_scope() {
        let (app, _) = admin_app().await;
        let resp = app
            .oneshot(req(
                "POST",
                "/api/v1/keys",
                ADMIN_KEY,
                Some(serde_json::json!({"name": "super", "scopes": ["admin"]})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = json_body(resp).await;
        let granted = body["scopes"].as_array().unwrap();
        // The `admin` alias expands to the full scope set.
        assert!(granted.iter().any(|s| s == "keys:manage"));
        assert!(granted.iter().any(|s| s == "profile:write"));
        assert_eq!(granted.len(), Scope::all().len());
    }

    #[tokio::test]
    async fn create_with_a_removed_alias_is_rejected() {
        let (app, _) = admin_app().await;
        // `write` is no longer an alias and is not a granular scope string.
        let resp = app
            .oneshot(req(
                "POST",
                "/api/v1/keys",
                ADMIN_KEY,
                Some(serde_json::json!({"name": "legacy", "scopes": ["write"]})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn omitted_scopes_default_to_read_only() {
        let (app, _) = admin_app().await;
        let resp = app
            .oneshot(req(
                "POST",
                "/api/v1/keys",
                ADMIN_KEY,
                Some(serde_json::json!({"name": "default"})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = json_body(resp).await;
        assert_eq!(body["scopes"], serde_json::json!(["example:read"]));
    }

    #[tokio::test]
    async fn delete_unknown_key_is_404() {
        let (app, _) = admin_app().await;

        let resp = app
            .oneshot(req("DELETE", "/api/v1/keys/missing", ADMIN_KEY, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn keys_endpoints_in_openapi() {
        let (app, _) = admin_app().await;

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let spec = json_body(resp).await;
        assert!(spec["paths"]["/api/v1/keys"].get("get").is_some());
        assert!(spec["paths"]["/api/v1/keys"].get("post").is_some());
        assert!(spec["paths"]["/api/v1/keys/{id}"].get("delete").is_some());
    }
}
