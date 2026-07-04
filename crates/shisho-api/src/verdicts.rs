//! Verdict-log management, `/api/v1/verdicts`. A signed-in user browses their
//! append-only recommendation log (filterable, grouped by session) and records
//! a reading outcome for a series.
//!
//! The log is append-only (PRD Principle 7): nothing here updates or deletes a
//! row. Recording an outcome **appends a feedback row** — the same mechanism the
//! MCP `record_feedback` tool uses — and resolves any open reconciliation flag,
//! so the correction feeds the consolidation loop and shows up as a new entry on
//! refetch. Reads require `verdicts` read access; the outcome append requires
//! `verdicts:write` (derived from the HTTP method by the shared resource guard).
//!
//! Keyed on the caller's `codex_user_sub`, like the profile endpoints: a user
//! who has not minted a Codex key yet has no verdict log and gets a clear 409.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::AppState;
use super::error::AppError;
use crate::middleware::{CurrentScope, require_scope};
use shisho_auth::keys::Resource;
use shisho_db::entities::verdict;
use shisho_db::repos::verdicts::{NewVerdict, SeriesKey};
use shisho_db::repos::{reconciliation, users, verdicts};
use shisho_ids::ExternalId;

/// Session id for feedback rows recorded outside a recommend run (matches the
/// MCP `record_feedback` tool).
const FEEDBACK_SESSION: &str = "feedback";

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_verdicts))
        .route("/{id}/outcome", post(record_outcome))
        .layer(middleware::from_fn(|req, next| {
            require_scope(Resource::Verdicts, req, next)
        }))
}

/// Filters narrowing the verdict log. Every field is optional; omitted fields
/// do not constrain. A row must match all provided fields to appear.
#[derive(Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
pub struct VerdictFilters {
    /// Exact tier match (e.g. "S", "A", "pass", "feedback").
    pub tier: Option<String>,
    /// Only verified (true) or unverified (false) rows.
    pub verified: Option<bool>,
    /// Only retracted (true) or standing (false) rows.
    pub retracted: Option<bool>,
    /// Exact outcome match (e.g. "loved", "dropped").
    pub outcome: Option<String>,
}

/// One verdict-log row, as the UI renders it.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VerdictDto {
    pub id: i64,
    /// Canonical `provider:id` (or `mangabaka:<id>`) when the row carries a
    /// series identity, else absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_id: Option<String>,
    pub tier: String,
    pub rationale: String,
    pub verified: bool,
    pub retracted: bool,
    /// `failedVerification` (stays excluded from pools) or `withdrawn` (returns
    /// to candidacy), when retracted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retraction_reason: Option<String>,
    /// The reading outcome recorded on this row (feedback rows carry it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub created_at: String,
}

/// One recommendation session's rows, grouped together.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionGroup {
    pub session_id: String,
    /// The earliest row in the session (ISO 8601).
    pub started_at: String,
    pub verdicts: Vec<VerdictDto>,
}

/// The reading outcome to record for a series.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordOutcomeRequest {
    /// What happened (e.g. "loved", "liked", "dropped", "mixed").
    pub outcome: String,
    /// Optional free-form notes on why.
    #[serde(default)]
    pub notes: Option<String>,
}

/// Resolve the signed-in user's `codex_user_sub` — the verdict-log key. Shares
/// the profile endpoints' contract: a request with no owning user or a user who
/// has not minted a Codex key yet gets a clear 409.
async fn codex_user_sub(state: &AppState, scope: &CurrentScope) -> Result<String, AppError> {
    let user_id = scope.0.owner().ok_or_else(|| {
        AppError::Conflict("this credential is not tied to a user, so it has no verdict log".into())
    })?;
    let user = users::get(&state.db, &user_id)
        .await
        .map_err(|e| AppError::Internal(e.into()))?
        .ok_or_else(|| AppError::NotFound("user not found".into()))?;
    user.codex_user_sub.ok_or_else(|| {
        AppError::Conflict(
            "no Codex identity yet: sign in to mint a Codex key before viewing your verdict log"
                .into(),
        )
    })
}

/// Canonical `provider:id` (or `mangabaka:<id>`) of a verdict row, when it
/// carries a series identity.
fn series_id(row: &verdict::Model) -> Option<String> {
    match SeriesKey::of(row) {
        Some(SeriesKey::Mangabaka(id)) => Some(format!("mangabaka:{id}")),
        Some(SeriesKey::Pair(provider, external)) => Some(format!("{provider}:{external}")),
        None => None,
    }
}

