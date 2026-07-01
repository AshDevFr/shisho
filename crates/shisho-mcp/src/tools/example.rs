//! Scaffold tools demonstrating the MCP tool pattern. Replace these with your
//! own domain tools.
//!
//! - `ping` takes no arguments and confirms the server is reachable. It needs
//!   the `example:read` scope, so a read-only key can call it.
//! - `echo` returns the message it was given. It needs the `example:write`
//!   scope, so a read-only key is refused — this is how per-tool scope gating
//!   shows up to a caller.
//!
//! A tool method's return is either `Ok(CallToolResult)` (success or a
//! recoverable domain error via [`super::fail`]) or `Err(ErrorData)` for a
//! protocol-level error. Parameter structs derive `Deserialize` +
//! `schemars::JsonSchema` so rmcp can advertise their input schema.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::Deserialize;

use super::ok_result;
use crate::McpService;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EchoParams {
    /// The message to echo back.
    pub message: String,
}

#[tool_router(router = example_tool_router, vis = "pub(crate)")]
impl McpService {
    #[tool(
        description = "Liveness check: returns \"pong\" and the server version. Takes no \
arguments. Requires the example:read scope."
    )]
    async fn ping(&self) -> Result<CallToolResult, ErrorData> {
        Ok(ok_result(
            "pong",
            serde_json::json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }),
        ))
    }

    #[tool(
        description = "Echo a message back to the caller, demonstrating a tool that takes \
arguments. Requires the example:write scope."
    )]
    async fn echo(
        &self,
        Parameters(p): Parameters<EchoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if p.message.trim().is_empty() {
            return Err(ErrorData::invalid_params("message must not be empty", None));
        }
        Ok(ok_result(
            p.message.clone(),
            serde_json::json!({ "message": p.message }),
        ))
    }
}
