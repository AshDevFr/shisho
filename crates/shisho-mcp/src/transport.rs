//! HTTP-mounted MCP transport: Streamable HTTP at `/mcp`.
//!
//! The MCP spec's earlier HTTP+SSE transport was deprecated in favor of
//! Streamable HTTP, and rmcp no longer ships a server for it, so this is the
//! only HTTP transport. `mcp.enabled` gates the mount.
//!
//! The transport runs stateless: each POST is a self-contained MCP exchange with
//! no session affinity, which suits programmatic clients and restarts cleanly.
//! Responses still stream as SSE frames per the Streamable HTTP spec; rmcp
//! handles the framing.

use std::sync::Arc;

use axum::{Router, middleware};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use shisho_api::AppState;
use tokio_util::sync::CancellationToken;

use super::McpService;
use super::auth::{McpIdentity, require_mcp_auth};

/// Mount path of the streamable HTTP transport.
pub const MCP_PATH: &str = "/mcp";

/// Mount the MCP transport on `app` when `mcp.enabled` is on; a no-op (the path
/// 404s) when off. Cancelling `ct` stops in-flight MCP work on shutdown. Kept
/// outside `/api/v1` and its REST middleware: MCP carries its own auth layer
/// with per-tool (not per-method) scope enforcement.
pub fn attach(app: Router, state: Arc<AppState>, ct: CancellationToken) -> Router {
    if !state.cfg.mcp.enabled {
        return app;
    }
    let service_state = state.clone();
    let service = StreamableHttpService::new(
        move || {
            Ok(McpService::new(
                service_state.clone(),
                McpIdentity::PerRequest,
            ))
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_cancellation_token(ct)
            // rmcp's default Host allowlist is loopback-only (DNS-rebinding
            // protection for unauthenticated local servers). This server is
            // meant to be reachable through a reverse proxy / tunnel and every
            // request must already carry a Bearer key, so Host validation is
            // disabled here.
            .disable_allowed_hosts(),
    );
    let mcp = Router::new()
        .nest_service(MCP_PATH, service)
        .layer(middleware::from_fn_with_state(state, require_mcp_auth));
    app.merge(mcp)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::service::{RoleClient, RunningService, ServiceError};
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use tower::ServiceExt as _;

    use super::*;
    use shisho_api as api;
    use shisho_api::test_util::{seed_key, test_state};
    use shisho_auth::keys::Scope;

    const ADMIN_KEY: &str = "shisho_test_admin_config_key";

    async fn state_with(tweak: impl FnOnce(&mut shisho_config::AppConfig)) -> Arc<AppState> {
        let mut cfg = shisho_config::AppConfig::default();
        cfg.auth.api_key = Some(ADMIN_KEY.into());
        tweak(&mut cfg);
        test_state(cfg).await
    }

    fn app_for(state: Arc<AppState>) -> Router {
        attach(api::router(state.clone()), state, CancellationToken::new())
    }

    /// Bind the full app on an ephemeral port and return its base URL.
    async fn spawn_app(state: Arc<AppState>) -> String {
        let app = app_for(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// Connect an rmcp client to a served app over streamable HTTP.
    async fn http_client(base: &str, key: &str) -> RunningService<RoleClient, ()> {
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(format!("{base}{MCP_PATH}"))
                .auth_header(key),
        );
        ().serve(transport).await.expect("client initialize")
    }

    #[tokio::test]
    async fn http_initialize_and_list_tools() {
        let state = state_with(|_| {}).await;
        let base = spawn_app(state).await;
        let client = http_client(&base, ADMIN_KEY).await;

        let info = client.peer_info().expect("server info");
        assert_eq!(
            info.server_info.as_ref().expect("server_info").name,
            "shisho"
        );

        let tools = client.list_tools(Default::default()).await.unwrap();
        assert_eq!(tools.tools.len(), 10);
        client.cancel().await.unwrap();
    }

    /// One registry, every door: a tool call over HTTP hits the same state as
    /// the rest of the app. The admin config key carries every scope but no
    /// user, so `status` passes the scope gate and dispatches into the handler,
    /// which then resolves identity against the shared state (and reports it has
    /// no Codex user) — proof the call reached the real handler over HTTP.
    #[tokio::test]
    async fn http_tool_call_dispatches_into_shared_state() {
        let state = state_with(|_| {}).await;
        let base = spawn_app(state.clone()).await;
        let client = http_client(&base, ADMIN_KEY).await;

        let err = client
            .call_tool(CallToolRequestParams::new("status"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Shisho user"),
            "dispatched into the handler's identity resolution: {err}"
        );
        client.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn mcp_disabled_serves_nothing_mcp_related() {
        let state = state_with(|cfg| cfg.mcp.enabled = false).await;
        let app = app_for(state);

        // GET falls through to the SPA fallback's 404; POST gets the fallback's
        // 405 (it only accepts GET). Either way the path is not an MCP endpoint.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(MCP_PATH)
                    .header(header::AUTHORIZATION, format!("Bearer {ADMIN_KEY}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(MCP_PATH)
                    .header(header::AUTHORIZATION, format!("Bearer {ADMIN_KEY}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn http_without_key_is_401_before_mcp_handling() {
        let state = state_with(|_| {}).await;
        let app = app_for(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(MCP_PATH)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn http_with_unknown_key_is_401() {
        let state = state_with(|_| {}).await;
        let app = app_for(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(MCP_PATH)
                    .header(header::AUTHORIZATION, "Bearer shisho_bogus")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// MCP auth reuses the REST `authenticate`, so key usage must update
    /// `last_used` exactly like a REST call.
    #[tokio::test]
    async fn mcp_key_use_touches_last_used() {
        use sea_orm::EntityTrait;

        let state = state_with(|_| {}).await;
        let (key, key_id) = seed_key(&state.db, "tracked", &[Scope::VERDICTS_READ], None).await;
        let base = spawn_app(state.clone()).await;
        let client = http_client(&base, &key).await;
        client.list_tools(Default::default()).await.unwrap();
        client.cancel().await.unwrap();

        // The touch is fire-and-forget; poll briefly for it to land.
        for _ in 0..100 {
            let row = shisho_db::entities::api_keys::Entity::find_by_id(&key_id)
                .one(&state.db)
                .await
                .unwrap()
                .unwrap();
            if row.last_used.is_some() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("last_used was never updated by MCP auth");
    }

    #[tokio::test]
    async fn read_scope_key_lists_tools_but_cannot_write() {
        let state = state_with(|_| {}).await;
        let (key, _) = seed_key(&state.db, "ro", &[Scope::CANDIDATES_READ], None).await;
        let base = spawn_app(state).await;
        let client = http_client(&base, &key).await;

        // Listing tools is not scope-gated: a read key sees the whole registry.
        let tools = client.list_tools(Default::default()).await.unwrap();
        assert_eq!(tools.tools.len(), 10);

        // A write tool it lacks the scope for is refused per tool, before dispatch.
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
        client.cancel().await.unwrap();
    }

    // --- OAuth connector auth (multi-user) ---

    async fn multi_user_state() -> Arc<AppState> {
        let mut cfg = shisho_config::AppConfig::default();
        cfg.auth.mode = shisho_config::AuthMode::MultiUser;
        cfg.server.public_url = Some("https://shisho.test".into());
        test_state(cfg).await
    }

    /// Seed a live OAuth access token for `user`, returning the plaintext token.
    async fn seed_oauth_token(state: &AppState, user: &str) -> String {
        let access = shisho_oauth::secret::new_access_token();
        let refresh = shisho_oauth::secret::new_refresh_token();
        let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        shisho_oauth::tokens::issue(
            &state.db,
            shisho_oauth::tokens::NewTokens {
                access_hash: &shisho_oauth::secret::hash(&access),
                refresh_hash: &shisho_oauth::secret::hash(&refresh),
                client_id: "claude",
                user_id: user,
                scopes: &shisho_auth::keys::scopes_to_json(&shisho_oauth::granted_scopes()),
                access_expires_at: &exp,
                refresh_expires_at: &exp,
            },
        )
        .await
        .unwrap();
        access
    }

    /// An OAuth access token (what an MCP connector ends up holding)
    /// authenticates `/mcp` through the shared `authenticate` path.
    #[tokio::test]
    async fn oauth_token_authenticates_mcp_http() {
        let state = multi_user_state().await;
        let token = seed_oauth_token(&state, "user-1").await;
        let base = spawn_app(state).await;
        let client = http_client(&base, &token).await;

        let tools = client.list_tools(Default::default()).await.unwrap();
        assert_eq!(tools.tools.len(), 10);
        client.cancel().await.unwrap();
    }

    /// In multi-user mode the 401 advertises the OAuth resource metadata, the
    /// discovery hook an OAuth connector follows.
    #[tokio::test]
    async fn mcp_401_advertises_resource_metadata_in_multi_user() {
        let app = app_for(multi_user_state().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(MCP_PATH)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .expect("WWW-Authenticate present")
            .to_str()
            .unwrap();
        assert!(
            www.contains("https://shisho.test/.well-known/oauth-protected-resource"),
            "points at the protected-resource metadata: {www}"
        );
    }

    /// Single-user mode keeps API-key auth only: no OAuth discovery hint.
    #[tokio::test]
    async fn mcp_401_has_no_resource_metadata_in_single_user() {
        let app = app_for(state_with(|_| {}).await);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(MCP_PATH)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(
            resp.headers().get(header::WWW_AUTHENTICATE).is_none(),
            "single-user 401 carries no OAuth hint"
        );
    }
}
