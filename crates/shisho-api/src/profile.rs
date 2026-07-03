//! Taste-profile management, `/api/v1/profile`. The out-of-band half of the
//! governed-document loop (PRD Principle 6): a signed-in user reads their
//! current profile and its version history, edits it directly (a new applied
//! version), and reviews the pending consolidation proposals produced by the
//! `consolidate` MCP prompt — approving or rejecting them here, never in chat.
//!
//! The profile is keyed on the caller's `codex_user_sub` (the same key the MCP
//! `get_profile` tool uses), so every handler resolves the signed-in user's
//! `codex_user_sub` first; a user who has not yet minted a Codex key has no
//! profile identity and gets a clear 409.
//!
//! Reads require `profile:read`, writes `profile:write` — the router derives the
//! action from the HTTP method via the shared resource guard, so a browser
//! session (full scope set) qualifies while a narrowly-scoped key does not.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::error::AppError;
use crate::middleware::{CurrentScope, require_scope};
use shisho_auth::keys::Resource;
use shisho_db::repos::profiles::ProfileError;
use shisho_db::repos::{profiles, users};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(get_profile).put(edit_profile))
        .route("/raw", get(get_profile_raw))
        .route("/versions", get(list_versions))
        .route("/proposals/{version}/approve", post(approve_proposal))
        .route("/proposals/{version}/reject", post(reject_proposal))
        .layer(middleware::from_fn(|req, next| {
            require_scope(Resource::Profile, req, next)
        }))
}

/// The current taste profile plus any pending proposal awaiting review.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfileResponse {
    /// The current profile content (Markdown). Empty for a version-0 profile.
    pub content: String,
    pub version: i64,
    /// When the current version was applied (ISO 8601), absent at version 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// A consolidation proposal awaiting the user's approval, if one is pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_proposal: Option<PendingProposalDto>,
}

/// A stored pending proposal (a `consolidate` output awaiting review).
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PendingProposalDto {
    /// The version this proposal will become if approved.
    pub version: i64,
    /// The version it was drafted against.
    pub base_version: i64,
    /// Proposed Markdown content.
    pub content: String,
    /// One-line summary of what changed and why.
    pub change_summary: String,
    /// Unified diff from the current profile to the proposal.
    pub diff: String,
}

/// One entry in the version history, with its rendered diff.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct VersionDto {
    pub version: i64,
    pub base_version: i64,
    /// `pending` | `applied` | `rejected` | `superseded`.
    pub status: String,
    pub change_summary: String,
    pub created_at: String,
    /// Unified diff from the base version's content to this version's.
    pub diff: String,
}

/// A direct edit: the full replacement content, optionally pinned to the version
/// it was drafted against (a stale base is rejected).
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EditProfileRequest {
    /// The full new profile content (Markdown).
    pub content: String,
    /// One-line summary of what changed; defaults to a generic web-edit note.
    #[serde(default)]
    pub change_summary: Option<String>,
    /// The version this edit was drafted against; a mismatch is a 409.
    #[serde(default)]
    pub base_version: Option<i64>,
}

/// Resolve the signed-in user's `codex_user_sub` — the profile key. A request
/// with no owning user (an unowned admin/config key) or a user who has not
/// minted a Codex key yet has no profile identity: a clear 409, never a
/// silently-empty profile for the wrong key.
async fn codex_user_sub(state: &AppState, scope: &CurrentScope) -> Result<String, AppError> {
    let user_id = scope.0.owner().ok_or_else(|| {
        AppError::Conflict(
            "this credential is not tied to a user, so it has no taste profile".into(),
        )
    })?;
    let user = users::get(&state.db, &user_id)
        .await
        .map_err(|e| AppError::Internal(e.into()))?
        .ok_or_else(|| AppError::NotFound("user not found".into()))?;
    user.codex_user_sub.ok_or_else(|| {
        AppError::Conflict(
            "no Codex identity yet: sign in to mint a Codex key before using your taste profile"
                .into(),
        )
    })
}

fn map_profile_error(err: ProfileError) -> AppError {
    match err {
        ProfileError::StaleBase { .. } | ProfileError::NotPending { .. } => {
            AppError::Conflict(err.to_string())
        }
        ProfileError::NoSuchVersion { .. } => AppError::NotFound(err.to_string()),
        ProfileError::Db(e) => AppError::Internal(e.into()),
    }
}

