//! Authentication and authorization middleware for the `/api/v1` router.
//!
//! Contract: every request must carry `Authorization: Bearer <key>`. In
//! single-user mode a configured `auth.api_key` is accepted via constant-time
//! comparison and grants the full scope set (superuser) without touching the
//! database. Otherwise the key is verified against the stored argon2 hashes.
//! The verified identity is inserted as an [`AuthContext`] request extension.
//!
//! Authorization is **per-resource**, not method-based: [`require_auth`] only
//! authenticates and inserts the context; each resource sub-router then layers
//! [`require_scope`] for its [`Resource`], deriving the required [`Action`]
//! from the HTTP method ([`action_for`]). The key-management routes use
//! [`require_keys_manage`] instead. This keeps the resource a compile-time
//! constant at each router boundary, so a scoped key cannot reach a resource it
//! was not granted, and `keys` is never gated by method alone.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, header};
use axum::middleware::Next;
use axum::response::Response;
use axum_extra::extract::CookieJar;
use sea_orm::{ActiveModelTrait, ActiveValue, DatabaseConnection, EntityTrait};
use shisho_config::AuthMode;
use subtle::ConstantTimeEq;

use crate::AppState;
use crate::auth_routes::SESSION_COOKIE;
use crate::error::AppError;
use shisho_auth::keys::{Action, Resource, Scope, allows, scopes_from_json, verify_key};
use shisho_db::entities::api_keys;
use shisho_db::repos::scope::UserScope;

/// Verified identity of the current request. `key_id`/`user_id` are `None` when
/// the config key authenticated (it exists outside the database).
#[derive(Clone, Debug)]
pub struct AuthContext {
    pub key_id: Option<String>,
    pub user_id: Option<String>,
    pub scopes: Vec<Scope>,
}

impl AuthContext {
    /// Whether this identity may perform `action` on `resource`. Plain set
    /// membership: actions are independent (no hierarchy).
    pub fn has_scope(&self, resource: Resource, action: Action) -> bool {
        allows(&self.scopes, resource, action)
    }

    /// The tenant scope this identity's repository calls run under. Derived from
    /// the auth mode, not the credential alone:
    /// - single-user mode is always unscoped (one tenant);
    /// - multi-user with a user (session or user-owned key) confines to it;
    /// - multi-user with no user is an admin/service key (unscoped).
    pub fn user_scope(&self, mode: AuthMode) -> UserScope {
        match (mode, &self.user_id) {
            (AuthMode::SingleUser, _) => UserScope::SingleUser,
            (AuthMode::MultiUser, Some(id)) => UserScope::user(id.clone()),
            (AuthMode::MultiUser, None) => UserScope::SingleUser,
        }
    }
}

/// Extractor yielding the request's [`UserScope`]. Requires [`require_auth`] to
/// have inserted the [`AuthContext`]; rejects with 401 otherwise. Handlers take
/// `scope: CurrentScope` and pass `&scope.0` to the repository layer.
pub struct CurrentScope(pub UserScope);

impl FromRequestParts<Arc<AppState>> for CurrentScope {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let ctx = parts
            .extensions
            .get::<AuthContext>()
            .ok_or(AppError::Unauthorized)?;
        Ok(CurrentScope(ctx.user_scope(state.cfg.auth.mode)))
    }
}

/// Resolve the caller to an admin user row, or reject with 403.
///
/// - **Single-user mode** has exactly one operator, who is implicitly the admin.
///   Any authenticated caller qualifies — including the static config key, which
///   has no user row — and resolves to the local user.
/// - **Multi-user mode**: a request with no owning user (an unowned service key)
///   or a non-admin user is forbidden. Admin status is
///   [`shisho_auth::admin::is_admin`] (email/group allowlists, or the first
///   provisioned user when both are empty).
pub async fn require_admin(
    state: &AppState,
    ctx: &AuthContext,
) -> Result<shisho_db::entities::users::Model, AppError> {
    if state.cfg.auth.mode == AuthMode::SingleUser {
        let id = shisho_auth::repo::ensure_local_user(&state.db)
            .await
            .map_err(|e| AppError::Internal(e.into()))?;
        return shisho_db::repos::users::get(&state.db, &id)
            .await
            .map_err(|e| AppError::Internal(e.into()))?
            .ok_or(AppError::Forbidden);
    }

    let user_id = ctx.user_id.as_deref().ok_or(AppError::Forbidden)?;
    let user = shisho_db::repos::users::get(&state.db, user_id)
        .await
        .map_err(|e| AppError::Internal(e.into()))?
        .ok_or(AppError::Forbidden)?;
    let admin = shisho_auth::admin::is_admin(
        &state.db,
        &state.cfg.auth.admin_emails,
        &state.cfg.auth.admin_groups,
        &user,
    )
    .await
    .map_err(|e| AppError::Internal(e.into()))?;
    if admin {
        Ok(user)
    } else {
        Err(AppError::Forbidden)
    }
}

