use axum::Json;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

/// Shared application error. Every fallible handler returns this so all error
/// responses share one JSON shape:
/// `{ "error": { "code": "...", "message": "..." } }`.
///
/// Some variants have no producer yet: they are the stable error contract that
/// handlers return as the API surface grows.
#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The requested resource does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The request is malformed or fails validation.
    #[error("{0}")]
    BadRequest(String),
    /// Missing or invalid credentials.
    #[error("unauthorized")]
    Unauthorized,
    /// Authenticated, but not allowed to do this.
    #[error("forbidden")]
    Forbidden,
    /// The authenticated account has been deactivated. A distinct 403 so the
    /// client can tell "your account is disabled" from "you lack this scope".
    #[error("this account has been deactivated")]
    Deactivated,
    /// The request conflicts with existing state.
    #[error("{0}")]
    Conflict(String),
    /// The request body exceeds the configured size limit.
    #[error("{0}")]
    PayloadTooLarge(String),
    /// The caller exceeded a rate limit. Carries the whole seconds the client
    /// should wait before retrying, surfaced as a `Retry-After` header.
    #[error("rate limit exceeded; retry in {0}s")]
    RateLimited(u64),
    /// A dependency (e.g. the database) is unreachable.
    #[error("{0}")]
    Unavailable(String),
    /// Unexpected failure. The cause is logged, never sent to the client.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sea_orm::DbErr> for AppError {
    fn from(err: sea_orm::DbErr) -> Self {
        Self::Internal(err.into())
    }
}

/// Wire format of every error response.
#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorDetail {
    /// Stable machine-readable error code (clients switch on this).
    #[schema(example = "not_found")]
    pub code: String,
    pub message: String,
}

impl AppError {
    /// Stable machine-readable code for the variant.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::BadRequest(_) => "bad_request",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::Deactivated => "account_deactivated",
            Self::Conflict(_) => "conflict",
            Self::PayloadTooLarge(_) => "payload_too_large",
            Self::RateLimited(_) => "rate_limited",
            Self::Unavailable(_) => "unavailable",
            Self::Internal(_) => "internal",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::Deactivated => StatusCode::FORBIDDEN,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let message = match &self {
            // The cause goes to the log; the client gets a generic message.
            Self::Internal(err) => {
                tracing::error!(error = ?err, "internal error");
                "internal server error".to_string()
            }
            other => other.to_string(),
        };

        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code().to_string(),
                message,
            },
        };
        let mut resp = (self.status(), Json(body)).into_response();
        // Rate-limited responses carry a Retry-After header (whole seconds), the
        // canonical signal clients and proxies honor.
        if let Self::RateLimited(secs) = &self
            && let Ok(value) = secs.to_string().parse()
        {
            resp.headers_mut().insert(header::RETRY_AFTER, value);
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use http_body_util::BodyExt;

    async fn render(err: AppError) -> (StatusCode, serde_json::Value) {
        let resp = err.into_response();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn app_error_maps_variants_to_status_and_code() {
        let cases: Vec<(AppError, StatusCode, &str)> = vec![
            (
                AppError::NotFound("item 0193 not found".into()),
                StatusCode::NOT_FOUND,
                "not_found",
            ),
            (
                AppError::BadRequest("name is required".into()),
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
            (
                AppError::Unauthorized,
                StatusCode::UNAUTHORIZED,
                "unauthorized",
            ),
            (AppError::Forbidden, StatusCode::FORBIDDEN, "forbidden"),
            (
                AppError::Conflict("slug already taken".into()),
                StatusCode::CONFLICT,
                "conflict",
            ),
            (
                AppError::Unavailable("database unreachable".into()),
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
            ),
            (
                AppError::Internal(anyhow!("boom")),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
            ),
        ];

        for (err, want_status, want_code) in cases {
            let (status, body) = render(err).await;
            assert_eq!(status, want_status);
            assert_eq!(body["error"]["code"], want_code);
            assert!(body["error"]["message"].is_string());
        }
    }

    #[tokio::test]
    async fn internal_error_body_hides_details() {
        let (status, body) = render(AppError::Internal(anyhow!("secret db password leaked"))).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let text = body.to_string();
        assert!(
            !text.contains("secret"),
            "internal details must not leak into the body: {text}"
        );
    }

    #[tokio::test]
    async fn rate_limited_sets_retry_after() {
        let resp = AppError::RateLimited(42).into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers().get(header::RETRY_AFTER).unwrap(), "42");
    }
}
