//! Tool implementations and the shared result/error conventions every tool
//! follows.
//!
//! Conventions:
//! - Every success carries both human-readable text and the full result as
//!   `structured_content`, so non-LLM clients can parse without scraping text.
//! - Malformed input is a JSON-RPC `invalid_params` error. Domain outcomes the
//!   model should see and may recover from are tool results with `is_error`.
//!   Unexpected failures are logged and surfaced as an opaque internal error,
//!   mirroring the REST 500 contract.
//!
//! [`example`] holds the scaffold tools (`ping`, `echo`). Replace them with your
//! own domain tools, and register each tool's required scope in
//! [`crate::service::required_scope`].

pub mod example;

use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock};

/// A successful result: readable text plus the same data as structured JSON.
pub(crate) fn ok_result(text: impl Into<String>, structured: serde_json::Value) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(text.into())]);
    result.structured_content = Some(structured);
    result
}

/// A domain failure the model should see and may recover from.
#[allow(dead_code)]
pub(crate) fn fail(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

/// An unexpected failure: log the cause, hide it from the client.
#[allow(dead_code)]
pub(crate) fn internal(err: anyhow::Error) -> ErrorData {
    tracing::error!(error = ?err, "mcp tool internal error");
    ErrorData::internal_error("internal server error", None)
}
