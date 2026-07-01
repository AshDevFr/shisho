//! Browser-facing login routes (multi-user mode).
//!
//! `GET /auth/providers` lists the configured backends for the login page.
//! `GET /auth/login/:provider` starts a flow: it stashes the CSRF state, OIDC
//! nonce, and PKCE verifier in a short-lived signed cookie (stateless server
//! side) and redirects to the provider. `GET /auth/callback/:provider` verifies
//! the returned state against that cookie, exchanges the code, provisions or
//! refreshes the user, issues a session cookie, and redirects back into the
//! app. `POST /auth/logout` clears the session and `GET /auth/me` returns the
//! current user.
//!
//! These routes are public by necessity; they only ever run in multi-user mode
//! (the router mounts nothing here otherwise).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use serde::{Deserialize, Serialize};

use shisho_auth::providers::ProviderInfo;
use shisho_auth::session::LoginState;
use shisho_db::repos::users;

use crate::AppState;
use crate::error::AppError;

/// Name of the short-lived cookie carrying the in-flight login state.
const LOGIN_COOKIE: &str = "shisho_login";
/// Name of the session cookie issued after a successful login.
pub const SESSION_COOKIE: &str = "shisho_session";

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/auth/providers", get(list_providers))
        .route("/auth/login/{provider}", get(login))
        .route("/auth/callback/{provider}", get(callback))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
}

/// The configured providers, in config order, for the login page.
async fn list_providers(State(state): State<Arc<AppState>>) -> Json<Vec<ProviderInfo>> {
    Json(state.providers.list())
}

/// The authenticated user (`GET /auth/me`). 401 without a valid session cookie.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MeResponse {
    id: String,
    email: Option<String>,
    display_name: Option<String>,
    avatar_url: Option<String>,
    is_admin: bool,
}

async fn me(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
) -> Result<Json<MeResponse>, AppError> {
    let cookie = jar.get(SESSION_COOKIE).ok_or(AppError::Unauthorized)?;
    let claims = state
        .session
        .verify_session(cookie.value())
        .map_err(|_| AppError::Unauthorized)?;
    let user = users::get(&state.db, &claims.sub)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("loading user: {e}")))?
        .ok_or(AppError::Unauthorized)?;
    let is_admin = shisho_auth::admin::is_admin(
        &state.db,
        &state.cfg.auth.admin_emails,
        &state.cfg.auth.admin_groups,
        &user,
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("resolving admin: {e}")))?;
    Ok(Json(MeResponse {
        id: user.id,
        email: user.email,
        display_name: user.display_name,
        avatar_url: user.avatar_url,
        is_admin,
    }))
}

#[derive(Debug, Deserialize)]
struct LoginQuery {
    /// In-app path to return to after login. Sanitized to a same-site path.
    redirect_to: Option<String>,
}

/// Start a login flow: build the provider redirect, stash the flow secrets in a
/// signed cookie, and 302 to the provider.
async fn login(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    Query(query): Query<LoginQuery>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    let provider = state
        .providers
        .get(&provider)
        .ok_or_else(|| AppError::NotFound(format!("unknown auth provider: {provider}")))?;

    let auth = provider
        .authorize()
        .map_err(|e| AppError::Internal(e.context("building authorization request")))?;

    let login_state = LoginState {
        provider: provider.name().to_string(),
        csrf: auth.csrf,
        nonce: auth.nonce,
        pkce_verifier: auth.pkce_verifier,
        redirect_to: safe_redirect(query.redirect_to.as_deref()),
        exp: 0,
    };
    let token = state
        .session
        .encode_login_state(login_state)
        .map_err(|e| AppError::Internal(e.context("signing login-state cookie")))?;

    let jar = jar.add(build_login_cookie(&state, token));
    Ok((jar, Redirect::to(&auth.url)))
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    /// Provider-side error (e.g. user denied consent).
    error: Option<String>,
}

