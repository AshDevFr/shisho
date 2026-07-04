//! Tool implementations: thin adapters from MCP params to the domain services
//! and repositories, and the shared result/error conventions every tool
//! follows.
//!
//! Conventions:
//! - Every success carries both human-readable text and the full result as
//!   `structured_content`, so non-LLM clients can parse without scraping text.
//! - A domain outcome the model should see and may recover from is a tool result
//!   with `is_error` and precise text. Only harness bugs (a missing user context,
//!   a failed DB write) are protocol errors.
//!
//! Each tool declares its required scope in [`crate::service::required_scope`].

pub mod candidates;
pub mod library;
pub mod lookup;
pub mod profile;
pub mod status;
pub mod verdicts;

use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;

use shisho_cache::SwrError;
use shisho_codex::CodexError;

/// The distinct, user-facing message for a Codex 401 on the caller's own
/// credential (PRD: never a silent failure).
pub(crate) const CODEX_KEY_REVOKED: &str =
    "your Codex authorization expired or was revoked; re-authenticate at the Shisho UI";

/// A successful tool result carrying one JSON DTO as both readable text and
/// structured content.
pub(crate) fn tool_json<S: Serialize>(dto: &S) -> Result<CallToolResult, ErrorData> {
    let value = serde_json::to_value(dto)
        .map_err(|e| ErrorData::internal_error(format!("serializing tool result: {e}"), None))?;
    let text = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_owned());
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(value);
    Ok(result)
}

/// A tool-level error: visible to the model as content, not a transport failure.
pub(crate) fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

/// Did this SWR build fail because Codex rejected the caller's credential?
pub(crate) fn swr_codex_unauthorized(err: &SwrError<CodexError>) -> bool {
    matches!(err, SwrError::Load(CodexError::Unauthorized { .. }))
}
