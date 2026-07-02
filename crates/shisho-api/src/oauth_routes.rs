//! OAuth 2.0 authorization-server endpoints, so an MCP connector (e.g. the
//! claude.ai web connector) can obtain a token for `/mcp` (it speaks only
//! OAuth 2.1, not static API keys).
//!
//! Surface: discovery metadata (RFC 9728 / 8414), dynamic client registration
//! (RFC 7591), the authorization endpoint with PKCE and a login + consent step,
//! and the token endpoint (authorization-code + refresh). The human-auth step
//! reuses the multi-user session: an unauthenticated `/authorize` sends the user
//! through the existing `/auth/login/:provider` flow and returns here, so the
//! code is pinned to a real `user_id`.
//!
//! These routes are public (no bearer middleware) and mount only in multi-user
//! mode; single-user instances never serve them.

use std::sync::Arc;

use axum::Form;
use axum::Json;
use axum::Router;
use axum::extract::{Query, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::CookieJar;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};

use shisho_auth::keys::{scopes_from_json, scopes_to_json};
use shisho_oauth::{
    ACCESS_TTL_SECS, CODE_TTL_SECS, REFRESH_TTL_SECS, clients, codes, granted_scopes, metadata,
    pkce, scopes_supported, secret, tokens,
};

use crate::AppState;
use crate::auth_routes::SESSION_COOKIE;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/authorize/consent", post(consent))
        .route("/oauth/token", post(token))
}

// --- Discovery metadata -----------------------------------------------------

async fn protected_resource_metadata(State(state): State<Arc<AppState>>) -> Response {
    let base = state.cfg.server.public_base();
    Json(metadata::protected_resource(&base, scopes_supported())).into_response()
}

async fn authorization_server_metadata(State(state): State<Arc<AppState>>) -> Response {
    let base = state.cfg.server.public_base();
    Json(metadata::authorization_server(&base, scopes_supported())).into_response()
}

// --- Dynamic client registration (RFC 7591) ---------------------------------

#[derive(Debug, Deserialize)]
struct RegisterRequest {
    #[serde(default)]
    redirect_uris: Vec<String>,
    #[serde(default)]
    client_name: Option<String>,
    // Other RFC 7591 metadata fields are accepted and ignored: this server only
    // needs the redirect URIs and a display name.
}

#[derive(Debug, Serialize)]
struct RegisterResponse {
    client_id: String,
    client_id_issued_at: i64,
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: &'static str,
    grant_types: Vec<&'static str>,
    response_types: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_name: Option<String>,
}

async fn register(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterRequest>,
) -> Response {
    if req.redirect_uris.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "at least one redirect_uri is required",
        );
    }
    let client_id = secret::new_client_id();
    if let Err(e) = clients::register(
        &state.db,
        &client_id,
        &req.redirect_uris,
        req.client_name.as_deref(),
    )
    .await
    {
        tracing::error!(error = ?e, "registering oauth client");
        return oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "could not register client",
        );
    }
    let body = RegisterResponse {
        client_id,
        client_id_issued_at: chrono::Utc::now().timestamp(),
        redirect_uris: req.redirect_uris,
        token_endpoint_auth_method: "none",
        grant_types: vec!["authorization_code", "refresh_token"],
        response_types: vec!["code"],
        client_name: req.client_name,
    };
    (StatusCode::CREATED, Json(body)).into_response()
}

// --- Authorization endpoint -------------------------------------------------

#[derive(Debug, Deserialize)]
struct AuthorizeParams {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    scope: Option<String>,
}