/// Complete a login flow: validate state, exchange the code, provision the user.
async fn callback(
    State(state): State<Arc<AppState>>,
    Path(provider_name): Path<String>,
    Query(query): Query<CallbackQuery>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    // Read the in-flight state before clearing the cookie: this flow is over
    // regardless of outcome, so the cookie is always dropped from the response.
    let token = jar_token(&jar);
    let jar = jar.remove(removal_cookie(LOGIN_COOKIE));

    if let Some(err) = query.error {
        return Err(AppError::BadRequest(format!(
            "provider returned error: {err}"
        )));
    }

    let login_state = jar_login_state(&state, &token)?;
    if login_state.provider != provider_name {
        return Err(AppError::BadRequest(
            "login-state cookie does not match callback provider".into(),
        ));
    }

    let returned_state = query
        .state
        .ok_or_else(|| AppError::BadRequest("callback is missing state".into()))?;
    if returned_state != login_state.csrf {
        return Err(AppError::BadRequest("state mismatch".into()));
    }

    let code = query
        .code
        .ok_or_else(|| AppError::BadRequest("callback is missing code".into()))?;

    let provider = state
        .providers
        .get(&provider_name)
        .ok_or_else(|| AppError::NotFound(format!("unknown auth provider: {provider_name}")))?;

    let identity = provider
        .exchange(
            state.providers.http(),
            code,
            login_state.pkce_verifier,
            login_state.nonce,
        )
        .await
        .map_err(|e| AppError::BadRequest(format!("login failed: {e:#}")))?;

    let user = users::find_or_create(&state.db, &identity)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("provisioning user: {e}")))?;

    // Issue the session cookie and return the browser to the app.
    let token = state
        .session
        .issue_session(&user.id)
        .map_err(|e| AppError::Internal(e.context("issuing session")))?;
    let jar = jar.add(build_session_cookie(&state, token));
    Ok((jar, Redirect::to(&login_state.redirect_to)))
}

/// Clear the session cookie (logout). In-flight login state is dropped too so a
/// stale cookie cannot linger.
async fn logout(jar: CookieJar) -> impl IntoResponse {
    let jar = jar
        .remove(removal_cookie(LOGIN_COOKIE))
        .remove(removal_cookie(SESSION_COOKIE));
    (jar, Redirect::to("/"))
}

/// Read the raw login-cookie JWT, or 400 if absent.
fn jar_token(jar: &CookieJar) -> String {
    jar.get(LOGIN_COOKIE)
        .map(|c| c.value().to_string())
        .unwrap_or_default()
}

/// Decode and verify the login-state cookie.
fn jar_login_state(state: &AppState, token: &str) -> Result<LoginState, AppError> {
    if token.is_empty() {
        return Err(AppError::BadRequest("missing login-state cookie".into()));
    }
    state
        .session
        .decode_login_state(token)
        .map_err(|_| AppError::BadRequest("invalid or expired login-state cookie".into()))
}

/// Cookie attributes: HttpOnly, SameSite=Lax (the provider redirect is a
/// top-level GET, which Lax allows), Path=/, Secure per config. The JWT's own
/// `exp` bounds its lifetime, so no Max-Age is needed.
fn build_login_cookie(state: &AppState, token: String) -> Cookie<'static> {
    let mut cookie = Cookie::new(LOGIN_COOKIE, token);
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_path("/");
    cookie.set_secure(state.session.secure_cookie);
    cookie
}

/// Build the session cookie. Same attributes as the login cookie, plus a
/// `Max-Age` matching the session TTL so it survives a browser restart; the
/// JWT's own `exp` remains the source of truth on the server.
fn build_session_cookie(state: &AppState, token: String) -> Cookie<'static> {
    let mut cookie = Cookie::new(SESSION_COOKIE, token);
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_path("/");
    cookie.set_secure(state.session.secure_cookie);
    cookie.set_max_age(time::Duration::hours(state.session.ttl_hours));
    cookie
}

/// The session cookie rendered as a `Set-Cookie` header value, for the
/// middleware's sliding-refresh re-issue.
pub fn session_cookie_string(state: &AppState, token: String) -> String {
    build_session_cookie(state, token).to_string()
}

