//! The rmcp server handler: one tool registry behind every transport.
//!
//! `McpService` holds the same [`AppState`] as the REST layer and registers the
//! example tools (defined in [`super::tools`]). The handler is written out by
//! hand instead of `#[tool_handler]` because `call_tool` enforces the per-tool
//! scope before dispatching.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Implementation, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};

use super::auth::{McpIdentity, scopes_allow};
use shisho_api::AppState;
use shisho_api::middleware::AuthContext;
use shisho_auth::keys::Scope;

/// Instructions shown to the client model on initialize: this is prompt
/// surface, so keep it short and operational. Rewrite it for your server.
const INSTRUCTIONS: &str = "This is an example MCP server scaffold. The `ping` tool checks \
that the server is reachable; the `echo` tool returns the message you send it. Replace these \
with tools for your own domain.";

#[derive(Clone)]
pub struct McpService {
    #[allow(dead_code)]
    pub(crate) state: Arc<AppState>,
    identity: McpIdentity,
    tool_router: ToolRouter<McpService>,
}

impl McpService {
    pub fn new(state: Arc<AppState>, identity: McpIdentity) -> Self {
        Self {
            state,
            identity,
            tool_router: Self::example_tool_router(),
        }
    }

    #[cfg(test)]
    pub(crate) fn tools(&self) -> Vec<rmcp::model::Tool> {
        self.tool_router.list_all()
    }

    /// The caller's scopes: fixed at startup for stdio, read back from the auth
    /// middleware's request extension for HTTP. A missing extension on the HTTP
    /// path means the route was mounted without the auth layer; deny rather than
    /// assume.
    fn scopes_for(&self, context: &RequestContext<RoleServer>) -> Result<Vec<Scope>, ErrorData> {
        match &self.identity {
            McpIdentity::Local(ctx) => Ok(ctx.scopes.clone()),
            McpIdentity::PerRequest => context
                .extensions
                .get::<axum::http::request::Parts>()
                .and_then(|parts| parts.extensions.get::<AuthContext>())
                .map(|ctx| ctx.scopes.clone())
                .ok_or_else(|| ErrorData::invalid_request("request is not authenticated", None)),
        }
    }
}

/// The granular scope each tool requires. Default-deny: a tool absent from this
/// map cannot be called at all, so adding a tool forces an explicit
/// `(resource, action)` decision (the scope-matrix test enforces it).
pub(crate) fn required_scope(tool: &str) -> Option<Scope> {
    match tool {
        "ping" => Some(Scope::EXAMPLE_READ),
        "echo" => Some(Scope::EXAMPLE_WRITE),
        _ => None,
    }
}

impl ServerHandler for McpService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("shisho", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: self.tool_router.list_all(),
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let scopes = self.scopes_for(&context)?;
        let required = required_scope(&request.name).ok_or_else(|| {
            ErrorData::invalid_params(format!("unknown tool {:?}", request.name), None)
        })?;
        if !scopes_allow(&scopes, required) {
            return Err(ErrorData::invalid_request(
                format!(
                    "this API key lacks the {required} scope required by the {} tool",
                    request.name
                ),
                None,
            ));
        }
        let tcc = ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }
}

#[cfg(test)]
mod tests {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::service::{RoleClient, RunningService, ServiceError};

    use super::*;
    use shisho_api::test_util::test_state;

    /// Serve `McpService` on an in-process duplex pipe and hand back a connected
    /// rmcp client. Exercises the same JSON-RPC plumbing as the stdio transport,
    /// without a child process.
    async fn client_with_state(
        state: Arc<AppState>,
        identity: McpIdentity,
    ) -> RunningService<RoleClient, ()> {
        let (client_io, server_io) = tokio::io::duplex(1 << 16);
        let service = McpService::new(state, identity);
        tokio::spawn(async move {
            if let Ok(running) = service.serve(server_io).await {
                let _ = running.waiting().await;
            }
        });
        ().serve(client_io).await.expect("client initialize")
    }

    /// A `Local` identity carrying the given scopes and no user (the shape the
    /// stdio superuser and scoped-key paths produce).
    fn local(scopes: Vec<Scope>) -> McpIdentity {
        McpIdentity::Local(shisho_api::middleware::AuthContext {
            key_id: None,
            user_id: None,
            scopes,
        })
    }

