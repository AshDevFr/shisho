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
use shisho_db::repos::verdicts::verdict_series_token;
use shisho_db::repos::{users, verdicts};
use shisho_ids::ExternalId;

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

fn to_dto(row: verdict::Model) -> VerdictDto {
    VerdictDto {
        series_id: verdict_series_token(&row),
        id: row.id,
        tier: row.tier,
        rationale: row.rationale,
        verified: row.verified,
        retracted: row.retracted,
        retraction_reason: row.retraction_reason,
        outcome: row.outcome,
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

/// The public MangaBaka series URL for a numeric id: `https://mangabaka.org/<id>`.
fn mangabaka_public_url(id: &str) -> String {
    format!("https://mangabaka.org/{id}")
}

/// The canonical Tsundoku join key for an external id (`<source>:<id>`), so a
/// verdict's identity and a Tsundoku feed item's identities compare equal.
fn canon(e: &ExternalId) -> String {
    e.as_tsundoku()
        .unwrap_or_else(|| format!("{}:{}", e.provider, e.id))
}

/// Enrich verdict DTOs in place with series title, summary, cover, and deep
/// links (MangaBaka / Codex / Tsundoku). Every field is best-effort: a metadata
/// gap, an unowned series, or an unreachable backend just leaves the
/// corresponding field absent. Metadata lookups run concurrently.
async fn enrich(
    services: &shisho_services::Services,
    codex_index: &std::collections::HashMap<String, String>,
    dtos: &mut [VerdictDto],
) {
    let codex_base = services.snapshots.codex().base_url().to_owned();

    // Parse each row's series identity once; resolve their metadata together.
    let parsed: Vec<Option<ExternalId>> = dtos
        .iter()
        .map(|d| {
            d.series_id
                .as_deref()
                .and_then(|t| ExternalId::from_tsundoku(t).ok())
        })
        .collect();

    let metas = futures_util::future::join_all(parsed.iter().map(|p| async {
        match p {
            Some(eid) => services.metadata.resolve(eid).await.ok().flatten(),
            None => None,
        }
    }))
    .await;

    // Tsundoku deep links: resolve every row's identity against Tsundoku's feed
    // in one paginated pass, mapping each known external id to its Tsundoku
    // series id. Best-effort: a feed error just omits the links.
    let tsundoku_ids = tsundoku_series_ids(services, &parsed).await;
    let tsundoku_base = services.tsundoku.client().base_url().to_owned();

    for ((dto, parsed), meta) in dtos.iter_mut().zip(parsed.iter()).zip(metas) {
        if let Some(meta) = meta {
            dto.title = Some(meta.title);
            dto.cover_url = meta.cover_url;
            dto.summary = meta.description;
        }
        if let Some(id) = dto
            .series_id
            .as_deref()
            .and_then(|t| t.strip_prefix("mangabaka:"))
        {
            dto.mangabaka_url = Some(mangabaka_public_url(id));
        }
        let Some(eid) = parsed.as_ref() else { continue };
        if !codex_base.is_empty()
            && let Some(uuid) = codex_index.get(&canon(eid))
        {
            dto.codex_url = Some(format!(
                "{}/series/{}",
                codex_base.trim_end_matches('/'),
                uuid
            ));
        }
        if !tsundoku_base.is_empty()
            && let Some(series_id) = tsundoku_ids.get(&canon(eid))
        {
            dto.tsundoku_url = Some(format!(
                "{}/series/{}",
                tsundoku_base.trim_end_matches('/'),
                series_id
            ));
        }
    }
}

/// The caller's cached Codex index (external id -> series UUID), the reliable
/// source for Codex deep links. Delegates to the SWR-cached
/// `snapshots.external_index`, so repeated page loads reuse one lightweight
/// `external-index` sweep instead of re-scanning Codex each time. Empty (links
/// absent) when there is no usable credential or the build errors.
async fn codex_external_index(
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

/// Resolve the parsed series identities against Tsundoku's feed in one paginated
/// pass, returning a map from canonical external id -> Tsundoku series id. Empty
/// when Tsundoku is unconfigured, nothing parses, or the feed errors.
async fn tsundoku_series_ids(
    services: &shisho_services::Services,
    parsed: &[Option<ExternalId>],
) -> std::collections::HashMap<String, i32> {
    let mut out: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
    let client = services.tsundoku.client();
    if !client.is_configured() {
        return out;
    }
    let tokens: Vec<String> = parsed
        .iter()
        .flatten()
        .filter_map(|e| e.as_tsundoku())
        .collect();
    if tokens.is_empty() {
        return out;
    }
    let mut cursor: Option<String> = None;
    loop {
        match client
            .feed_filtered(cursor.as_deref(), Some(200), &tokens)
            .await
        {
            Ok(page) => {
                for item in &page.items {
                    for eid in &item.external_ids {
                        out.insert(canon(&eid.normalized()), item.series_id);
                    }
                }
                cursor = page.next_cursor;
                if !page.has_more || cursor.is_none() {
                    break;
                }
            }
            Err(err) => {
                tracing::debug!(error = %err, "tsundoku feed resolution failed; tsundoku links omitted");
                break;
            }
        }
    }
    out
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
    // Oldest-first from the repo; keep the session id + arrival time alongside
    // each row so grouping survives the enrichment pass.
    let rows: Vec<verdict::Model> = verdicts::list_for_user(&state.db, &sub)
        .await
        .map_err(|e| AppError::Internal(e.into()))?
        .into_iter()
        .filter(|r| matches(r, &filters))
        .collect();
    let sessions: Vec<String> = rows.iter().map(|r| r.session_id.clone()).collect();
    let createds: Vec<String> = rows.iter().map(|r| r.created_at.to_rfc3339()).collect();
    let mut dtos: Vec<VerdictDto> = rows.into_iter().map(to_dto).collect();

    // Title / cover / deep links, when the recommendation service is configured.
    if let Some(services) = state.services.as_ref() {
        // Build the external-id -> Codex UUID map on demand from the caller's
        // slim external-index, so Codex links resolve even on a cold snapshot.
        let codex_index = codex_external_index(&state, &scope).await;
        enrich(services, codex_index.as_ref(), &mut dtos).await;
    }

    // Group preserving first-seen session order, then present most-recent first.
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, SessionGroup> =
        std::collections::HashMap::new();
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

    #[test]
    fn mangabaka_public_url_is_the_org_site() {
        assert_eq!(
            super::mangabaka_public_url("9741"),
            "https://mangabaka.org/9741"
        );
    }

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