/// A removal cookie (expired, empty) at `Path=/`, so the browser actually
/// clears the cookie set with that path.
fn removal_cookie(name: &'static str) -> Cookie<'static> {
    let mut cookie = Cookie::new(name, "");
    cookie.set_path("/");
    cookie
}

/// Reduce a caller-supplied redirect target to a safe same-site path: it must
/// start with a single `/` (not `//`, which is a protocol-relative URL) and
/// carry no scheme. Anything else falls back to `/`.
fn safe_redirect(target: Option<&str>) -> String {
    match target {
        Some(t) if t.starts_with('/') && !t.starts_with("//") => t.to_string(),
        _ => "/".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use sea_orm::EntityTrait;
    use shisho_auth::keys::Scope;
    use shisho_auth::oauth2::OAuth2Provider;
    use shisho_auth::providers::{Provider, ProviderRegistry};
    use shisho_auth::session::SessionKeys;
    use shisho_config::{AppConfig, AuthMode, ProviderConfig, ProviderKind, SessionConfig};
    use shisho_db::entities::users;

    use crate::test_util::{TEST_SESSION_SECRET, seed_key, test_state_with_registry};

    /// An AppState whose single `mock` OAuth2 provider points at `server`, with
    /// the default test session policy.
    async fn state_for(server: &MockServer) -> Arc<crate::AppState> {
        state_for_session(server, None).await
    }

    /// As [`state_for`], but with an explicit session policy (TTL/refresh) so the
    /// sliding-refresh test can force a near-expiry session.
    async fn state_for_session(
        server: &MockServer,
        session: Option<SessionConfig>,
    ) -> Arc<crate::AppState> {
        let mut cfg = AppConfig::default();
        cfg.auth.mode = AuthMode::MultiUser;
        if let Some(session) = session {
            cfg.auth.session = session;
        }
        let pcfg = ProviderConfig {
            name: "mock".into(),
            kind: ProviderKind::Oauth2,
            client_id: "cid".into(),
            client_secret: Some("csecret".into()),
            auth_url: Some(format!("{}/authorize", server.uri())),
            token_url: Some(format!("{}/token", server.uri())),
            userinfo_url: Some(format!("{}/userinfo", server.uri())),
            ..Default::default()
        };
        let provider = Provider::OAuth2(Box::new(
            OAuth2Provider::from_config(&pcfg, "http://localhost/auth/callback/mock".into())
                .unwrap(),
        ));
        cfg.auth.providers.push(pcfg);
        let registry = ProviderRegistry::from_providers(vec![provider]);
        test_state_with_registry(cfg, registry).await
    }

    /// Stub the token + userinfo endpoints to return `email`/`name` for `sub`.
    async fn mock_provider(server: &MockServer, sub: &str, email: &str, name: &str) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "mock-access-token",
                "token_type": "bearer",
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/userinfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sub": sub,
                "email": email,
                "name": name,
            })))
            .mount(server)
            .await;
    }

    fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri(uri);
        if let Some(c) = cookie {
            b = b.header(header::COOKIE, c);
        }
        b.body(Body::empty()).unwrap()
    }

    /// The `shisho_login=...` cookie value from a Set-Cookie header.
    fn login_cookie(resp: &axum::http::Response<Body>) -> String {
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        set_cookie.split(';').next().unwrap().to_string()
    }

    /// The `shisho_session=...` cookie from a (possibly multi-header) Set-Cookie
    /// response, if one was set.
    fn session_cookie(resp: &axum::http::Response<Body>) -> Option<String> {
        resp.headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .find_map(|v| {
                let s = v.to_str().ok()?;
                s.starts_with("shisho_session=")
                    .then(|| s.split(';').next().unwrap().to_string())
            })
    }

    /// Recover the CSRF state from a login cookie by decoding its JWT with the
    /// known test secret.
    fn csrf_from_cookie(cookie: &str) -> String {
        let token = cookie.strip_prefix("shisho_login=").unwrap();
        SessionKeys::for_test(TEST_SESSION_SECRET)
            .decode_login_state(token)
            .unwrap()
            .csrf
    }

    /// Drive a full login: GET /auth/login, then GET /auth/callback with the
    /// returned cookie and the matching state. Returns the callback response.
    async fn run_login(app: &axum::Router, sub: &str) -> axum::http::Response<Body> {
        let resp = app
            .clone()
            .oneshot(get("/auth/login/mock", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER, "login redirects");
        let cookie = login_cookie(&resp);
        let state = csrf_from_cookie(&cookie);

        app.clone()
            .oneshot(get(
                &format!("/auth/callback/mock?code=abc-{sub}&state={state}"),
                Some(&cookie),
            ))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn provisions_user_on_first_login() {
        let server = MockServer::start().await;
        mock_provider(&server, "sub-1", "a@example.com", "Ada").await;
        let state = state_for(&server).await;
        let app = crate::router(state.clone());

        let resp = run_login(&app, "sub-1").await;
        assert_eq!(
            resp.status(),
            StatusCode::SEE_OTHER,
            "callback redirects on success"
        );

        let rows = users::Entity::find().all(&state.db).await.unwrap();
        let provisioned: Vec<_> = rows
            .iter()
            .filter(|u| u.provider.as_deref() == Some("mock"))
            .collect();
        assert_eq!(provisioned.len(), 1);
        assert_eq!(provisioned[0].oidc_sub.as_deref(), Some("sub-1"));
        assert_eq!(provisioned[0].email.as_deref(), Some("a@example.com"));
        assert_eq!(provisioned[0].display_name.as_deref(), Some("Ada"));
    }

    #[tokio::test]
    async fn second_login_is_idempotent() {
        let server = MockServer::start().await;
        mock_provider(&server, "sub-1", "a@example.com", "Ada").await;
        let state = state_for(&server).await;
        let app = crate::router(state.clone());

        run_login(&app, "sub-1").await;
        run_login(&app, "sub-1").await;

        let provisioned = users::Entity::find()
            .all(&state.db)
            .await
            .unwrap()
            .into_iter()
            .filter(|u| u.provider.as_deref() == Some("mock"))
            .count();
        assert_eq!(provisioned, 1, "same identity must not duplicate");
    }

    #[tokio::test]
    async fn rejects_mismatched_state() {
        let server = MockServer::start().await;
        mock_provider(&server, "sub-1", "a@example.com", "Ada").await;
        let state = state_for(&server).await;
        let app = crate::router(state);

        let resp = app
            .clone()
            .oneshot(get("/auth/login/mock", None))
            .await
            .unwrap();
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        let cookie = set_cookie.split(';').next().unwrap().to_string();

        let resp = app
            .oneshot(get(
                "/auth/callback/mock?code=abc&state=not-the-real-state",
                Some(&cookie),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rejects_unknown_provider_name() {
        let server = MockServer::start().await;
        let state = state_for(&server).await;
        let app = crate::router(state);

        let resp = app.oneshot(get("/auth/login/nope", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn lists_configured_providers() {
        let server = MockServer::start().await;
        let state = state_for(&server).await;
        let app = crate::router(state);

        let resp = app.oneshot(get("/auth/providers", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let providers: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(providers[0]["name"], "mock");
        assert_eq!(providers[0]["kind"], "oauth2");
    }

    #[tokio::test]
    async fn single_user_mode_has_no_auth_routes() {
        let state = crate::test_util::test_state(AppConfig::default()).await;
        let app = crate::router(state);

        for uri in ["/auth/providers", "/auth/login/mock"] {
            let resp = app.clone().oneshot(get(uri, None)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri} must be absent");
        }
    }

    fn post(uri: &str, cookie: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method("POST").uri(uri);
        if let Some(c) = cookie {
            b = b.header(header::COOKIE, c);
        }
        b.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn session_issued_on_callback_and_me_works() {
        let server = MockServer::start().await;
        mock_provider(&server, "sub-1", "a@example.com", "Ada").await;
        let state = state_for(&server).await;
        let app = crate::router(state);

        let resp = run_login(&app, "sub-1").await;
        let session = session_cookie(&resp).expect("callback sets a session cookie");

        let resp = app.oneshot(get("/auth/me", Some(&session))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let me: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(me["email"], "a@example.com");
        assert_eq!(me["displayName"], "Ada");
        // The first provisioned user is the admin (no admin_emails configured).
        assert_eq!(me["isAdmin"], true);
    }

    #[tokio::test]
    async fn me_without_session_is_401() {
        let server = MockServer::start().await;
        let state = state_for(&server).await;
        let app = crate::router(state);

        let resp = app.clone().oneshot(get("/auth/me", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .oneshot(get("/auth/me", Some("shisho_session=garbage.jwt.value")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "tampered token");
    }

    #[tokio::test]
    async fn logout_clears_session_cookie() {
        let server = MockServer::start().await;
        let state = state_for(&server).await;
        let app = crate::router(state);

        let resp = app
            .oneshot(post(
                "/auth/logout",
                Some("shisho_session=some.session.jwt"),
            ))
            .await
            .unwrap();
        let cookies: Vec<String> = resp
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        let cleared = cookies
            .iter()
            .any(|c| c.starts_with("shisho_session=") && c.contains("Max-Age=0"));
        assert!(
            cleared,
            "logout must clear the session cookie; got {cookies:?}"
        );
    }

    #[tokio::test]
    async fn sliding_refresh_reissues_cookie() {
        let server = MockServer::start().await;
        mock_provider(&server, "sub-1", "a@example.com", "Ada").await;
        // 1h TTL with a 24h refresh window: any authenticated request is "near
        // expiry" and must re-issue the cookie.
        let session = SessionConfig {
            secret: Some(TEST_SESSION_SECRET.into()),
            ttl_hours: 1,
            refresh_threshold_hours: 24,
            secure_cookie: false,
        };
        let state = state_for_session(&server, Some(session)).await;
        let app = crate::router(state);

        let resp = run_login(&app, "sub-1").await;
        let cookie = session_cookie(&resp).unwrap();

        // Hit a protected API route with the session cookie (no bearer key).
        let resp = app
            .oneshot(get("/api/v1/whoami", Some(&cookie)))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "session authenticates the API"
        );
        assert!(
            session_cookie(&resp).is_some(),
            "a near-expiry session must be re-issued"
        );
    }

    #[tokio::test]
    async fn api_key_path_unaffected_by_sessions() {
        // In multi-user mode, a bearer API key still authenticates with no
        // cookie present (CLI/MCP path is independent of sessions).
        let server = MockServer::start().await;
        let state = state_for(&server).await;
        let (key, _) = seed_key(&state.db, "cli", &Scope::all(), None).await;
        let app = crate::router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/whoami")
                    .header(header::AUTHORIZATION, format!("Bearer {key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