async fn authorize(
    State(state): State<Arc<AppState>>,
    Query(p): Query<AuthorizeParams>,
    RawQuery(raw): RawQuery,
    jar: CookieJar,
) -> Response {
    // Validate the client and redirect_uri before trusting the redirect for
    // error reporting: an unregistered redirect_uri must never receive a
    // redirect (open-redirect / token-leak guard).
    let Some(client_id) = p.client_id.as_deref() else {
        return error_page(StatusCode::BAD_REQUEST, "Missing client_id.");
    };
    let client = match clients::get(&state.db, client_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return error_page(StatusCode::BAD_REQUEST, "Unknown client_id."),
        Err(e) => {
            tracing::error!(error = ?e, "loading oauth client");
            return error_page(StatusCode::INTERNAL_SERVER_ERROR, "Internal error.");
        }
    };
    let Some(redirect_uri) = p.redirect_uri.as_deref() else {
        return error_page(StatusCode::BAD_REQUEST, "Missing redirect_uri.");
    };
    if !clients::allows_redirect(&client, redirect_uri) {
        return error_page(
            StatusCode::BAD_REQUEST,
            "redirect_uri is not registered for this client.",
        );
    }

    // From here, errors may safely redirect back to the client.
    let st = p.state.as_deref();
    if p.response_type.as_deref() != Some("code") {
        return redirect_error(redirect_uri, "unsupported_response_type", st);
    }
    let Some(challenge) = p.code_challenge.as_deref().filter(|c| !c.is_empty()) else {
        return redirect_error(redirect_uri, "invalid_request", st);
    };
    if p.code_challenge_method.as_deref() != Some(pkce::METHOD_S256) {
        return redirect_error(redirect_uri, "invalid_request", st);
    }

    // Require a logged-in user. If absent, send them through the existing login
    // and return to this exact authorize URL afterwards.
    match current_user(&state, &jar) {
        Some(user_id) => {
            consent_page(
                &state,
                &client,
                redirect_uri,
                challenge,
                st,
                p.scope.as_deref(),
                &user_id,
            )
            .await
        }
        None => login_page(&state, raw.as_deref()),
    }
}

#[derive(Debug, Deserialize)]
struct ConsentForm {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    #[serde(default)]
    state: Option<String>,
    decision: String,
}

async fn consent(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    Form(form): Form<ConsentForm>,
) -> Response {
    // The session cookie is SameSite=Lax, so a cross-site POST carries no
    // session and lands here unauthenticated: that is the CSRF guard.
    let Some(user_id) = current_user(&state, &jar) else {
        return error_page(
            StatusCode::UNAUTHORIZED,
            "Your session expired. Start the connection again.",
        );
    };
    // Re-validate the client and redirect: the hidden fields are client-supplied.
    let client = match clients::get(&state.db, &form.client_id).await {
        Ok(Some(c)) => c,
        _ => return error_page(StatusCode::BAD_REQUEST, "Unknown client."),
    };
    if !clients::allows_redirect(&client, &form.redirect_uri) {
        return error_page(
            StatusCode::BAD_REQUEST,
            "redirect_uri is not registered for this client.",
        );
    }

    let st = form.state.as_deref();
    if form.decision != "allow" {
        return redirect_error(&form.redirect_uri, "access_denied", st);
    }

    let code = secret::new_code();
    let code_hash = secret::hash(&code);
    let scopes_json = scopes_to_json(&granted_scopes());
    let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(CODE_TTL_SECS)).to_rfc3339();
    if let Err(e) = codes::issue(
        &state.db,
        codes::NewCode {
            code_hash: &code_hash,
            client_id: &form.client_id,
            redirect_uri: &form.redirect_uri,
            code_challenge: &form.code_challenge,
            user_id: &user_id,
            scopes: &scopes_json,
            expires_at: &expires_at,
        },
    )
    .await
    {
        tracing::error!(error = ?e, "issuing authorization code");
        return error_page(StatusCode::INTERNAL_SERVER_ERROR, "Internal error.");
    }

    redirect_success(&form.redirect_uri, &code, st)
}

// --- Token endpoint ---------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
}

async fn token(State(state): State<Arc<AppState>>, Form(form): Form<TokenForm>) -> Response {
    match form.grant_type.as_deref() {
        Some("authorization_code") => token_authorization_code(&state, form).await,
        Some("refresh_token") => token_refresh(&state, form).await,
        _ => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "grant_type must be authorization_code or refresh_token",
        ),
    }
}