fn to_dto(row: verdict::Model) -> VerdictDto {
    VerdictDto {
        series_id: series_id(&row),
        id: row.id,
        tier: row.tier,
        rationale: row.rationale,
        verified: row.verified,
        retracted: row.retracted,
        retraction_reason: row.retraction_reason,
        outcome: row.outcome,
        notes: row.notes,
        created_at: row.created_at.to_rfc3339(),
    }
}

fn matches(row: &verdict::Model, f: &VerdictFilters) -> bool {
    f.tier.as_ref().is_none_or(|t| &row.tier == t)
        && f.verified.is_none_or(|v| row.verified == v)
        && f.retracted.is_none_or(|r| row.retracted == r)
        && f.outcome
            .as_ref()
            .is_none_or(|o| row.outcome.as_deref() == Some(o.as_str()))
}

/// The caller's verdict log, filtered and grouped by session (most recent
/// session first). The whole log is small by construction, so filtering and
/// grouping happen in memory.
#[utoipa::path(
    get,
    path = "/api/v1/verdicts",
    tag = "verdicts",
    params(VerdictFilters),
    responses(
        (status = 200, body = [SessionGroup]),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 409, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn list_verdicts(
    State(state): State<Arc<AppState>>,
    scope: CurrentScope,
    Query(filters): Query<VerdictFilters>,
) -> Result<Json<Vec<SessionGroup>>, AppError> {
    let sub = codex_user_sub(&state, &scope).await?;
    // Oldest-first from the repo; group preserving arrival order, then present
    // most-recent session first.
    let rows = verdicts::list_for_user(&state.db, &sub)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;

    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, SessionGroup> =
        std::collections::HashMap::new();
    for row in rows {
        if !matches(&row, &filters) {
            continue;
        }
        let created = row.created_at.to_rfc3339();
        let session = row.session_id.clone();
        match groups.get_mut(&session) {
            Some(g) => g.verdicts.push(to_dto(row)),
            None => {
                order.push(session.clone());
                groups.insert(
                    session.clone(),
                    SessionGroup {
                        session_id: session,
                        started_at: created,
                        verdicts: vec![to_dto(row)],
                    },
                );
            }
        }
    }

    // Most recent session first (by its first row's arrival).
    let mut out: Vec<SessionGroup> = order
        .into_iter()
        .filter_map(|s| groups.remove(&s))
        .collect();
    out.reverse();
    Ok(Json(out))
}

/// Record a reading outcome for the series a verdict points at. Appends a
/// feedback row (the log stays append-only) and closes any open reconciliation
/// question on the series.
#[utoipa::path(
    post,
    path = "/api/v1/verdicts/{id}/outcome",
    tag = "verdicts",
    params(("id" = i64, Path, description = "the verdict whose series the outcome is for")),
    request_body = RecordOutcomeRequest,
    responses(
        (status = 201, body = VerdictDto),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 404, body = super::error::ErrorBody),
        (status = 409, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn record_outcome(
    State(state): State<Arc<AppState>>,
    scope: CurrentScope,
    Path(id): Path<i64>,
    Json(req): Json<RecordOutcomeRequest>,
) -> Result<(axum::http::StatusCode, Json<VerdictDto>), AppError> {
    let sub = codex_user_sub(&state, &scope).await?;

    let target = verdicts::find_for_user(&state.db, &sub, id)
        .await
        .map_err(|e| AppError::Internal(e.into()))?
        .ok_or_else(|| AppError::NotFound(format!("verdict {id} not found")))?;

    // The feedback row needs the target's series identity to join.
    let Some(token) = series_id(&target) else {
        return Err(AppError::Conflict(
            "this verdict has no series identity to attach an outcome to".into(),
        ));
    };
    let external_id = ExternalId::from_tsundoku(&token)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("rebuilding series id {token}: {e}")))?;

    let row = verdicts::insert(
        &state.db,
        &sub,
        NewVerdict {
            external_id: Some(external_id.clone()),
            session_id: FEEDBACK_SESSION.into(),
            tier: FEEDBACK_SESSION.into(),
            rationale: "outcome recorded in the web UI".into(),
            verified: false,
            retracted: false,
            retraction_reason: None,
            outcome: Some(req.outcome),
            notes: req.notes,
        },
    )
    .await
    .map_err(|e| AppError::Internal(e.into()))?;

    // An outcome answers any pending reconciliation question on the series.
    reconciliation::resolve_for_series(&state.db, &sub, [&external_id])
        .await
        .map_err(|e| AppError::Internal(e.into()))?;

    Ok((axum::http::StatusCode::CREATED, Json(to_dto(row))))
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

    async fn app() -> (axum::Router, std::sync::Arc<api::AppState>) {
        let mut cfg = AppConfig::default();
        cfg.auth.mode = AuthMode::MultiUser;
        let state = test_state(cfg).await;
        users::ActiveModel {
            id: Set(USER.to_owned()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            codex_user_sub: Set(Some(SUB.to_owned())),
            ..Default::default()
        }
        .insert(&state.db)
        .await
        .expect("seed user");
        (api::router(state.clone()), state)
    }

    fn cookie() -> String {
        let keys = SessionKeys::for_test(TEST_SESSION_SECRET);
        format!("shisho_session={}", keys.issue_session(USER).unwrap())
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .header(header::COOKIE, cookie())
            .body(Body::empty())
            .unwrap()
    }

    fn post_json(uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::COOKIE, cookie())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn seed(state: &api::AppState, session: &str, tier: &str, raw_id: &str) -> i64 {
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
        .expect("seed verdict")
        .id
    }

    #[tokio::test]
    async fn lists_grouped_by_session_most_recent_first() {
        let (app, state) = app().await;
        seed(&state, "session-a", "S", "mangabaka:1").await;
        seed(&state, "session-a", "A", "mangabaka:2").await;
        seed(&state, "session-b", "B", "mangabaka:3").await;

        let resp = app.oneshot(get("/api/v1/verdicts")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let groups = json_body(resp).await;
        let arr = groups.as_array().unwrap();
        assert_eq!(arr.len(), 2, "two sessions");
        assert_eq!(arr[0]["sessionId"], "session-b", "most recent first");
        assert_eq!(arr[1]["sessionId"], "session-a");
        assert_eq!(arr[1]["verdicts"].as_array().unwrap().len(), 2);
        assert_eq!(arr[1]["verdicts"][0]["seriesId"], "mangabaka:1");
    }

    #[tokio::test]
    async fn filters_narrow_the_log() {
        let (app, state) = app().await;
        seed(&state, "s1", "S", "mangabaka:1").await;
        seed(&state, "s1", "pass", "mangabaka:2").await;

        let resp = app
            .oneshot(get("/api/v1/verdicts?tier=pass"))
            .await
            .unwrap();
        let groups = json_body(resp).await;
        let arr = groups.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let verdicts = arr[0]["verdicts"].as_array().unwrap();
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0]["tier"], "pass");
    }

    #[tokio::test]
    async fn recording_an_outcome_appends_a_feedback_row() {
        let (app, state) = app().await;
        let id = seed(&state, "s1", "S", "mangabaka:1").await;

        let resp = app
            .clone()
            .oneshot(post_json(
                &format!("/api/v1/verdicts/{id}/outcome"),
                serde_json::json!({"outcome": "loved"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created = json_body(resp).await;
        assert_eq!(created["outcome"], "loved");
        assert_eq!(created["seriesId"], "mangabaka:1");

        // The original row is untouched (append-only): the log now has a
        // feedback row alongside it, and the outcome shows on refetch.
        let resp = app
            .oneshot(get("/api/v1/verdicts?outcome=loved"))
            .await
            .unwrap();
        let groups = json_body(resp).await;
        let arr = groups.as_array().unwrap();
        assert_eq!(arr.len(), 1, "the feedback session");
        assert_eq!(arr[0]["sessionId"], "feedback");
        assert_eq!(arr[0]["verdicts"][0]["outcome"], "loved");

        // The original S-tier verdict still exists, unmodified.
        let all = shisho_db::repos::verdicts::list_for_user(&state.db, SUB)
            .await
            .unwrap();
        assert_eq!(all.len(), 2, "original + appended feedback");
        assert!(all.iter().any(|r| r.id == id && r.outcome.is_none()));
    }

    #[tokio::test]
    async fn outcome_on_an_unknown_verdict_is_404() {
        let (app, _state) = app().await;
        let resp = app
            .oneshot(post_json(
                "/api/v1/verdicts/999/outcome",
                serde_json::json!({"outcome": "loved"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn no_codex_identity_is_409() {
        let mut cfg = AppConfig::default();
        cfg.auth.mode = AuthMode::MultiUser;
        let state = test_state(cfg).await;
        users::ActiveModel {
            id: Set(USER.to_owned()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            codex_user_sub: Set(None),
            ..Default::default()
        }
        .insert(&state.db)
        .await
        .unwrap();
        let app = api::router(state);
        let resp = app.oneshot(get("/api/v1/verdicts")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }
}
