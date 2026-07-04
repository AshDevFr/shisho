//! Operational status, `/api/v1/status`. A signed-in user sees backend
//! reachability, cache freshness, the MangaBaka dump age + refresh state, their
//! unresolvable-series list (with Codex deep links), and their minted-key state,
//! and can trigger a dump refresh.
//!
//! Status content mirrors the MCP `status` tool (both call the shared
//! `shisho_services::health::probe`). Per-user bits (the unresolvable list, the
//! minted-key state) key on the caller's `codex_user_sub`; the unresolvable list
//! reads the credential-less cached snapshot (`snapshots.peek`), so it never
//! originates a Codex call from a browser session.
//!
//! Reads require `status:read`. The dump-refresh action is an operator action
//! gated on `status:write` (derived from the POST method by the shared resource
//! guard).

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::error::AppError;
use crate::middleware::{CurrentScope, require_scope};
use shisho_auth::keys::Resource;
use shisho_db::repos::users;
use shisho_services::Services;
use shisho_services::health::{self, HealthResponse};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(get_status))
        .route("/dump-refresh", post(trigger_dump_refresh))
        .layer(middleware::from_fn(|req, next| {
            require_scope(Resource::Status, req, next)
        }))
}

/// The caller's operational status.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponse {
    /// Backend reachability + cache freshness + backbone state (mirrors the MCP
    /// `status` tool and the health probe).
    pub health: HealthResponse,
    /// The caller's library snapshot view, when a warm snapshot exists (built on
    /// their last authenticated touch). Absent when none is cached — no Codex
    /// call is made from this browser session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library: Option<LibraryStatus>,
    /// The caller's minted Codex key state.
    pub minted_key: MintedKeyStatus,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LibraryStatus {
    /// Age of the cached snapshot (ISO 8601).
    pub as_of: String,
    pub series_count: usize,
    pub unresolvable_count: usize,
    pub unresolvable: Vec<UnresolvableEntry>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnresolvableEntry {
    pub series_id: String,
    pub title: String,
    /// The raw `source:externalId` strings that failed to map, verbatim.
    pub raw_sources: Vec<String>,
    /// Deep link to the series in Codex, where metadata matching is fixed
    /// (PRD Principle 8: fix at the source, never patch mappings locally).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_url: Option<String>,
}

/// The caller's minted Codex key state. `present` says a key is stored; the UI
/// offers the OIDC re-auth "regenerate" flow when the user wants a fresh one (or
/// when it has been revoked and the per-user surfaces started failing).
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MintedKeyStatus {
    /// A Codex key is stored for this user.
    pub present: bool,
    /// The resolved `codex_user_sub`, when a key is stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_user_sub: Option<String>,
}

/// Resolve the signed-in user's `codex_user_sub`, or `None` when the user has no
/// Codex identity yet (the status page still renders — it just shows no key and
/// no per-user library view).
async fn user_row(
    state: &AppState,
    scope: &CurrentScope,
) -> Result<Option<shisho_db::entities::users::Model>, AppError> {
    let Some(user_id) = scope.0.owner() else {
        return Ok(None);
    };
    users::get(&state.db, &user_id)
        .await
        .map_err(|e| AppError::Internal(e.into()))
}

fn services(state: &AppState) -> Result<&Services, AppError> {
    state.services.as_ref().ok_or_else(|| {
        AppError::Unavailable(
            "the recommendation service is not configured on this instance".into(),
        )
    })
}

/// Assemble the caller's operational status.
#[utoipa::path(
    get,
    path = "/api/v1/status",
    tag = "status",
    responses(
        (status = 200, body = StatusResponse),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 503, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn get_status(
    State(state): State<Arc<AppState>>,
    scope: CurrentScope,
) -> Result<Json<StatusResponse>, AppError> {
    let services = services(&state)?;
    let health = health::probe(&state.db, services).await;

    let user = user_row(&state, &scope).await?;
    let codex_user_sub = user.as_ref().and_then(|u| u.codex_user_sub.clone());

    // Per-user library view from the credential-less cached snapshot (never
    // builds; absent when the user has no warm snapshot).
    let library = codex_user_sub.as_deref().and_then(|sub| {
        services.snapshots.peek(sub).map(|hit| {
            let codex_base = services.snapshots.codex().base_url().to_owned();
            let snapshot = hit.value;
            LibraryStatus {
                as_of: hit.as_of.to_rfc3339(),
                series_count: snapshot.series.len(),
                unresolvable_count: snapshot.unresolvable.len(),
                unresolvable: snapshot
                    .unresolvable
                    .iter()
                    .map(|u| UnresolvableEntry {
                        series_id: u.series_id.clone(),
                        title: u.title.clone(),
                        raw_sources: u.raw_sources.clone(),
                        codex_url: (!codex_base.is_empty())
                            .then(|| format!("{codex_base}/series/{}", u.series_id)),
                    })
                    .collect(),
            }
        })
    });

    let minted_key = MintedKeyStatus {
        present: user
            .as_ref()
            .is_some_and(|u| u.codex_key_ciphertext.is_some()),
        codex_user_sub,
    };

    Ok(Json(StatusResponse {
        health,
        library,
        minted_key,
    }))
}

/// Trigger a MangaBaka dump refresh in the background. Returns immediately; the
/// refresh (download + ingest) runs to completion asynchronously and the
/// provider serializes overlapping runs. Poll `GET /status` for `refreshRunning`
/// and the new dump age.
#[utoipa::path(
    post,
    path = "/api/v1/status/dump-refresh",
    tag = "status",
    responses(
        (status = 202),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 503, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn trigger_dump_refresh(
    State(state): State<Arc<AppState>>,
    _scope: CurrentScope,
) -> Result<StatusCode, AppError> {
    let services = services(&state)?;
    let provider = std::sync::Arc::clone(services.metadata.provider());
    // Fire-and-forget: the provider skips an overlapping run, and the outcome
    // lands in its status (last_refresh + refreshRunning), which GET /status
    // surfaces. Nothing to await here.
    tokio::spawn(async move {
        shisho_services::sweeps::run_mangabaka_refresh(&provider).await;
    });
    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};
    use tower::ServiceExt;

    use crate as api;
    use crate::test_util::{TEST_SESSION_SECRET, test_state};
    use shisho_auth::session::SessionKeys;
    use shisho_config::{AppConfig, AuthMode};
    use shisho_db::entities::users;

    const USER: &str = "user-alice";

    // The status endpoints need a configured `Services` on the state; the
    // default test state has none (no codex.base_url), so these assert the
    // service-unconfigured contract. A fuller integration test with wiremock
    // backends lives with the domain crates.
    async fn app() -> axum::Router {
        let mut cfg = AppConfig::default();
        cfg.auth.mode = AuthMode::MultiUser;
        let state = test_state(cfg).await;
        users::ActiveModel {
            id: Set(USER.to_owned()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        }
        .insert(&state.db)
        .await
        .expect("seed user");
        api::router(state)
    }

    fn cookie() -> String {
        let keys = SessionKeys::for_test(TEST_SESSION_SECRET);
        format!("shisho_session={}", keys.issue_session(USER).unwrap())
    }

    fn req(method: &str, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, cookie())
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn status_requires_a_configured_service() {
        let app = app().await;
        let resp = app.oneshot(req("GET", "/api/v1/status")).await.unwrap();
        // No codex.base_url in the default test config → no Services.
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn dump_refresh_requires_a_configured_service() {
        let app = app().await;
        let resp = app
            .oneshot(req("POST", "/api/v1/status/dump-refresh"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn status_needs_authentication() {
        let app = app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // Body is not asserted; the point is the auth gate fires before dispatch.
        let _ = resp.into_body().collect().await;
    }
}
