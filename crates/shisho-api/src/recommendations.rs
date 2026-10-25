//! Current recommendations, `/api/v1/recommendations`: what Home renders.
//!
//! The verdict log re-evaluated per series (see
//! `shisho_services::recommendations`): one entry per standing series across
//! all sessions, split into *recommended* and *reading* by the caller's live
//! Codex library. Outcomes are recorded through the verdict endpoint
//! (`POST /verdicts/{id}/outcome`) against the entry's verdict id.
//!
//! `POST /sync` is the web twin of the MCP `reconcile` tool: re-read the
//! library from Codex with the caller's stored key and run the full feedback
//! pass, so a rating set a minute ago shows up without waiting.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::error::AppError;
use super::verdicts::{VerdictDto, codex_external_index, codex_user_sub, enrich, to_dto};
use crate::middleware::{CurrentScope, require_scope};
use shisho_auth::keys::Resource;
use shisho_codex::CodexError;
use shisho_services::recommendations::{self, Current};
use shisho_services::{PassError, UserCtx};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(current))
        .route("/sync", post(sync))
        .layer(middleware::from_fn(|req, next| {
            require_scope(Resource::Verdicts, req, next)
        }))
}

/// One standing recommendation.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecommendationDto {
    /// The latest standing verdict for the series; record outcomes against
    /// its `id`.
    pub verdict: VerdictDto,
    /// How many recommendation sessions named the series.
    pub times_recommended: usize,
    /// The caller owns a copy in Codex (an acquirable pick bought since).
    pub in_library: bool,
    /// Books read across the furthest-along owned copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub books_read: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book_count: Option<i64>,
}

/// The re-evaluated list, ordered for display (tier, then newest).
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CurrentRecommendationsResponse {
    /// When the library snapshot the split is based on was read (ISO 8601).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_as_of: Option<String>,
    /// Why the library could not be read, when it could not. Every standing
    /// series is then listed under `recommended`, unchecked against reading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_gap: Option<String>,
    /// Standing, not started.
    pub recommended: Vec<RecommendationDto>,
    /// Standing, started in Codex, waiting for an outcome.
    pub reading: Vec<RecommendationDto>,
}

/// What a sync recorded.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncResponse {
    pub library_as_of: String,
    /// Outcomes recorded (or withdrawn) from what Codex shows.
    pub outcomes_inferred: usize,
    /// Reconciliation questions newly raised.
    pub questions_raised: usize,
    /// Reconciliation questions closed.
    pub questions_resolved: usize,
    /// Whether ranking parameters were recomputed.
    pub retuned: bool,
}

/// The caller's stored Codex identity, or why it cannot be used.
async fn user_ctx(state: &AppState, scope: &CurrentScope) -> Result<UserCtx, String> {
    let (Some(user_id), Some(dek), Some(identity)) = (
        scope.0.owner(),
        state.dek.as_ref(),
        state.codex_identity.as_ref(),
    ) else {
        return Err("Codex is not configured on this instance".into());
    };
    identity
        .resolve(&state.db, dek, &user_id)
        .await
        .map_err(|err| {
            err.reauth_message()
                .map(str::to_owned)
                .unwrap_or_else(|| err.to_string())
        })
}

