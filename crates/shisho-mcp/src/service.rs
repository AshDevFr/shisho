//! The rmcp server handler: one tool registry behind every transport.
//!
//! `McpService` holds the same [`AppState`] as the REST layer and registers the
//! domain tools (defined in [`super::tools`]). The handler is written out by
//! hand instead of `#[tool_handler]` because `call_tool` enforces the per-tool
//! scope before dispatching and injects the caller's [`AuthContext`] into the
//! call so per-user tools can resolve a Codex [`UserCtx`] uniformly across the
//! HTTP and stdio transports.

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
use shisho_services::Services;
use shisho_services::UserCtx;
use shisho_services::codex_identity::IdentityError;

/// Instructions shown to the client model on initialize: this is prompt
/// surface, kept short and operational.
const INSTRUCTIONS: &str = "Shisho is a library-aware manga recommendation service over the \
caller's own Codex library. Use get_candidates for recommendation pools, lookup/search_library \
for series details, list_libraries to scope a session, record_verdict to log every \
recommendation decision, record_feedback for reading outcomes, get_profile/propose_profile_update \
for the taste profile, and status to qualify data freshness. The recommend and consolidate \
prompts encode the full workflows.";

#[derive(Clone)]
pub struct McpService {
    pub(crate) state: Arc<AppState>,
    identity: McpIdentity,
    tool_router: ToolRouter<McpService>,
}

impl McpService {
    pub fn new(state: Arc<AppState>, identity: McpIdentity) -> Self {
        Self {
            state,
            identity,
            tool_router: Self::candidates_router()
                + Self::lookup_router()
                + Self::library_router()
                + Self::verdicts_router()
                + Self::profile_router()
                + Self::status_router(),
        }
    }

    #[cfg(test)]
    pub(crate) fn tools(&self) -> Vec<rmcp::model::Tool> {
        self.tool_router.list_all()
    }

    /// The caller's verified identity: fixed at startup for stdio, read back
    /// from the auth middleware's request extension for HTTP. A missing
    /// extension on the HTTP path means the route was mounted without the auth
    /// layer; deny rather than assume.
    fn auth_context(&self, context: &RequestContext<RoleServer>) -> Result<AuthContext, ErrorData> {
        match &self.identity {
            McpIdentity::Local(ctx) => Ok(ctx.clone()),
            McpIdentity::PerRequest => context
                .extensions
                .get::<axum::http::request::Parts>()
                .and_then(|parts| parts.extensions.get::<AuthContext>())
                .cloned()
                .ok_or_else(|| ErrorData::invalid_request("request is not authenticated", None)),
        }
    }

    /// The domain service registry, or a typed error when the domain surface is
    /// not configured (no `codex.base_url`).
    pub(crate) fn services(&self) -> Result<&Services, ErrorData> {
        self.state.services.as_ref().ok_or_else(|| {
            ErrorData::invalid_request(
                "the recommendation service is not configured on this instance (set codex.base_url)",
                None,
            )
        })
    }

    /// Resolve the caller's per-user Codex [`UserCtx`] from their verified
    /// identity: needs a user-owned key (`user_id`), at-rest encryption (the
    /// `dek`), and a configured Codex identity resolver. Each absence is a
    /// distinct, actionable error; a revoked stored key surfaces the re-auth
    /// message (PRD 3.4) — mirrors the REST `CodexUserCtx` extractor.
    pub(crate) async fn user_ctx(&self, auth: &AuthContext) -> Result<UserCtx, ErrorData> {
        let Some(user_id) = auth.user_id.as_deref() else {
            return Err(ErrorData::invalid_request(
                "this credential is not tied to a Shisho user, so it has no Codex identity; use a \
                 user-owned API key (or sign in) to run per-user tools",
                None,
            ));
        };
        let dek = self.state.dek.as_ref().ok_or_else(|| {
            ErrorData::invalid_request(
                "per-user Codex access requires at-rest encryption to be configured",
                None,
            )
        })?;
        let identity = self.state.codex_identity.as_ref().ok_or_else(|| {
            ErrorData::invalid_request("Codex is not configured on this instance", None)
        })?;
        identity
            .resolve(&self.state.db, dek, user_id)
            .await
            .map_err(map_identity_error)
    }
}

