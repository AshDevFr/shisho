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
pub mod setup;
pub mod static_files;
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
