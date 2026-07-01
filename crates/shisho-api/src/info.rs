//! `GET /api/v1/info`: public instance metadata for the login-less first-run
//! UX. It leaks nothing sensitive — just the display name, version, and auth
//! mode — so it needs no authentication.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;

/// Instance metadata. Field names are part of the public API contract
/// (`auth_mode` is snake_case by convention).
#[derive(Serialize, ToSchema)]
pub struct InfoResponse {
    /// Configured display name of this instance.
    #[schema(example = "Shisho")]
    pub name: String,
    #[schema(example = "0.1.0")]
    pub version: String,
    #[schema(example = "single_user")]
    pub auth_mode: String,
}

/// Instance name, version, and auth mode.
#[utoipa::path(
    get,
    path = "/api/v1/info",
    tag = "shisho",
    responses((status = 200, body = InfoResponse))
)]
pub async fn info(State(state): State<Arc<AppState>>) -> Json<InfoResponse> {
    Json(InfoResponse {
        name: state.cfg.app.name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        auth_mode: state.cfg.auth.mode.as_str().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate as api;

    #[tokio::test]
    async fn info_returns_name_version_auth_mode() {
        let mut cfg = shisho_config::AppConfig::default();
        cfg.app.name = "My Service".to_string();

        let app = api::router(api::test_util::test_state(cfg).await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["name"], "My Service");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["auth_mode"], "single_user");
    }
}