/// Authenticate the request and insert the [`AuthContext`]. Two credentials are
/// accepted: an `Authorization: Bearer` API key (CLI/MCP/programmatic, any
/// mode) or a session cookie (browser, multi-user mode). The bearer key takes
/// precedence when both are present. Scope enforcement happens in the
/// per-resource [`require_scope`] layer that wraps each router.
///
/// On a session that is within its sliding-refresh window, a fresh cookie is
/// appended to the response so active use keeps the session alive.
pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let mut refreshed_cookie: Option<String> = None;

    let ctx = if let Some(token) = bearer_token(req.headers()) {
        authenticate(&state, token).await?
    } else {
        let (ctx, refresh) = session_auth(&state, req.headers())?.ok_or(AppError::Unauthorized)?;
        refreshed_cookie = refresh;
        ctx
    };

    // A deactivated account is locked out on every surface: its sessions and
    // its API keys both stop working until reactivation.
    reject_if_deactivated(&state, &ctx).await?;

    req.extensions_mut().insert(ctx);
    let mut resp = next.run(req).await;
    if let Some(cookie) = refreshed_cookie
        && let Ok(value) = cookie.parse()
    {
        resp.headers_mut().append(header::SET_COOKIE, value);
    }
    Ok(resp)
}

/// Reject a request whose owning user has been deactivated. A config/service
/// key with no owning user is unaffected (single-user mode has no deactivation).
/// A missing user row is treated as not-deactivated: identity provisioning is
/// the auth flow's concern, not this gate.
async fn reject_if_deactivated(state: &AppState, ctx: &AuthContext) -> Result<(), AppError> {
    let Some(user_id) = ctx.user_id.as_deref() else {
        return Ok(());
    };
    let deactivated = shisho_db::repos::users::get(&state.db, user_id)
        .await
        .map_err(|e| AppError::Internal(e.into()))?
        .is_some_and(|u| u.deactivated_at.is_some());
    if deactivated {
        return Err(AppError::Deactivated);
    }
    Ok(())
}

