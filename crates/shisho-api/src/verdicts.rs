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

use std::collections::HashMap;
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
use shisho_db::repos::verdicts::{SeriesKey, VerdictState, verdict_series_token};
use shisho_db::repos::{users, verdicts};
use shisho_ids::ExternalId;
use shisho_services::recommendations::is_standing;

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
    /// The series' current outcome (e.g. "loved", "dropped"): matches every
    /// row of a series whose latest outcome is this, feedback row or not.
    pub outcome: Option<String>,
    /// `pending`: rows of series still waiting on the reader (what Home
    /// lists). `settled`: everything else (an outcome, a pass, a retraction).
    /// Omitted: both.
    pub state: Option<LogState>,
    /// `false` hides feedback (outcome) rows; omitted or `true` keeps them.
    pub feedback: Option<bool>,
}

/// Whether a series still waits on the reader.
#[derive(Deserialize, ToSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogState {
    Pending,
    Settled,
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
    /// The series' current outcome, folded over the whole log, so a
    /// recommendation row shows how it ended without its feedback row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_outcome: Option<String>,
    /// Whether the series is still a standing recommendation (no outcome,
    /// pass or retraction yet): the rows an outcome can be recorded against.
    pub pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub created_at: String,
    /// Series title, resolved from the MangaBaka backbone (absent when the
    /// service is not configured or the id does not resolve).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Cover image URL, when the backbone has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    /// Deep link to the series on MangaBaka (only for MangaBaka-keyed verdicts).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mangabaka_url: Option<String>,
    /// Deep link to the series in Codex, when the caller owns it (the snapshot
    /// maps its identity to a Codex UUID).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_url: Option<String>,
    /// The series description from the MangaBaka backbone, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Deep link to the series in Tsundoku, when Tsundoku's catalog knows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tsundoku_url: Option<String>,
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
pub(crate) async fn codex_user_sub(
    state: &AppState,
    scope: &CurrentScope,
) -> Result<String, AppError> {
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

pub(crate) fn to_dto(row: verdict::Model) -> VerdictDto {
    VerdictDto {
        series_id: verdict_series_token(&row),
        id: row.id,
        tier: row.tier,
        rationale: row.rationale,
        verified: row.verified,
        retracted: row.retracted,
        retraction_reason: row.retraction_reason,
        series_outcome: row.outcome.clone(),
        outcome: row.outcome,
        // Set from the series fold by the list handlers.
        pending: false,
        notes: row.notes,
        created_at: row.created_at.to_rfc3339(),
        // Filled in by `enrich` when the recommendation service is configured.
        title: None,
        cover_url: None,
        mangabaka_url: None,
        codex_url: None,
        summary: None,
        tsundoku_url: None,
    }
}

/// Enrich verdict DTOs in place with series title, summary, cover, and deep
/// links. Best-effort per field (see `shisho_services::verdict_views`).
pub(crate) async fn enrich(
    services: &shisho_services::Services,
    codex_index: &shisho_services::library_snapshot::CodexIndex,
    dtos: &mut [VerdictDto],
) {
    let series: Vec<Option<String>> = dtos.iter().map(|d| d.series_id.clone()).collect();
    let links = shisho_services::verdict_views::verdict_links(services, codex_index, &series).await;
    for (dto, links) in dtos.iter_mut().zip(links) {
        dto.title = links.title;
        dto.cover_url = links.cover_url;
        dto.summary = links.summary;
        dto.mangabaka_url = links.mangabaka_url;
        dto.codex_url = links.codex_url;
        dto.tsundoku_url = links.tsundoku_url;
    }
}

/// The caller's cached Codex index (external id -> series UUID), the reliable
/// source for Codex deep links. Delegates to the SWR-cached
/// `snapshots.external_index`, so repeated page loads reuse one lightweight
/// `external-index` sweep instead of re-scanning Codex each time. Empty (links
/// absent) when there is no usable credential or the build errors.
pub(crate) async fn codex_external_index(
    state: &AppState,
    scope: &CurrentScope,
) -> Arc<shisho_services::library_snapshot::CodexIndex> {
    let empty = || Arc::new(shisho_services::library_snapshot::CodexIndex::new());
    let (Some(user_id), Some(dek), Some(identity), Some(services)) = (
        scope.0.owner(),
        state.dek.as_ref(),
        state.codex_identity.as_ref(),
        state.services.as_ref(),
    ) else {
        return empty();
    };
    let ctx = match identity.resolve(&state.db, dek, &user_id).await {
        Ok(ctx) => ctx,
        Err(err) => {
            tracing::debug!(error = %err, "codex identity resolve failed; codex links omitted");
            return empty();
        }
    };
    match services
        .snapshots
        .external_index(&ctx.user_sub, &ctx.credential)
        .await
    {
        Ok(cached) => cached.value,
        Err(err) => {
            tracing::debug!(error = %err, "codex external-index build failed; codex links omitted");
            empty()
        }
    }
}

/// Where a row's series stands: its current outcome and whether it is still
/// pending. A row with no series identity is its own series.
struct SeriesView {
    outcome: Option<String>,
    pending: bool,
}

fn series_views(rows: &[verdict::Model]) -> Vec<SeriesView> {
    let mut groups: HashMap<SeriesKey, Vec<verdict::Model>> = HashMap::new();
    for row in rows {
        if let Some(key) = SeriesKey::of(row) {
            groups.entry(key).or_default().push(row.clone());
        }
    }
    let folded: HashMap<SeriesKey, VerdictState> = groups
        .into_iter()
        .map(|(key, rows)| (key, verdicts::fold(&rows)))
        .collect();
    rows.iter()
        .map(|row| {
            let state = match SeriesKey::of(row).and_then(|k| folded.get(&k)) {
                Some(state) => state.clone(),
                None => verdicts::fold(std::slice::from_ref(row)),
            };
            SeriesView {
                pending: is_standing(&state),
                outcome: state.outcome,
            }
        })
        .collect()
}

fn matches(row: &verdict::Model, series: &SeriesView, f: &VerdictFilters) -> bool {
    f.tier.as_ref().is_none_or(|t| &row.tier == t)
        && f.verified.is_none_or(|v| row.verified == v)
        && f.retracted.is_none_or(|r| row.retracted == r)
        && f.outcome
            .as_ref()
            .is_none_or(|o| series.outcome.as_deref() == Some(o.as_str()))
        && f.state
            .is_none_or(|s| (s == LogState::Pending) == series.pending)
        && (f.feedback != Some(false) || row.outcome.is_none())
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
    // Oldest-first from the repo; keep the session id + arrival time alongside
    // each row so grouping survives the enrichment pass.
    // The series state folds over the whole log, before any filter drops rows.
    let all = verdicts::list_for_user(&state.db, &sub)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    let views = series_views(&all);
    let rows: Vec<(verdict::Model, SeriesView)> = all
        .into_iter()
        .zip(views)
        .filter(|(r, v)| matches(r, v, &filters))
        .collect();
    let sessions: Vec<String> = rows.iter().map(|(r, _)| r.session_id.clone()).collect();
    let createds: Vec<String> = rows
        .iter()
        .map(|(r, _)| r.created_at.to_rfc3339())
        .collect();
    let mut dtos: Vec<VerdictDto> = rows
        .into_iter()
        .map(|(row, view)| VerdictDto {
            series_outcome: view.outcome,
            pending: view.pending,
            ..to_dto(row)
        })
        .collect();

    // Title / cover / deep links, when the recommendation service is configured.
    if let Some(services) = state.services.as_ref() {
        // Build the external-id -> Codex UUID map on demand from the caller's
        // slim external-index, so Codex links resolve even on a cold snapshot.
        let codex_index = codex_external_index(&state, &scope).await;
        enrich(services, codex_index.as_ref(), &mut dtos).await;
    }

    // Group preserving first-seen session order, then present most-recent first.
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, SessionGroup> = HashMap::new();
    for ((session, created), dto) in sessions.into_iter().zip(createds).zip(dtos) {
        match groups.get_mut(&session) {
            Some(g) => g.verdicts.push(dto),
            None => {
                order.push(session.clone());
                groups.insert(
                    session.clone(),
                    SessionGroup {
                        session_id: session,
                        started_at: created,
                        verdicts: vec![dto],
                    },
                );
            }
        }
    }

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
    let Some(token) = verdict_series_token(&target) else {
        return Err(AppError::Conflict(
            "this verdict has no series identity to attach an outcome to".into(),
        ));
    };
    let external_id = ExternalId::from_tsundoku(&token)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("rebuilding series id {token}: {e}")))?;

    // One write path with the MCP tool: repeats are no-ops, the outcome is
    // normalized, and it answers any pending reconciliation question.
    let ack = shisho_services::verdicts::record_feedback(
        &state.db,
        &sub,
        &external_id,
        &req.outcome,
        req.notes,
    )
    .await
    .map_err(|e| AppError::Internal(e.into()))?;

    let mut dto = to_dto(ack.row);
    if let Some(services) = state.services.as_ref() {
        let codex_index = codex_external_index(&state, &scope).await;
        enrich(
            services,
            codex_index.as_ref(),
            std::slice::from_mut(&mut dto),
        )
        .await;
    }
    Ok((axum::http::StatusCode::CREATED, Json(dto)))
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
        assert_eq!(arr.len(), 2, "the feedback session and the original one");
        assert_eq!(arr[0]["sessionId"], "feedback");
        assert_eq!(arr[0]["verdicts"][0]["outcome"], "loved");
        assert_eq!(arr[1]["verdicts"][0]["seriesOutcome"], "loved");

        // The original S-tier verdict still exists, unmodified.
        let all = shisho_db::repos::verdicts::list_for_user(&state.db, SUB)
            .await
            .unwrap();
        assert_eq!(all.len(), 2, "original + appended feedback");
        assert!(all.iter().any(|r| r.id == id && r.outcome.is_none()));
    }

    /// Every row of the flattened response, as `(seriesId, tier)`.
    fn rows(groups: &serde_json::Value) -> Vec<(String, String)> {
        groups
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["verdicts"].as_array().unwrap().iter())
            .map(|v| {
                (
                    v["seriesId"].as_str().unwrap().to_string(),
                    v["tier"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// One standing pick, one read and loved, one passed.
    async fn seed_mixed_log(app: &axum::Router, state: &api::AppState) {
        seed(state, "s1", "A", "mangabaka:1").await;
        let loved = seed(state, "s1", "S", "mangabaka:2").await;
        seed(state, "s1", "pass", "mangabaka:3").await;
        let resp = app
            .clone()
            .oneshot(post_json(
                &format!("/api/v1/verdicts/{loved}/outcome"),
                serde_json::json!({"outcome": "loved"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn pending_lists_only_standing_recommendations() {
        let (app, state) = app().await;
        seed_mixed_log(&app, &state).await;

        let resp = app
            .oneshot(get("/api/v1/verdicts?state=pending&feedback=false"))
            .await
            .unwrap();
        let groups = json_body(resp).await;
        assert_eq!(rows(&groups), [("mangabaka:1".into(), "A".into())]);
        assert_eq!(groups[0]["verdicts"][0]["pending"], true);
    }

    #[tokio::test]
    async fn settled_rows_carry_the_series_outcome_without_feedback_rows() {
        let (app, state) = app().await;
        seed_mixed_log(&app, &state).await;

        let resp = app
            .clone()
            .oneshot(get("/api/v1/verdicts?state=settled&feedback=false"))
            .await
            .unwrap();
        let groups = json_body(resp).await;
        assert_eq!(
            rows(&groups),
            [
                ("mangabaka:2".into(), "S".into()),
                ("mangabaka:3".into(), "pass".into())
            ]
        );
        let loved = &groups[0]["verdicts"][0];
        assert_eq!(loved["seriesOutcome"], "loved");
        assert_eq!(loved["pending"], false);

        // With feedback rows shown, the outcome row joins its series.
        let resp = app
            .oneshot(get("/api/v1/verdicts?state=settled"))
            .await
            .unwrap();
        let tiers: Vec<String> = rows(&json_body(resp).await)
            .into_iter()
            .map(|(_, t)| t)
            .collect();
        assert!(tiers.contains(&"feedback".to_string()));
    }

    #[tokio::test]
    async fn the_outcome_filter_matches_the_series_not_just_its_feedback_row() {
        let (app, state) = app().await;
        seed_mixed_log(&app, &state).await;

        let resp = app
            .oneshot(get("/api/v1/verdicts?outcome=loved&feedback=false"))
            .await
            .unwrap();
        assert_eq!(
            rows(&json_body(resp).await),
            [("mangabaka:2".into(), "S".into())]
        );
    }

    #[tokio::test]
    async fn recording_the_same_outcome_twice_appends_one_feedback_row() {
        let (app, state) = app().await;
        let id = seed(&state, "s1", "S", "mangabaka:1").await;

        for outcome in ["loved", "Loved"] {
            let resp = app
                .clone()
                .oneshot(post_json(
                    &format!("/api/v1/verdicts/{id}/outcome"),
                    serde_json::json!({"outcome": outcome, "notes": "the art"}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        let all = shisho_db::repos::verdicts::list_for_user(&state.db, SUB)
            .await
            .unwrap();
        assert_eq!(all.len(), 2, "original + one feedback row, not two");
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