async fn token_authorization_code(state: &AppState, form: TokenForm) -> Response {
    let (Some(code), Some(redirect_uri), Some(client_id), Some(verifier)) = (
        form.code.as_deref(),
        form.redirect_uri.as_deref(),
        form.client_id.as_deref(),
        form.code_verifier.as_deref(),
    ) else {
        return invalid_request("code, redirect_uri, client_id, and code_verifier are required");
    };

    let now = chrono::Utc::now().to_rfc3339();
    let code_hash = secret::hash(code);
    let row = match codes::consume(&state.db, &code_hash, &now).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return invalid_grant("authorization code is invalid, expired, or already used");
        }
        Err(e) => return server_error(e, "consuming authorization code"),
    };

    if row.client_id != client_id || row.redirect_uri != redirect_uri {
        return invalid_grant("client_id or redirect_uri does not match the authorization code");
    }
    if !pkce::verify_s256(verifier, &row.code_challenge) {
        return invalid_grant("PKCE verification failed");
    }

    issue_token_pair(state, &row.client_id, &row.user_id, &row.scopes).await
}

async fn token_refresh(state: &AppState, form: TokenForm) -> Response {
    let Some(refresh_token) = form.refresh_token.as_deref() else {
        return invalid_request("refresh_token is required");
    };
    let refresh_hash = secret::hash(refresh_token);
    let row = match tokens::find_by_refresh_hash(&state.db, &refresh_hash).await {
        Ok(Some(row)) => row,
        Ok(None) => return invalid_grant("refresh token is invalid"),
        Err(e) => return server_error(e, "loading refresh token"),
    };

    let now = chrono::Utc::now().to_rfc3339();
    if row.refresh_expires_at.as_str() <= now.as_str() {
        // Expired: rotate it out so a stale token cannot linger.
        let _ = tokens::delete_by_access_hash(&state.db, &row.access_hash).await;
        return invalid_grant("refresh token has expired");
    }
    if let Some(client_id) = form.client_id.as_deref()
        && client_id != row.client_id
    {
        return invalid_grant("client_id does not match the refresh token");
    }

    // Rotate: the old pair is invalidated and a fresh pair issued.
    if let Err(e) = tokens::delete_by_access_hash(&state.db, &row.access_hash).await {
        return server_error(e, "rotating refresh token");
    }
    issue_token_pair(state, &row.client_id, &row.user_id, &row.scopes).await
}

/// Mint and persist an access/refresh pair carrying `scopes_json`, returning the
/// RFC 6749 token response.
async fn issue_token_pair(
    state: &AppState,
    client_id: &str,
    user_id: &str,
    scopes_json: &str,
) -> Response {
    let access = secret::new_access_token();
    let refresh = secret::new_refresh_token();
    let now = chrono::Utc::now();
    let access_expires_at = (now + chrono::Duration::seconds(ACCESS_TTL_SECS)).to_rfc3339();
    let refresh_expires_at = (now + chrono::Duration::seconds(REFRESH_TTL_SECS)).to_rfc3339();

    if let Err(e) = tokens::issue(
        &state.db,
        tokens::NewTokens {
            access_hash: &secret::hash(&access),
            refresh_hash: &secret::hash(&refresh),
            client_id,
            user_id,
            scopes: scopes_json,
            access_expires_at: &access_expires_at,
            refresh_expires_at: &refresh_expires_at,
        },
    )
    .await
    {
        return server_error(e, "persisting issued tokens");
    }

    let scope = scopes_from_json(scopes_json)
        .map(|v| {
            v.iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let body = serde_json::json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL_SECS,
        "refresh_token": refresh,
        "scope": scope,
    });
    let mut resp = Json(body).into_response();
    // RFC 6749 §5.1: token responses must not be cached.
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

// --- Helpers ----------------------------------------------------------------

/// Resolve the session cookie to a `user_id`, or `None` if absent/invalid.
fn current_user(state: &AppState, jar: &CookieJar) -> Option<String> {
    let cookie = jar.get(SESSION_COOKIE)?;
    state
        .session
        .verify_session(cookie.value())
        .ok()
        .map(|claims| claims.sub)
}

/// Percent-encode a value for use in a URL query string.
fn enc(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

/// Escape a value for safe interpolation into HTML text/attributes.
fn esc(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Append `key=value` to a base URL, choosing `?` or `&` as needed.
fn append_query(url: &str, pair: &str) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}{pair}")
}

/// Redirect back to the client with an OAuth error (and the echoed `state`).
fn redirect_error(redirect_uri: &str, error: &str, state: Option<&str>) -> Response {
    let mut url = append_query(redirect_uri, &format!("error={error}"));
    if let Some(s) = state {
        url = append_query(&url, &format!("state={}", enc(s)));
    }
    Redirect::to(&url).into_response()
}

/// Redirect back to the client with the authorization `code` (and `state`).
fn redirect_success(redirect_uri: &str, code: &str, state: Option<&str>) -> Response {
    let mut url = append_query(redirect_uri, &format!("code={}", enc(code)));
    if let Some(s) = state {
        url = append_query(&url, &format!("state={}", enc(s)));
    }
    Redirect::to(&url).into_response()
}

/// A standalone error page (used when there is no safe redirect target).
fn error_page(status: StatusCode, message: &str) -> Response {
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>shisho</title></head>\
         <body style=\"font-family:system-ui;max-width:32rem;margin:4rem auto;padding:0 1rem\">\
         <h1>Authorization error</h1><p>{}</p></body></html>",
        esc(message)
    );
    (status, Html(html)).into_response()
}

