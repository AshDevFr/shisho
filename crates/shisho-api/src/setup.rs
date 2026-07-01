//! First-run setup wizard.
//!
//! When an instance starts **unconfigured** (single-user mode with no
//! `auth.api_key` and no `[[auth.providers]]`), `shisho serve` mints a one-shot
//! setup token, prints a `/setup?token=...` URL beside the generated admin key,
//! and exposes the token-gated endpoints below. The token is the only thing
//! standing between an exposed fresh instance and a takeover, so every endpoint
//! 404s without it (and on an already-configured instance, where the token is
//! never set).
//!
//! - `GET  /api/v1/setup`       — report unconfigured state (mode, provider count)
//! - `POST /api/v1/setup/key`   — generate and reveal an admin key, then consume
//!   the token (single-user mode)
//!
//! Writing OIDC provider config is out of scope: the wizard *displays* the TOML
//! the operator must add; the config file stays the source of truth.

use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use utoipa::ToSchema;

use super::AppState;
use super::error::AppError;
use shisho_config::{AppConfig, AuthMode};

/// One-shot setup token state. `Some` while the instance is unconfigured and
/// the token is unused; `None` once consumed or on a configured instance.
pub struct SetupState {
    token: Mutex<Option<String>>,
}

impl SetupState {
    /// Enable setup, guarded by `token`.
    pub fn enabled(token: impl Into<String>) -> Self {
        Self {
            token: Mutex::new(Some(token.into())),
        }
    }

    /// Setup closed: every endpoint 404s.
    pub fn disabled() -> Self {
        Self {
            token: Mutex::new(None),
        }
    }

    /// Whether `provided` matches the live token (constant-time). False when
    /// setup is disabled or already consumed.
    fn matches(&self, provided: &str) -> bool {
        let guard = self.token.lock().expect("setup token mutex");
        match guard.as_deref() {
            Some(token) => bool::from(token.as_bytes().ct_eq(provided.as_bytes())),
            None => false,
        }
    }

    /// Invalidate the token so it cannot be reused (one-shot).
    fn consume(&self) {
        *self.token.lock().expect("setup token mutex") = None;
    }

    /// The live token, for the one-time operator-facing `/setup?token=...` URL
    /// printed at startup. `None` when setup is closed.
    pub fn token_for_display(&self) -> Option<String> {
        self.token.lock().expect("setup token mutex").clone()
    }
}

/// A fresh instance is "unconfigured" when single-user mode has no static key
/// and no providers: there is nothing to authenticate against yet, so the
/// wizard is offered. Any configured credential closes it.
pub fn is_unconfigured(cfg: &AppConfig) -> bool {
    cfg.auth.api_key.is_none() && cfg.auth.providers.is_empty()
}

/// Generate a high-entropy one-shot setup token.
pub fn generate_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/setup", get(setup_state))
        .route("/setup/key", post(generate_key))
}

#[derive(Deserialize)]
pub struct TokenQuery {
    token: Option<String>,
}

/// Reject unless a valid setup token is present; collapses "setup disabled",
/// "no token", and "wrong token" all to 404 so the endpoint's existence leaks
/// nothing on a configured instance.
fn require_token(state: &AppState, q: &TokenQuery) -> Result<(), AppError> {
    let ok = q.token.as_deref().is_some_and(|t| state.setup.matches(t));
    if ok {
        Ok(())
    } else {
        Err(AppError::NotFound("not found".into()))
    }
}

/// Unconfigured-state report for the wizard.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupStateResponse {
    /// Configured auth mode (`single_user` / `multi_user`).
    pub auth_mode: String,
    /// Number of `[[auth.providers]]` already present in config.
    pub providers_configured: usize,
}