async fn build_profile_response(state: &AppState, sub: &str) -> Result<ProfileResponse, AppError> {
    let profile = profiles::get_or_default(&state.db, sub)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    let pending = profiles::pending(&state.db, sub)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;

    let pending_proposal = match pending {
        Some(row) => {
            let diff = profiles::diff(&profile.content, &row.content);
            Some(PendingProposalDto {
                version: row.version,
                base_version: row.base_version,
                content: row.content,
                change_summary: row.change_summary,
                diff,
            })
        }
        None => None,
    };

    Ok(ProfileResponse {
        content: profile.content,
        version: profile.version,
        updated_at: profile.updated_at.map(|t| t.to_rfc3339()),
        pending_proposal,
    })
}

/// The caller's current taste profile plus any pending proposal.
#[utoipa::path(
    get,
    path = "/api/v1/profile",
    tag = "profile",
    responses(
        (status = 200, body = ProfileResponse),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 409, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn get_profile(
    State(state): State<Arc<AppState>>,
    scope: CurrentScope,
) -> Result<Json<ProfileResponse>, AppError> {
    let sub = codex_user_sub(&state, &scope).await?;
    Ok(Json(build_profile_response(&state, &sub).await?))
}

/// The caller's current profile as raw Markdown (for copy/download).
#[utoipa::path(
    get,
    path = "/api/v1/profile/raw",
    tag = "profile",
    responses(
        (status = 200, content_type = "text/markdown"),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 409, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn get_profile_raw(
    State(state): State<Arc<AppState>>,
    scope: CurrentScope,
) -> Result<impl IntoResponse, AppError> {
    let sub = codex_user_sub(&state, &scope).await?;
    let profile = profiles::get_or_default(&state.db, &sub)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    Ok((
        [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        profile.content,
    ))
}

/// The caller's version history, newest first, each with its rendered diff.
#[utoipa::path(
    get,
    path = "/api/v1/profile/versions",
    tag = "profile",
    responses(
        (status = 200, body = [VersionDto]),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 409, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn list_versions(
    State(state): State<Arc<AppState>>,
    scope: CurrentScope,
) -> Result<Json<Vec<VersionDto>>, AppError> {
    let sub = codex_user_sub(&state, &scope).await?;
    let rows = profiles::rendered_history(&state.db, &sub)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    let out = rows
        .into_iter()
        .map(|v| VersionDto {
            version: v.version,
            base_version: v.base_version,
            status: v.status,
            change_summary: v.change_summary,
            created_at: v.created_at.to_rfc3339(),
            diff: v.diff,
        })
        .collect();
    Ok(Json(out))
}

/// Directly edit the profile, applying a new version immediately.
#[utoipa::path(
    put,
    path = "/api/v1/profile",
    tag = "profile",
    request_body = EditProfileRequest,
    responses(
        (status = 200, body = ProfileResponse),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 409, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn edit_profile(
    State(state): State<Arc<AppState>>,
    scope: CurrentScope,
    Json(req): Json<EditProfileRequest>,
) -> Result<Json<ProfileResponse>, AppError> {
    let sub = codex_user_sub(&state, &scope).await?;
    let summary = req
        .change_summary
        .unwrap_or_else(|| "Edited directly in the web UI".to_owned());
    profiles::edit_direct(&state.db, &sub, &req.content, &summary, req.base_version)
        .await
        .map_err(map_profile_error)?;
    Ok(Json(build_profile_response(&state, &sub).await?))
}

/// Approve a pending proposal, applying it as the current profile.
#[utoipa::path(
    post,
    path = "/api/v1/profile/proposals/{version}/approve",
    tag = "profile",
    params(("version" = i64, Path, description = "the pending proposal's version")),
    responses(
        (status = 200, body = ProfileResponse),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 404, body = super::error::ErrorBody),
        (status = 409, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn approve_proposal(
    State(state): State<Arc<AppState>>,
    scope: CurrentScope,
    Path(version): Path<i64>,
) -> Result<Json<ProfileResponse>, AppError> {
    let sub = codex_user_sub(&state, &scope).await?;
    profiles::approve(&state.db, &sub, version)
        .await
        .map_err(map_profile_error)?;
    Ok(Json(build_profile_response(&state, &sub).await?))
}

/// Reject a pending proposal; the current profile is unchanged.
#[utoipa::path(
    post,
    path = "/api/v1/profile/proposals/{version}/reject",
    tag = "profile",
    params(("version" = i64, Path, description = "the pending proposal's version")),
    responses(
        (status = 204),
        (status = 401, body = super::error::ErrorBody),
        (status = 403, body = super::error::ErrorBody),
        (status = 404, body = super::error::ErrorBody),
        (status = 409, body = super::error::ErrorBody)
    ),
    security(("bearer" = []))
)]
pub async fn reject_proposal(
    State(state): State<Arc<AppState>>,
    scope: CurrentScope,
    Path(version): Path<i64>,
) -> Result<StatusCode, AppError> {
    let sub = codex_user_sub(&state, &scope).await?;
    profiles::reject(&state.db, &sub, version)
        .await
        .map_err(map_profile_error)?;
    Ok(StatusCode::NO_CONTENT)
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
    use shisho_db::repos::profiles;

    const USER: &str = "user-alice";
    const SUB: &str = "codex-alice";

    async fn app_with_user(
        codex_sub: Option<&str>,
    ) -> (axum::Router, std::sync::Arc<api::AppState>) {
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

    fn session_cookie(user: &str) -> String {
        let keys = SessionKeys::for_test(TEST_SESSION_SECRET);
        format!("shisho_session={}", keys.issue_session(user).unwrap())
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .header(header::COOKIE, session_cookie(USER))
            .body(Body::empty())
            .unwrap()
    }

    fn send_json(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, session_cookie(USER))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn reads_default_then_reflects_a_direct_edit() {
        let (app, _state) = app_with_user(Some(SUB)).await;

        let resp = app.clone().oneshot(get("/api/v1/profile")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["version"], 0);
        assert_eq!(body["content"], "");

        let resp = app
            .clone()
            .oneshot(send_json(
                "PUT",
                "/api/v1/profile",
                serde_json::json!({"content": "# Loves\n- slow-life fantasy"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await["version"], 1);

        let resp = app.oneshot(get("/api/v1/profile")).await.unwrap();
        let body = json_body(resp).await;
        assert_eq!(body["version"], 1);
        assert!(
            body["content"]
                .as_str()
                .unwrap()
                .contains("slow-life fantasy")
        );
    }

    #[tokio::test]
    async fn a_user_without_a_codex_identity_is_409() {
        let (app, _state) = app_with_user(None).await;
        let resp = app.oneshot(get("/api/v1/profile")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn approve_applies_a_pending_proposal() {
        let (app, state) = app_with_user(Some(SUB)).await;
        // A consolidate-style proposal against the empty v0 profile.
        profiles::propose(
            &state.db,
            SUB,
            "# Loves\n- mystery",
            "found a mystery pattern",
            Some(0),
        )
        .await
        .expect("propose");

        // It shows up as pending.
        let resp = app.clone().oneshot(get("/api/v1/profile")).await.unwrap();
        let body = json_body(resp).await;
        assert_eq!(body["version"], 0, "still the current profile");
        assert_eq!(body["pendingProposal"]["version"], 1);

        // Approve it.
        let resp = app
            .clone()
            .oneshot(send_json(
                "POST",
                "/api/v1/profile/proposals/1/approve",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app.oneshot(get("/api/v1/profile")).await.unwrap();
        let body = json_body(resp).await;
        assert_eq!(body["version"], 1);
        assert!(body["content"].as_str().unwrap().contains("mystery"));
        assert!(body.get("pendingProposal").is_none());
    }

    #[tokio::test]
    async fn reject_leaves_the_profile_unchanged() {
        let (app, state) = app_with_user(Some(SUB)).await;
        profiles::propose(&state.db, SUB, "# Loves\n- gore", "noise", Some(0))
            .await
            .expect("propose");

        let resp = app
            .clone()
            .oneshot(send_json(
                "POST",
                "/api/v1/profile/proposals/1/reject",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app.oneshot(get("/api/v1/profile")).await.unwrap();
        let body = json_body(resp).await;
        assert_eq!(body["version"], 0, "rejected proposal never applied");
        assert!(body.get("pendingProposal").is_none());
    }

    #[tokio::test]
    async fn versions_lists_history_with_diffs() {
        let (app, _state) = app_with_user(Some(SUB)).await;
        app.clone()
            .oneshot(send_json(
                "PUT",
                "/api/v1/profile",
                serde_json::json!({"content": "# Loves\n- isekai"}),
            ))
            .await
            .unwrap();

        let resp = app.oneshot(get("/api/v1/profile/versions")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let versions = json_body(resp).await;
        let arr = versions.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["version"], 1);
        assert!(arr[0]["diff"].as_str().unwrap().contains("isekai"));
    }
}