/// An OAuth JSON error object (`{error, error_description}`).
fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

fn invalid_request(description: &str) -> Response {
    oauth_error(StatusCode::BAD_REQUEST, "invalid_request", description)
}

fn invalid_grant(description: &str) -> Response {
    oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", description)
}

fn server_error(err: sea_orm::DbErr, context: &str) -> Response {
    tracing::error!(error = ?err, "{context}");
    oauth_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "internal error",
    )
}

/// Minimal self-contained stylesheet for the two server-rendered OAuth pages.
/// These pages are emitted as raw HTML by the backend (the React app never
/// renders them), so they carry their own CSS and follow the system colour
/// scheme. No external resources are loaded: a self-hosted tool should not
/// phone home from its consent screen. Replace the wordmark/styling to taste.
const PAGE_STYLE: &str = r#"
*,*::before,*::after{box-sizing:border-box}
:root{color-scheme:light dark}
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;
  padding:2rem 1.25rem;line-height:1.5;
  font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,system-ui,sans-serif;
  background:#f3f0e9;color:#1b1a17}
@media (prefers-color-scheme:dark){body{background:#101218;color:#ecedf1}}
.card{width:100%;max-width:30rem;background:#fffdf9;border:1px solid #e6dfce;border-radius:16px;
  padding:2rem;box-shadow:0 18px 44px -22px rgba(40,34,20,.38)}
@media (prefers-color-scheme:dark){.card{background:#1b1e26;border-color:#2a2f3a}}
.brand{display:flex;align-items:center;justify-content:space-between;margin-bottom:1.25rem}
.brand-mark{font-weight:700}
.eyebrow{font-size:.66rem;font-weight:600;letter-spacing:.14em;text-transform:uppercase;opacity:.6}
h1{font-size:1.5rem;margin:0 0 .6rem}
.lead{margin:0 0 1.3rem;opacity:.8}
.scopes{list-style:none;margin:0 0 1.4rem;padding:0;display:flex;flex-direction:column;gap:.5rem}
.scope{padding:.75rem .9rem;border:1px solid #e6dfce;border-radius:10px;background:#f6f2e9}
@media (prefers-color-scheme:dark){.scope{border-color:#2a2f3a;background:#232734}}
.scope-title{font-weight:600}
.scope-desc{opacity:.7;font-size:.85rem}
.badges{display:flex;flex-wrap:wrap;gap:.35rem;margin-top:.4rem}
.badge{font-size:.7rem;font-weight:600;padding:.16rem .5rem;border-radius:999px;background:#e2f0ec;color:#0c7a6b}
.actions{display:flex;gap:.6rem}
.btn{flex:1;font:inherit;font-weight:600;padding:.7rem 1rem;border-radius:10px;cursor:pointer;border:1px solid transparent}
.btn-primary{background:#0c7a6b;color:#fff}
.btn-ghost{background:transparent;border-color:#d6cdb8}
.providers{list-style:none;margin:.4rem 0 0;padding:0;display:flex;flex-direction:column;gap:.6rem}
.provider{display:flex;justify-content:center;padding:.75rem 1rem;border-radius:10px;text-decoration:none;
  font-weight:600;color:inherit;background:#f6f2e9;border:1px solid #d6cdb8}
.footnote{margin:1.2rem 0 0;padding-top:1rem;border-top:1px solid #e6dfce;opacity:.65;font-size:.78rem}
"#;

/// Wrap a card body in the shared document shell: `<head>` + stylesheet, the
/// centered card, and the wordmark with an uppercase eyebrow label.
fn page_shell(eyebrow: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head>\
         <meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light dark\">\
         <title>shisho</title><style>{style}</style></head>\
         <body><main class=\"card\">\
         <div class=\"brand\"><span class=\"brand-mark\">shisho</span>\
         <span class=\"eyebrow\">{eyebrow}</span></div>\
         {body}</main></body></html>",
        style = PAGE_STYLE,
        eyebrow = esc(eyebrow),
        body = body,
    )
}

/// Title and one-line description for a resource, used to render each permission
/// as a human-readable row rather than a raw `resource:action` pair. Replace
/// these when you replace the placeholder `example` resource with your own.
fn resource_meta(resource: shisho_auth::keys::Resource) -> (&'static str, &'static str) {
    use shisho_auth::keys::Resource;
    match resource {
        Resource::Example => ("Example data", "The application's example resource"),
        Resource::Keys => ("API keys", "Create and revoke access credentials"),
        Resource::Candidates => (
            "Recommendation candidates",
            "Generate candidate pools from your library and the catalog",
        ),
        Resource::Lookup => (
            "Series lookup",
            "Look up the full picture of a single series",
        ),
        Resource::Library => ("Your library", "Search and list your own Codex library"),
        Resource::Verdicts => (
            "Recommendation verdicts",
            "Record recommendation decisions and feedback",
        ),
        Resource::Profile => (
            "Taste profile",
            "Read your taste profile and propose updates to it",
        ),
        Resource::Status => (
            "Service status",
            "Check backend reachability and data freshness",
        ),
    }
}

/// Badge label for an action.
fn action_label(action: shisho_auth::keys::Action) -> &'static str {
    use shisho_auth::keys::Action;
    match action {
        Action::Read => "View",
        Action::Write => "Create &amp; edit",
        Action::Delete => "Delete",
        Action::Manage => "Manage",
    }
}

/// Render the granted scopes as grouped permission rows: one row per resource,
/// each carrying a badge per action. Scope strings come from the fixed grant
/// set (not user input), so the static labels need no escaping.
fn scope_rows(scopes: &[shisho_auth::keys::Scope]) -> String {
    use shisho_auth::keys::{Action, Resource};
    let mut groups: Vec<(Resource, Vec<Action>)> = Vec::new();
    for s in scopes {
        match groups.iter_mut().find(|(r, _)| *r == s.resource) {
            Some((_, acts)) => acts.push(s.action),
            None => groups.push((s.resource, vec![s.action])),
        }
    }
    groups
        .iter()
        .map(|(resource, actions)| {
            let (title, desc) = resource_meta(*resource);
            let badges = actions
                .iter()
                .map(|a| format!("<span class=\"badge\">{}</span>", action_label(*a)))
                .collect::<String>();
            format!(
                "<li class=\"scope\"><span class=\"scope-title\">{title}</span>\
                 <span class=\"scope-desc\">{desc}</span>\
                 <span class=\"badges\">{badges}</span></li>"
            )
        })
        .collect()
}

/// Render a page that prompts the user to sign in, then returns to this exact
/// authorize request. Each provider links into the existing login flow with a
/// `redirect_to` back here.
fn login_page(state: &AppState, raw_query: Option<&str>) -> Response {
    let return_to = match raw_query {
        Some(q) => format!("/oauth/authorize?{q}"),
        None => "/oauth/authorize".to_string(),
    };
    let buttons = state
        .providers
        .list()
        .into_iter()
        .map(|p| {
            format!(
                "<li><a class=\"provider\" href=\"/auth/login/{name}?redirect_to={target}\">Sign in with {label}</a></li>",
                name = enc(&p.name),
                target = enc(&return_to),
                label = esc(&p.label),
            )
        })
        .collect::<String>();

    let body = format!(
        "<h1>Connect to shisho</h1>\
         <p class=\"lead\">Sign in to authorize this application.</p>\
         <ul class=\"providers\">{buttons}</ul>"
    );
    Html(page_shell("Sign in", &body)).into_response()
}

/// Render the consent screen for an authenticated user.
async fn consent_page(
    state: &AppState,
    client: &shisho_db::entities::oauth_clients::Model,
    redirect_uri: &str,
    code_challenge: &str,
    oauth_state: Option<&str>,
    _scope: Option<&str>,
    user_id: &str,
) -> Response {
    // Best-effort display name for the signed-in user.
    let who = match shisho_db::repos::users::get(&state.db, user_id).await {
        Ok(Some(u)) => u
            .email
            .or(u.display_name)
            .unwrap_or_else(|| "your account".into()),
        _ => "your account".into(),
    };
    let client_name = client
        .client_name
        .clone()
        .unwrap_or_else(|| "An application".into());
    let state_field = match oauth_state {
        Some(s) => format!(
            "<input type=\"hidden\" name=\"state\" value=\"{}\">",
            esc(s)
        ),
        None => String::new(),
    };

    let body = format!(
        "<h1>Authorize access</h1>\
         <p class=\"lead\"><strong>{client}</strong> wants to connect on behalf of <strong>{who}</strong> and will be able to:</p>\
         <ul class=\"scopes\">{scopes}</ul>\
         <form class=\"actions\" method=\"post\" action=\"/oauth/authorize/consent\">\
         <input type=\"hidden\" name=\"client_id\" value=\"{client_id}\">\
         <input type=\"hidden\" name=\"redirect_uri\" value=\"{redirect_uri}\">\
         <input type=\"hidden\" name=\"code_challenge\" value=\"{challenge}\">\
         {state_field}\
         <button class=\"btn btn-ghost\" type=\"submit\" name=\"decision\" value=\"deny\">Deny</button>\
         <button class=\"btn btn-primary\" type=\"submit\" name=\"decision\" value=\"allow\">Allow access</button>\
         </form>\
         <p class=\"footnote\">You can revoke this access anytime from your settings.</p>",
        client = esc(&client_name),
        who = esc(&who),
        scopes = scope_rows(&granted_scopes()),
        client_id = esc(&client.client_id),
        redirect_uri = esc(redirect_uri),
        challenge = esc(code_challenge),
        state_field = state_field,
    );
    Html(page_shell("Authorization request", &body)).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use shisho_auth::session::SessionKeys;
    use shisho_config::{AppConfig, AuthMode};
    use tower::ServiceExt;

    use crate::AppState;
    use crate::test_util::{TEST_SESSION_SECRET, test_state};

    // RFC 7636 Appendix B PKCE example.
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    const REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";

    async fn multi_user_state() -> Arc<AppState> {
        let mut cfg = AppConfig::default();
        cfg.auth.mode = AuthMode::MultiUser;
        cfg.server.public_url = Some("https://shisho.test".into());
        test_state(cfg).await
    }

    fn session_cookie(user: &str) -> String {
        let keys = SessionKeys::for_test(TEST_SESSION_SECRET);
        format!("shisho_session={}", keys.issue_session(user).unwrap())
    }

    fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri(uri);
        if let Some(c) = cookie {
            b = b.header(header::COOKIE, c);
        }
        b.body(Body::empty()).unwrap()
    }

    fn post_form(uri: &str, body: String, cookie: Option<&str>) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(c) = cookie {
            b = b.header(header::COOKIE, c);
        }
        b.body(Body::from(body)).unwrap()
    }

    fn post_json(uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn json_body(resp: axum::http::Response<Body>) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Register a client and return its `client_id`.
    async fn register_client(app: &axum::Router) -> String {
        let resp = app
            .clone()
            .oneshot(post_json(
                "/oauth/register",
                serde_json::json!({ "redirect_uris": [REDIRECT], "client_name": "Claude" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        json_body(resp).await["client_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Drive register → consent(allow) → return the authorization code.
    async fn obtain_code(app: &axum::Router, client_id: &str, session: &str) -> String {
        let body = format!(
            "client_id={client_id}&redirect_uri={}&code_challenge={CHALLENGE}&state=xyz&decision=allow",
            super::enc(REDIRECT)
        );
        let resp = app
            .clone()
            .oneshot(post_form("/oauth/authorize/consent", body, Some(session)))
            .await
            .unwrap();
        assert!(
            resp.status().is_redirection(),
            "consent redirects to the client"
        );
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            loc.starts_with(REDIRECT),
            "redirects to the registered uri: {loc}"
        );
        let code = loc
            .split_once("code=")
            .and_then(|(_, rest)| rest.split('&').next())
            .expect("code param present")
            .to_string();
        assert!(loc.contains("state=xyz"), "echoes state: {loc}");
        code
    }

    #[tokio::test]
    async fn discovery_documents_advertise_endpoints() {
        let app = crate::router(multi_user_state().await);

        let pr = json_body(
            app.clone()
                .oneshot(get("/.well-known/oauth-protected-resource", None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(pr["resource"], "https://shisho.test");
        assert_eq!(pr["authorization_servers"][0], "https://shisho.test");

        let as_doc = json_body(
            app.oneshot(get("/.well-known/oauth-authorization-server", None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            as_doc["authorization_endpoint"],
            "https://shisho.test/oauth/authorize"
        );
        assert_eq!(as_doc["token_endpoint"], "https://shisho.test/oauth/token");
        assert_eq!(
            as_doc["registration_endpoint"],
            "https://shisho.test/oauth/register"
        );
        assert_eq!(as_doc["code_challenge_methods_supported"][0], "S256");
        assert_eq!(as_doc["token_endpoint_auth_methods_supported"][0], "none");
    }

    #[tokio::test]
    async fn full_authorization_code_flow_yields_a_working_token() {
        let state = multi_user_state().await;
        let app = crate::router(state);
        let client_id = register_client(&app).await;
        let session = session_cookie("user-1");

        let authorize_uri = format!(
            "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&state=xyz",
            super::enc(REDIRECT)
        );
        let resp = app
            .clone()
            .oneshot(get(&authorize_uri, Some(&session)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "consent page");
        let html = String::from_utf8(
            resp.into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(html.contains("Allow"), "consent page has an Allow control");

        let code = obtain_code(&app, &client_id, &session).await;

        let body = format!(
            "grant_type=authorization_code&code={code}&redirect_uri={}&client_id={client_id}&code_verifier={VERIFIER}",
            super::enc(REDIRECT)
        );
        let resp = app
            .clone()
            .oneshot(post_form("/oauth/token", body, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let tok = json_body(resp).await;
        let access = tok["access_token"].as_str().unwrap().to_string();
        let refresh = tok["refresh_token"].as_str().unwrap().to_string();
        assert_eq!(tok["token_type"], "Bearer");
        assert!(access.starts_with("shisho_at_"));
        assert!(refresh.starts_with("shisho_rt_"));

        // The access token authenticates the REST API (same path MCP uses).
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/whoami")
                    .header(header::AUTHORIZATION, format!("Bearer {access}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "issued token authenticates");

        // Refresh rotates to a new pair.
        let resp = app
            .oneshot(post_form(
                "/oauth/token",
                format!("grant_type=refresh_token&refresh_token={refresh}"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let rotated = json_body(resp).await;
        assert!(
            rotated["access_token"]
                .as_str()
                .unwrap()
                .starts_with("shisho_at_")
        );
        assert_ne!(
            rotated["access_token"], access,
            "rotation issues a fresh token"
        );
    }

    #[tokio::test]
    async fn code_exchange_requires_correct_pkce_verifier() {
        let app = crate::router(multi_user_state().await);
        let client_id = register_client(&app).await;
        let code = obtain_code(&app, &client_id, &session_cookie("user-1")).await;

        let body = format!(
            "grant_type=authorization_code&code={code}&redirect_uri={}&client_id={client_id}&code_verifier=wrong-verifier",
            super::enc(REDIRECT)
        );
        let resp = app
            .oneshot(post_form("/oauth/token", body, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(resp).await["error"], "invalid_grant");
    }

    #[tokio::test]
    async fn used_code_cannot_be_replayed() {
        let app = crate::router(multi_user_state().await);
        let client_id = register_client(&app).await;
        let code = obtain_code(&app, &client_id, &session_cookie("user-1")).await;
        let body = || {
            format!(
                "grant_type=authorization_code&code={code}&redirect_uri={}&client_id={client_id}&code_verifier={VERIFIER}",
                super::enc(REDIRECT)
            )
        };
        let first = app
            .clone()
            .oneshot(post_form("/oauth/token", body(), None))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let second = app
            .oneshot(post_form("/oauth/token", body(), None))
            .await
            .unwrap();
        assert_eq!(
            second.status(),
            StatusCode::BAD_REQUEST,
            "code is single-use"
        );
    }

    #[tokio::test]
    async fn authorize_without_session_prompts_login() {
        let app = crate::router(multi_user_state().await);
        let client_id = register_client(&app).await;
        let uri = format!(
            "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&state=xyz",
            super::enc(REDIRECT)
        );
        let resp = app.oneshot(get(&uri, None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = String::from_utf8(
            resp.into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(
            html.contains("Sign in"),
            "unauthenticated authorize prompts login"
        );
    }

    #[tokio::test]
    async fn authorize_rejects_unregistered_redirect_uri() {
        let app = crate::router(multi_user_state().await);
        let client_id = register_client(&app).await;
        let uri = format!(
            "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256",
            super::enc("https://evil.example/steal")
        );
        let resp = app
            .oneshot(get(&uri, Some(&session_cookie("user-1"))))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn deny_redirects_with_access_denied() {
        let app = crate::router(multi_user_state().await);
        let client_id = register_client(&app).await;
        let body = format!(
            "client_id={client_id}&redirect_uri={}&code_challenge={CHALLENGE}&state=xyz&decision=deny",
            super::enc(REDIRECT)
        );
        let resp = app
            .oneshot(post_form(
                "/oauth/authorize/consent",
                body,
                Some(&session_cookie("user-1")),
            ))
            .await
            .unwrap();
        assert!(resp.status().is_redirection());
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            loc.contains("error=access_denied"),
            "deny reports access_denied: {loc}"
        );
    }

    #[tokio::test]
    async fn expired_access_token_is_rejected() {
        let state = multi_user_state().await;
        let access = shisho_oauth::secret::new_access_token();
        let refresh = shisho_oauth::secret::new_refresh_token();
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        shisho_oauth::tokens::issue(
            &state.db,
            shisho_oauth::tokens::NewTokens {
                access_hash: &shisho_oauth::secret::hash(&access),
                refresh_hash: &shisho_oauth::secret::hash(&refresh),
                client_id: "c",
                user_id: "user-1",
                scopes: &shisho_auth::keys::scopes_to_json(&shisho_oauth::granted_scopes()),
                access_expires_at: &past,
                refresh_expires_at: &past,
            },
        )
        .await
        .unwrap();
        let app = crate::router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/whoami")
                    .header(header::AUTHORIZATION, format!("Bearer {access}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "expired token is rejected"
        );
    }

    #[tokio::test]
    async fn oauth_surface_absent_in_single_user_mode() {
        let app = crate::router(test_state(AppConfig::default()).await);
        for uri in [
            "/.well-known/oauth-authorization-server",
            "/.well-known/oauth-protected-resource",
            "/oauth/authorize",
        ] {
            let resp = app.clone().oneshot(get(uri, None)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri} must be absent");
        }
    }
}
