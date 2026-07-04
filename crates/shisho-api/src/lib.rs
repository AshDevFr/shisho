//! Everything axum: the shared [`AppState`], the router, the route handlers and
//! their DTOs, the [`error::AppError`] envelope, the auth middleware, the
//! OpenAPI document, and the embedded SPA fallback.

pub mod admin_routes;
pub mod auth_routes;
pub mod connections;
pub mod docs;
pub mod error;
pub mod health;
pub mod info;
pub mod keys;
pub mod middleware;
pub mod oauth_routes;
pub mod profile;
pub mod setup;
pub mod static_files;
pub mod status;
pub mod verdicts;
pub mod whoami;

#[cfg(any(test, feature = "test-util"))]
pub mod test_util;

use std::sync::Arc;

use axum::routing::get;
use axum::{Json, Router};
use sea_orm::DatabaseConnection;
use shisho_auth::keys::Resource;
use shisho_auth::providers::ProviderRegistry;
use shisho_auth::session::SessionKeys;
use shisho_config::{AppConfig, AuthMode};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

use crate::middleware::{require_auth, require_scope};

/// Shared state every handler reads. Cheap to clone (`Arc`/`Clone` fields).
///
/// As you add a domain, give its handlers what they need here (a service
/// handle, a cache, an event bus). The skeleton keeps it minimal: the database,
/// config, the login provider registry, session-cookie keys, and the first-run
/// setup state.
#[derive(Clone)]
pub struct AppState {
    /// The primary (and only) database: global `users` / `api_keys` / `oauth_*`
    /// plus any tenant data, row-scoped by `user_id` in multi-user mode.
    pub db: DatabaseConnection,
    pub cfg: AppConfig,
    /// Configured login providers (multi-user mode). Empty in single-user mode.
    pub providers: Arc<ProviderRegistry>,
    /// HMAC key material + policy for session and login-state cookies.
    pub session: SessionKeys,
    /// First-run setup wizard state (one-shot token).
    pub setup: Arc<setup::SetupState>,
    /// The data-encryption key, present only when at-rest encryption is
    /// configured. Needed at request time to seal the minted Codex key; `None`
    /// disables Codex-credential minting (a recoverable key must be encrypted).
    pub dek: Option<shisho_db::Dek>,
    /// The Codex identity resolver (client + `/me` validation cache), present
    /// only when `codex.base_url` is configured. Resolves each per-user request
    /// to a `UserCtx`; `None` disables per-user Codex surfaces.
    pub codex_identity: Option<Arc<shisho_services::codex_identity::CodexIdentity>>,
    /// The domain service registry (library snapshot, tsundoku cache, metadata
    /// backbone, sweeps) the MCP tools adapt over. Present on the same
    /// condition as `codex_identity` — the domain surface needs the caller's
    /// Codex library — so `Some` iff `codex.base_url` is configured.
    pub services: Option<shisho_services::Services>,
}

/// How long a stored Codex key stays trusted between `/me` re-validations, so a
/// revocation propagates within this window.
const CODEX_VALIDATION_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Build the Codex identity resolver from config: `Some` when `codex.base_url`
/// is set, else `None` (per-user Codex surfaces are disabled). Kept here so the
/// binary's `build_app_state` need not depend on `shisho-codex`/`shisho-services`
/// directly.
pub fn build_codex_identity(
    cfg: &AppConfig,
) -> Option<Arc<shisho_services::codex_identity::CodexIdentity>> {
    if cfg.codex.base_url.is_empty() {
        return None;
    }
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("building the Codex HTTP client");
    let client = shisho_codex::CodexClient::new(&cfg.codex.base_url, http);
    Some(Arc::new(
        shisho_services::codex_identity::CodexIdentity::new(client, CODEX_VALIDATION_TTL),
    ))
}