/// The caller's current recommendations, re-evaluated against their library.
#[utoipa::path(
    get,
    path = "/api/v1/recommendations",
    tag = "recommendations",
    responses(
        (status = 200, body = CurrentRecommendationsResponse),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 409, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn current(
    State(state): State<Arc<AppState>>,
    scope: CurrentScope,
) -> Result<Json<CurrentRecommendationsResponse>, AppError> {
    let sub = codex_user_sub(&state, &scope).await?;

    // Best effort: without the library the list is still the log's answer,
    // and the gap says what is unchecked.
    let snapshot = match (state.services.as_ref(), user_ctx(&state, &scope).await) {
        (None, _) => Err("the recommendation service is not configured".to_owned()),
        (_, Err(gap)) => Err(gap),
        (Some(services), Ok(ctx)) => services
            .snapshots
            .get(&ctx.user_sub, &ctx.credential)
            .await
            .map_err(|err| format!("reading the library from Codex failed: {err}")),
    };
    let (library_as_of, library_gap, snapshot) = match snapshot {
        Ok(hit) => (Some(hit.as_of.to_rfc3339()), None, Some(hit.value)),
        Err(gap) => (None, Some(gap), None),
    };

    let current = recommendations::current(&state.db, &sub, snapshot.as_deref())
        .await
        .map_err(|e| AppError::Internal(e.into()))?;

    let split = current.recommended.len();
    let entries: Vec<Current> = current
        .recommended
        .into_iter()
        .chain(current.reading)
        .collect();
    let mut dtos: Vec<VerdictDto> = entries.iter().map(|c| to_dto(c.verdict.clone())).collect();
    if let Some(services) = state.services.as_ref() {
        let codex_index = codex_external_index(&state, &scope).await;
        enrich(services, codex_index.as_ref(), &mut dtos).await;
    }
    let mut all: Vec<RecommendationDto> = entries
        .into_iter()
        .zip(dtos)
        .map(|(c, verdict)| RecommendationDto {
            verdict,
            times_recommended: c.times_recommended,
            in_library: c.in_library,
            books_read: c.books_read,
            book_count: c.book_count,
        })
        .collect();
    let reading = all.split_off(split);

    Ok(Json(CurrentRecommendationsResponse {
        library_as_of,
        library_gap,
        recommended: all,
        reading,
    }))
}

/// Re-read the caller's library from Codex and run the full feedback pass.
#[utoipa::path(
    post,
    path = "/api/v1/recommendations/sync",
    tag = "recommendations",
    responses(
        (status = 200, body = SyncResponse),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 409, body = super::error::ErrorBody),
        (status = 503, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn sync(
    State(state): State<Arc<AppState>>,
    scope: CurrentScope,
) -> Result<Json<SyncResponse>, AppError> {
    codex_user_sub(&state, &scope).await?;
    let services = state.services.as_ref().ok_or_else(|| {
        AppError::Unavailable("the recommendation service is not configured".into())
    })?;
    let ctx = user_ctx(&state, &scope).await.map_err(AppError::Conflict)?;
    let (as_of, outcome) = match services.reconcile_now(&ctx.user_sub, &ctx.credential).await {
        Ok(done) => done,
        Err(PassError::Codex(CodexError::Unauthorized { .. })) => {
            return Err(AppError::Conflict(
                "Your Codex authorization expired or was revoked. \
                 Re-authenticate at the Shisho UI to restore access."
                    .into(),
            ));
        }
        Err(PassError::Codex(err)) => {
            return Err(AppError::Unavailable(format!(
                "reading the library from Codex failed: {err}"
            )));
        }
        Err(PassError::Db(err)) => return Err(AppError::Internal(err.into())),
    };
    Ok(Json(SyncResponse {
        library_as_of: as_of.to_rfc3339(),
        outcomes_inferred: outcome.reconcile.inferred,
        questions_raised: outcome.reconcile.flagged,
        questions_resolved: outcome.reconcile.resolved,
        retuned: outcome.retuned.is_some(),
    }))
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
    use shisho_db::repos::verdicts::{self, NewVerdict};
    use shisho_ids::ExternalId;

    const USER: &str = "user-alice";
    const SUB: &str = "codex-alice";

    async fn app(codex_sub: Option<&str>) -> (axum::Router, std::sync::Arc<api::AppState>) {
        let mut cfg = AppConfig::default();
        cfg.auth.mode = AuthMode::MultiUser;
        let state = test_state(cfg).await;
        users::ActiveModel {
            id: Set(USER.to_owned()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            codex_user_sub: Set(codex_sub.map(str::to_owned)),
            ..Default::default()
        }
        .insert(&state.db)
        .await
        .expect("seed user");
        (api::router(state.clone()), state)
    }

    fn request(method: &str, uri: &str) -> Request<Body> {
        let keys = SessionKeys::for_test(TEST_SESSION_SECRET);
        Request::builder()
            .method(method)
            .uri(uri)
            .header(
                header::COOKIE,
                format!("shisho_session={}", keys.issue_session(USER).unwrap()),
            )
            .body(Body::empty())
            .unwrap()
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn seed(state: &api::AppState, session: &str, tier: &str, raw_id: &str) {
        verdicts::insert(
            &state.db,
            SUB,
            NewVerdict {
                external_id: Some(ExternalId::from_tsundoku(raw_id).unwrap()),
                session_id: session.into(),
                tier: tier.into(),
                rationale: "seeded".into(),
                verified: true,
                retracted: false,
                retraction_reason: None,
                outcome: None,
                notes: None,
            },
        )
        .await
        .expect("seed verdict");
    }

    #[tokio::test]
    async fn recommendations_without_a_library_are_the_log_view_with_a_gap() {
        let (app, state) = app(Some(SUB)).await;
        seed(&state, "s1", "B", "mangabaka:1").await;
        seed(&state, "s2", "A", "mangabaka:1").await;
        seed(&state, "s2", "pass", "mangabaka:2").await;

        let resp = app
            .oneshot(request("GET", "/api/v1/recommendations"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        let recommended = body["recommended"].as_array().unwrap();
        assert_eq!(recommended.len(), 1);
        assert_eq!(recommended[0]["verdict"]["seriesId"], "mangabaka:1");
        assert_eq!(recommended[0]["verdict"]["tier"], "A");
        assert_eq!(recommended[0]["timesRecommended"], 2);
        assert_eq!(recommended[0]["inLibrary"], false);
        assert!(body["reading"].as_array().unwrap().is_empty());
        assert!(body["libraryGap"].is_string(), "{body}");
        assert!(body.get("libraryAsOf").is_none());
    }

    #[tokio::test]
    async fn recommendations_need_a_codex_identity() {
        let (app, _) = app(None).await;
        let resp = app
            .oneshot(request("GET", "/api/v1/recommendations"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn recommendations_sync_without_codex_is_unavailable() {
        let (app, _) = app(Some(SUB)).await;
        let resp = app
            .oneshot(request("POST", "/api/v1/recommendations/sync"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
