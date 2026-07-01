use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::error::{AppError, ErrorBody};

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub status: &'static str,
}

/// Liveness check. Pings the database and reports `ok`.
#[utoipa::path(
    get,
    path = "/api/v1/health",
    tag = "shisho",
    responses(
        (status = 200, body = Health),
        (status = 503, body = ErrorBody, description = "Database unreachable")
    )
)]
pub async fn health(State(state): State<Arc<AppState>>) -> Result<Json<Health>, AppError> {
    state
        .db
        .ping()
        .await
        .map_err(|e| AppError::Unavailable(format!("database unreachable: {e}")))?;
    Ok(Json(Health { status: "ok" }))
}