/// Build the domain [`shisho_services::Services`] registry from config and the
/// shared DB handle: the Codex/Tsundoku clients, the MangaBaka backbone (with
/// any on-disk dump loaded best-effort — download/refresh stay the scheduler's
/// job), and the SWR caches. `Some` on the same condition as
/// [`build_codex_identity`] (the domain tools need the caller's Codex library),
/// so the two stay in lockstep. Kept here so `build_app_state` need not depend
/// on the client crates directly.
pub async fn build_services(
    cfg: &AppConfig,
    db: sea_orm::DatabaseConnection,
) -> anyhow::Result<Option<shisho_services::Services>> {
    use anyhow::Context;

    if cfg.codex.base_url.is_empty() {
        return Ok(None);
    }
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("building the domain HTTP client")?;

    let codex = shisho_codex::CodexClient::new(&cfg.codex.base_url, http.clone());
    let tsundoku = shisho_tsundoku::TsundokuClient::new(
        &cfg.tsundoku.base_url,
        cfg.tsundoku.admin_bearer.clone(),
        http,
    );
    let mangabaka = std::sync::Arc::new(
        shisho_mangabaka::MangabakaProvider::new(&cfg.mangabaka, cfg.database.mangabaka_dir())
            .context("building the MangaBaka provider")?,
    );
    // Serve an existing dump immediately if one is on disk; a missing dump is
    // fine (API-fallback / cold until the scheduler refreshes it).
    if mangabaka.load_existing_dump().await {
        tracing::info!("loaded the on-disk MangaBaka dump");
    }

    Ok(Some(shisho_services::Services::new(
        codex,
        tsundoku,
        mangabaka,
        db,
        &cfg.cache,
        cfg.tsundoku.owned_prefilter,
    )))
}

/// Build the application router.
///
/// `/api/v1/*` carries the JSON API; everything else falls through to the
/// embedded SPA so client-side routing works on hard refresh. The SPA fallback
/// must stay last.
pub fn router(state: Arc<AppState>) -> Router {
    build_router(state, Router::new())
}

/// [`router`] with extra routes mounted behind the auth + `example`-scope
/// layers. Tests use this to probe the per-resource scope middleware without a
/// real data route.
#[cfg(test)]
pub fn router_with_protected(state: Arc<AppState>, extra: Router<Arc<AppState>>) -> Router {
    build_router(state, extra)
}

fn build_router(state: Arc<AppState>, extra_example_scoped: Router<Arc<AppState>>) -> Router {
    let docs_enabled = state.cfg.api.docs;

    // Public by design: health for probes, info for the login-less first-run UX
    // (it leaks nothing beyond name/version/mode). The setup routes are public
    // but token-gated, and 404 on a configured instance.
    let public = Router::new()
        .route("/health", get(health::health))
        .route("/info", get(info::info))
        .merge(setup::router());

    // The placeholder `example` resource: any route added here is gated by
    // `(example, <method-action>)`. The skeleton mounts nothing of its own —
    // this is where your domain's CRUD routes go. The test probe rides this
    // guard; the action is derived from the HTTP method.
    let example_resource = extra_example_scoped.layer(axum::middleware::from_fn(|req, next| {
        require_scope(Resource::Example, req, next)
    }));

    let protected = Router::new()
        .route("/whoami", get(whoami::whoami))
        .merge(example_resource)
        // keys::router() carries its own keys:manage guard.
        .nest("/keys", keys::router())
        // Connected-app (OAuth grant) management; same keys:manage guard.
        .nest("/connections", connections::router())
        // Taste-profile management; carries its own profile:read/write guard.
        .nest("/profile", profile::router())
        // Verdict-log browsing + outcome recording; own verdicts:read/write guard.
        .nest("/verdicts", verdicts::router())
        // Operational status + dump-refresh action; own status:read/write guard.
        .nest("/status", status::router())
        // Admin routes self-gate on require_admin inside each handler.
        .merge(admin_routes::router())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ));

    // Browser-facing login routes and the OAuth authorization server both live
    // at the root and are public. Both mount only in multi-user mode, so
    // single-user instances keep that surface untouched.
    let (auth, oauth) = if state.cfg.auth.mode == AuthMode::MultiUser {
        (auth_routes::router(), oauth_routes::router())
    } else {
        (Router::new(), Router::new())
    };

    let mut app = Router::new()
        .nest("/api/v1", public.merge(protected).with_state(state.clone()))
        .merge(auth.with_state(state.clone()))
        .merge(oauth.with_state(state))
        .route("/openapi.json", get(openapi_json))
        .fallback(get(static_files::serve_static))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    if docs_enabled {
        app = app.merge(Scalar::with_url("/docs", docs::ApiDoc::openapi()));
    }
    app
}

/// The OpenAPI document. Public like `/docs`: it is API metadata, and the
/// frontend type generation consumes it.
async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(docs::ApiDoc::openapi())
}