    async fn admin_client() -> RunningService<RoleClient, ()> {
        let state = test_state(Default::default()).await;
        client_with_state(state, local(Scope::all())).await
    }

    const ALL_TOOLS: [&str; 2] = ["echo", "ping"];

    #[tokio::test]
    async fn initialize_reports_binary_metadata() {
        let client = admin_client().await;
        let info = client.peer_info().expect("server info after initialize");
        assert_eq!(info.server_info.name, "shisho");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.capabilities.tools.is_some(), "tools capability");
        assert!(
            !info.instructions.as_deref().unwrap_or("").is_empty(),
            "instructions are the prompt surface; they must exist"
        );
        client.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn tools_list_returns_all_with_schemas() {
        let client = admin_client().await;
        let listed = client.list_tools(Default::default()).await.unwrap();

        let mut names: Vec<&str> = listed.tools.iter().map(|t| t.name.as_ref()).collect();
        names.sort_unstable();
        assert_eq!(names, ALL_TOOLS);

        for tool in &listed.tools {
            assert!(
                tool.description.as_deref().is_some_and(|d| !d.is_empty()),
                "{} needs a description",
                tool.name
            );
            let schema = serde_json::to_value(tool.input_schema.as_ref()).unwrap();
            assert_eq!(schema["type"], "object", "{} schema", tool.name);
        }
        let echo = listed.tools.iter().find(|t| t.name == "echo").unwrap();
        let schema = serde_json::to_value(echo.input_schema.as_ref()).unwrap();
        assert!(
            schema["properties"]["message"].is_object(),
            "echo must declare its message parameter: {schema}"
        );
        client.cancel().await.unwrap();
    }

    /// Default-deny guard: every registered tool must declare a scope.
    #[tokio::test]
    async fn every_registered_tool_declares_a_scope() {
        let state = test_state(Default::default()).await;
        let service = McpService::new(state, local(Scope::all()));
        for tool in service.tools() {
            assert!(
                required_scope(&tool.name).is_some(),
                "tool {} is registered but missing from required_scope",
                tool.name
            );
        }
    }

    #[tokio::test]
    async fn read_scope_cannot_call_write_tools() {
        let state = test_state(Default::default()).await;
        let client = client_with_state(state, local(vec![Scope::EXAMPLE_READ])).await;

        // The read tool works.
        let result = client
            .call_tool(CallToolRequestParams::new("ping"))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));

        // The write tool is rejected with an actionable message.
        let err = client
            .call_tool(
                CallToolRequestParams::new("echo").with_arguments(
                    serde_json::json!({"message": "hi"})
                        .as_object()
                        .cloned()
                        .unwrap(),
                ),
            )
            .await
            .unwrap_err();
        match err {
            ServiceError::McpError(data) => {
                assert!(data.message.contains("write"), "{}", data.message);
                assert!(data.message.contains("echo"), "{}", data.message);
            }
            other => panic!("expected an MCP error, got {other:?}"),
        }
        client.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn unknown_tool_is_an_invalid_params_error() {
        let client = admin_client().await;
        let err = client
            .call_tool(CallToolRequestParams::new("nonexistent_tool"))
            .await
            .unwrap_err();
        match err {
            ServiceError::McpError(data) => {
                assert!(data.message.contains("unknown tool"), "{}", data.message)
            }
            other => panic!("expected an MCP error, got {other:?}"),
        }
        client.cancel().await.unwrap();
    }

    /// End-to-end through the protocol: echo round-trips the message.
    #[tokio::test]
    async fn echo_roundtrip_over_transport() {
        let client = admin_client().await;
        let result = client
            .call_tool(
                CallToolRequestParams::new("echo").with_arguments(
                    serde_json::json!({"message": "hello mcp"})
                        .as_object()
                        .cloned()
                        .unwrap(),
                ),
            )
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        let structured = result.structured_content.expect("structured content");
        assert_eq!(structured["message"], "hello mcp");
        client.cancel().await.unwrap();
    }
}