/// Resolve a session cookie to an [`AuthContext`]. Returns:
/// - `Ok(None)` when no session cookie is present (caller falls through to 401),
/// - `Err(Unauthorized)` when a cookie is present but expired or tampered,
/// - `Ok(Some((ctx, maybe_refreshed_cookie)))` on a valid session.
///
/// A session grants the full scope set: a logged-in user owns all actions over
/// their own data. Tenant isolation (not scope) confines them to their rows.
pub fn session_auth(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<(AuthContext, Option<String>)>, AppError> {
    let jar = CookieJar::from_headers(headers);
    let Some(cookie) = jar.get(SESSION_COOKIE) else {
        return Ok(None);
    };
    let claims = state
        .session
        .verify_session(cookie.value())
        .map_err(|_| AppError::Unauthorized)?;

    let ctx = AuthContext {
        key_id: None,
        user_id: Some(claims.sub.clone()),
        scopes: Scope::all(),
    };

    let refresh = if state.session.needs_refresh(&claims) {
        state
            .session
            .issue_session(&claims.sub)
            .ok()
            .map(|token| crate::auth_routes::session_cookie_string(state, token))
    } else {
        None
    };

    Ok(Some((ctx, refresh)))
}

/// Per-resource scope guard. Reads the [`AuthContext`] that [`require_auth`]
/// inserted and checks `(resource, action_for(method))`. Wrapped per nested
/// router via a closure that fixes `resource` at the boundary, e.g.
/// `from_fn(move |req, next| require_scope(Resource::Example, req, next))`.
pub async fn require_scope(
    resource: Resource,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let action = action_for(req.method());
    let allowed = req
        .extensions()
        .get::<AuthContext>()
        .ok_or(AppError::Unauthorized)?
        .has_scope(resource, action);
    if !allowed {
        return Err(AppError::Forbidden);
    }
    Ok(next.run(req).await)
}

/// Scope guard for the key-management routes: always requires `keys:manage`
/// regardless of method (the `keys` resource has a single action).
pub async fn require_keys_manage(req: Request, next: Next) -> Result<Response, AppError> {
    let allowed = req
        .extensions()
        .get::<AuthContext>()
        .ok_or(AppError::Unauthorized)?
        .has_scope(Resource::Keys, Action::Manage);
    if !allowed {
        return Err(AppError::Forbidden);
    }
    Ok(next.run(req).await)
}

pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Map an HTTP method to the action it requires on a resource: safe methods
/// read, `DELETE` deletes, everything else (POST/PUT/PATCH) writes.
pub fn action_for(method: &Method) -> Action {
    match *method {
        Method::GET | Method::HEAD | Method::OPTIONS => Action::Read,
        Method::DELETE => Action::Delete,
        _ => Action::Write,
    }
}

/// Verify a bearer token against the config key and the stored key hashes.
/// Shared by the REST middleware, the MCP HTTP auth layer, and the MCP stdio
/// startup check, so every surface authenticates identically.
pub async fn authenticate(state: &AppState, token: &str) -> Result<AuthContext, AppError> {
    // Config-key fast path: constant-time compare (no timing oracle), no
    // argon2 cost, full scope set (superuser). Single-user mode only.
    if state.cfg.auth.mode == AuthMode::SingleUser
        && let Some(cfg_key) = &state.cfg.auth.api_key
        && bool::from(token.as_bytes().ct_eq(cfg_key.as_bytes()))
    {
        return Ok(AuthContext {
            key_id: None,
            user_id: None,
            scopes: Scope::all(),
        });
    }

    // OAuth access token (an MCP connector, multi-user mode only). Tried before
    // the API-key scan: an access token is shaped distinctly and resolves by an
    // O(1) hash lookup. A well-formed but unknown or expired token is
    // unauthorized outright — it can never match an argon2 key row, so there is
    // no point falling through to the scan.
    if state.cfg.auth.mode == AuthMode::MultiUser && shisho_oauth::secret::is_access_token(token) {
        let hash = shisho_oauth::secret::hash(token);
        if let Some(row) = shisho_oauth::tokens::find_by_access_hash(&state.db, &hash).await? {
            let now = chrono::Utc::now().to_rfc3339();
            if row.access_expires_at.as_str() > now.as_str() {
                let scopes = scopes_from_json(&row.scopes).map_err(|e| {
                    anyhow::anyhow!("corrupt scopes on oauth token {}: {e}", row.access_hash)
                })?;
                return Ok(AuthContext {
                    key_id: None,
                    user_id: Some(row.user_id),
                    scopes,
                });
            }
        }
        return Err(AppError::Unauthorized);
    }

    // Argon2 hashes cannot be indexed, so verify against every live row. Fine at
    // personal scale; a key-id-embedding token format gives O(1) lookup if key
    // counts ever grow.
    let now = chrono::Utc::now().to_rfc3339();
    let rows = api_keys::Entity::find().all(&state.db).await?;
    for row in rows {
        // ISO 8601 strings in a fixed timezone compare lexicographically.
        if row.expires_at.as_deref().is_some_and(|exp| exp <= &*now) {
            continue;
        }
        if verify_key(token, &row.key_hash) {
            let scopes = scopes_from_json(&row.scopes)
                .map_err(|e| anyhow::anyhow!("corrupt scopes on key {}: {e}", row.id))?;
            touch_last_used(state.db.clone(), row.id.clone());
            return Ok(AuthContext {
                key_id: Some(row.id),
                user_id: row.user_id,
                scopes,
            });
        }
    }
    Err(AppError::Unauthorized)
}

/// Record key usage without coupling auth latency to a write transaction:
/// fire-and-forget, failures are logged and ignored.
fn touch_last_used(db: DatabaseConnection, key_id: String) {
    tokio::spawn(async move {
        let update = api_keys::ActiveModel {
            id: ActiveValue::Unchanged(key_id.clone()),
            last_used: ActiveValue::Set(Some(chrono::Utc::now().to_rfc3339())),
            ..Default::default()
        };
        if let Err(err) = update.update(&db).await {
            tracing::warn!(%key_id, error = %err, "failed to update api key last_used");
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use axum::routing::get;
    use http_body_util::BodyExt;
    use sea_orm::EntityTrait;
    use tower::ServiceExt;

    use crate::test_util::{seed_key, test_state};
    use crate::{self as api, AppState};
    use shisho_auth::keys::Scope;
    use shisho_db::entities::api_keys;

    /// Real router plus a probe route mounted behind the auth + `example`-scope
    /// layers. The probe answers all three methods so per-action independence
    /// (read/write/delete) is testable on one resource.
    fn probe_app(state: Arc<AppState>) -> Router {
        let probe = Router::new().route(
            "/probe",
            get(async || "ok").post(async || "ok").delete(async || "ok"),
        );
        api::router_with_protected(state, probe)
    }

    fn request(method: &str, uri: &str, bearer: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = bearer {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder.body(Body::empty()).unwrap()
    }

    async fn status_and_code(resp: axum::response::Response) -> (StatusCode, String) {
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        (status, body["error"]["code"].as_str().unwrap_or("").into())
    }

    #[tokio::test]
    async fn missing_bearer_is_401() {
        let app = probe_app(test_state(Default::default()).await);

        for req in [
            request("GET", "/api/v1/probe", None),
            // Malformed scheme.
            Request::builder()
                .uri("/api/v1/probe")
                .header(header::AUTHORIZATION, "Token abc")
                .body(Body::empty())
                .unwrap(),
            // Well-formed but unknown key.
            request("GET", "/api/v1/probe", Some("shisho_unknown")),
        ] {
            let resp = app.clone().oneshot(req).await.unwrap();
            let (status, code) = status_and_code(resp).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert_eq!(code, "unauthorized");
        }
    }

    #[tokio::test]
    async fn config_key_grants_admin() {
        let mut cfg = shisho_config::AppConfig::default();
        cfg.auth.api_key = Some("shisho_config_secret".into());
        let app = probe_app(test_state(cfg).await);

        for method in ["GET", "POST"] {
            let resp = app
                .clone()
                .oneshot(request(
                    method,
                    "/api/v1/probe",
                    Some("shisho_config_secret"),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{method} should pass");
        }

        // Near-miss must not authenticate.
        let resp = app
            .oneshot(request(
                "GET",
                "/api/v1/probe",
                Some("shisho_config_secreT"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn db_key_authenticates_with_scopes() {
        let state = test_state(Default::default()).await;
        let (key, _) = seed_key(
            &state.db,
            "rw",
            &[Scope::EXAMPLE_READ, Scope::EXAMPLE_WRITE],
            None,
        )
        .await;
        let app = probe_app(state);

        for method in ["GET", "POST"] {
            let resp = app
                .clone()
                .oneshot(request(method, "/api/v1/probe", Some(&key)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{method} should pass");
        }
    }

    #[tokio::test]
    async fn read_key_cannot_write_403() {
        let state = test_state(Default::default()).await;
        let (key, _) = seed_key(&state.db, "ro", &[Scope::EXAMPLE_READ], None).await;
        let app = probe_app(state);

        let resp = app
            .clone()
            .oneshot(request("GET", "/api/v1/probe", Some(&key)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(request("POST", "/api/v1/probe", Some(&key)))
            .await
            .unwrap();
        let (status, code) = status_and_code(resp).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(code, "forbidden");
    }

    /// Actions are independent within a resource: an `example:write` key may
    /// write but neither read nor delete.
    #[tokio::test]
    async fn write_key_cannot_read_or_delete_403() {
        let state = test_state(Default::default()).await;
        let (key, _) = seed_key(&state.db, "wo", &[Scope::EXAMPLE_WRITE], None).await;
        let app = probe_app(state);

        let post = app
            .clone()
            .oneshot(request("POST", "/api/v1/probe", Some(&key)))
            .await
            .unwrap();
        assert_eq!(post.status(), StatusCode::OK, "write is allowed");

        for method in ["GET", "DELETE"] {
            let resp = app
                .clone()
                .oneshot(request(method, "/api/v1/probe", Some(&key)))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "{method} must be forbidden for a write-only key"
            );
        }
    }

    #[tokio::test]
    async fn expired_key_is_401() {
        let state = test_state(Default::default()).await;
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let (key, _) = seed_key(&state.db, "expired", &Scope::all(), Some(past)).await;
        let app = probe_app(state);

        let resp = app
            .oneshot(request("GET", "/api/v1/probe", Some(&key)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn health_docs_info_are_public() {
        let app = api::router(test_state(Default::default()).await);

        for uri in ["/api/v1/health", "/api/v1/info", "/docs", "/openapi.json"] {
            let resp = app
                .clone()
                .oneshot(request("GET", uri, None))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri} must be public");
        }
    }

    #[tokio::test]
    async fn last_used_updated_after_auth() {
        let state = test_state(Default::default()).await;
        let (key, id) = seed_key(&state.db, "tracked", &[Scope::EXAMPLE_READ], None).await;
        let app = probe_app(state.clone());

        let resp = app
            .oneshot(request("GET", "/api/v1/probe", Some(&key)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // The update is fire-and-forget; poll briefly for it to land.
        for _ in 0..100 {
            let row = api_keys::Entity::find_by_id(&id)
                .one(&state.db)
                .await
                .unwrap()
                .unwrap();
            if row.last_used.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("last_used was never updated");
    }

    /// The keys router demands `keys:manage` regardless of method: an
    /// `example`-scoped key (no manage) is forbidden; the full scope set passes.
    #[tokio::test]
    async fn keys_route_requires_keys_manage() {
        let state = test_state(Default::default()).await;
        let example = shisho_auth::keys::expand_alias("write").unwrap();
        let (example_key, _) = seed_key(&state.db, "w", &example, None).await;
        let (admin_key, _) = seed_key(&state.db, "a", &Scope::all(), None).await;
        let app = api::router(state);

        let resp = app
            .clone()
            .oneshot(request("GET", "/api/v1/keys", Some(&example_key)))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "example scopes lack keys:manage"
        );
        let resp = app
            .oneshot(request("GET", "/api/v1/keys", Some(&admin_key)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "full scope set manages keys");
    }
}