/// Report the first-run state (token-gated).
#[utoipa::path(
    get,
    path = "/api/v1/setup",
    tag = "setup",
    params(("token" = String, Query, description = "One-shot setup token")),
    responses(
        (status = 200, body = SetupStateResponse),
        (status = 404, body = super::error::ErrorBody, description = "Setup is closed or the token is wrong")
    )
)]
pub async fn setup_state(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TokenQuery>,
) -> Result<Json<SetupStateResponse>, AppError> {
    require_token(&state, &q)?;
    Ok(Json(SetupStateResponse {
        auth_mode: state.cfg.auth.mode.as_str().to_string(),
        providers_configured: state.cfg.auth.providers.len(),
    }))
}

/// The revealed admin key. Returned exactly once.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupKeyResponse {
    /// Plaintext admin API key. Store it now; it is never shown again.
    pub key: String,
}

/// Generate and reveal an admin key, then consume the setup token (single-user
/// mode only; multi-user identities come from configured providers).
#[utoipa::path(
    post,
    path = "/api/v1/setup/key",
    tag = "setup",
    params(("token" = String, Query, description = "One-shot setup token")),
    responses(
        (status = 200, body = SetupKeyResponse),
        (status = 400, body = super::error::ErrorBody, description = "Not applicable in multi-user mode"),
        (status = 404, body = super::error::ErrorBody, description = "Setup is closed or the token is wrong")
    )
)]
pub async fn generate_key(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TokenQuery>,
) -> Result<Json<SetupKeyResponse>, AppError> {
    require_token(&state, &q)?;
    if state.cfg.auth.mode == AuthMode::MultiUser {
        return Err(AppError::BadRequest(
            "multi-user mode authenticates via configured providers, not a generated key".into(),
        ));
    }

    let user_id = shisho_auth::repo::ensure_local_user(&state.db)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    let (_, plaintext) = shisho_auth::repo::create_key(
        &state.db,
        Some(user_id),
        "setup",
        &shisho_auth::keys::Scope::all(),
        None,
    )
    .await
    .map_err(AppError::Internal)?;

    // One-shot: the token is spent the moment a key is minted.
    state.setup.consume();
    Ok(Json(SetupKeyResponse { key: plaintext }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate as api;
    use crate::test_util::{TEST_SETUP_TOKEN, test_state};

    fn cfg_unconfigured() -> shisho_config::AppConfig {
        shisho_config::AppConfig::default()
    }

    #[test]
    fn unconfigured_detection() {
        use super::is_unconfigured;
        use shisho_config::ProviderConfig;

        assert!(is_unconfigured(&cfg_unconfigured()));

        let mut cfg = cfg_unconfigured();
        cfg.auth.api_key = Some("shisho_x".into());
        assert!(!is_unconfigured(&cfg));

        let mut cfg = cfg_unconfigured();
        cfg.auth.providers = vec![ProviderConfig::default()];
        assert!(!is_unconfigured(&cfg));
    }

    async fn json(resp: axum::response::Response) -> (StatusCode, serde_json::Value) {
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn setup_requires_token() {
        let app = api::router(test_state(cfg_unconfigured()).await);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/setup")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/setup?token=nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn setup_404_when_configured() {
        let mut cfg = cfg_unconfigured();
        cfg.auth.api_key = Some("shisho_configured".into());
        let app = api::router(test_state(cfg).await);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/setup?token={TEST_SETUP_TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn state_reports_mode_with_valid_token() {
        let app = api::router(test_state(cfg_unconfigured()).await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/setup?token={TEST_SETUP_TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = json(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["authMode"], "single_user");
        assert_eq!(body["providersConfigured"], 0);
    }

    #[tokio::test]
    async fn wizard_key_authenticates_immediately_and_token_is_one_shot() {
        let state = test_state(cfg_unconfigured()).await;
        let app = api::router(state);

        // Generate a key through the wizard.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/setup/key?token={TEST_SETUP_TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = json(resp).await;
        assert_eq!(status, StatusCode::OK);
        let key = body["key"].as_str().unwrap().to_string();
        assert!(key.starts_with("shisho_"));

        // The key works against a protected route without a restart.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/whoami")
                    .header(header::AUTHORIZATION, format!("Bearer {key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // The token is spent: a second call 404s.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/setup/key?token={TEST_SETUP_TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