/// Map a Codex identity-resolution failure to a tool-facing protocol error.
/// A revoked/expired stored key and a missing credential are the caller's to
/// fix (actionable messages); everything else is an internal failure.
fn map_identity_error(err: IdentityError) -> ErrorData {
    match &err {
        IdentityError::CredentialsExpired => {
            ErrorData::invalid_request(err.reauth_message().unwrap_or_default().to_string(), None)
        }
        IdentityError::NoCredential => ErrorData::invalid_request(err.to_string(), None),
        _ => {
            ErrorData::internal_error(format!("resolving your Codex identity failed: {err}"), None)
        }
    }
}

/// The granular scope each tool requires. Default-deny: a tool absent from this
/// map cannot be called at all, so adding a tool forces an explicit
/// `(resource, action)` decision (the scope-matrix test enforces it).
pub(crate) fn required_scope(tool: &str) -> Option<Scope> {
    match tool {
        "get_candidates" => Some(Scope::CANDIDATES_READ),
        "lookup" => Some(Scope::LOOKUP_READ),
        "list_libraries" | "search_library" => Some(Scope::LIBRARY_READ),
        "record_verdict" | "record_feedback" => Some(Scope::VERDICTS_WRITE),
        "get_profile" => Some(Scope::PROFILE_READ),
        "propose_profile_update" => Some(Scope::PROFILE_WRITE),
        "status" => Some(Scope::STATUS_READ),
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
        mut context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let auth = self.auth_context(&context)?;
        let required = required_scope(&request.name).ok_or_else(|| {
            ErrorData::invalid_params(format!("unknown tool {:?}", request.name), None)
        })?;
        if !scopes_allow(&auth.scopes, required) {
            return Err(ErrorData::invalid_request(
                format!(
                    "this API key lacks the {required} scope required by the {} tool",
                    request.name
                ),
                None,
            ));
        }
        // Hand the verified identity to the tool so per-user tools resolve their
        // UserCtx the same way on stdio and HTTP.
        context.extensions.insert(auth);
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

    const ALL_TOOLS: [&str; 9] = [
        "get_candidates",
        "get_profile",
        "list_libraries",
        "lookup",
        "propose_profile_update",
        "record_feedback",
        "record_verdict",
        "search_library",
        "status",
    ];

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
        let candidates = listed
            .tools
            .iter()
            .find(|t| t.name == "get_candidates")
            .unwrap();
        let schema = serde_json::to_value(candidates.input_schema.as_ref()).unwrap();
        assert!(
            schema["properties"]["pool"].is_object(),
            "get_candidates must declare its pool parameter: {schema}"
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
    async fn a_wrong_scope_is_denied_before_dispatch() {
        // A key scoped only to candidates:read.
        let state = test_state(Default::default()).await;
        let client = client_with_state(state, local(vec![Scope::CANDIDATES_READ])).await;

        // A write tool it lacks the scope for is rejected with an actionable
        // message, before any dispatch.
        let err = client
            .call_tool(CallToolRequestParams::new("record_verdict"))
            .await
            .unwrap_err();
        match err {
            ServiceError::McpError(data) => {
                assert!(data.message.contains("write"), "{}", data.message);
                assert!(data.message.contains("record_verdict"), "{}", data.message);
            }
            other => panic!("expected an MCP error, got {other:?}"),
        }

        // A tool it IS scoped for passes the gate and reaches identity
        // resolution (which then fails here: the test key is tied to no user).
        // The point is that the failure is not a scope denial.
        let err = client
            .call_tool(
                CallToolRequestParams::new("get_candidates").with_arguments(
                    serde_json::json!({"pool": "library"})
                        .as_object()
                        .cloned()
                        .unwrap(),
                ),
            )
            .await
            .unwrap_err();
        match err {
            ServiceError::McpError(data) => {
                assert!(
                    !data.message.contains("lacks the"),
                    "the scope gate should have passed: {}",
                    data.message
                );
                assert!(
                    data.message.contains("Shisho user"),
                    "reached identity resolution: {}",
                    data.message
                );
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
}
