//! MCP authentication: identity resolution per transport plus the axum layer
//! guarding the HTTP-mounted transport.
//!
//! Key verification is [`shisho_api::middleware::authenticate`], the exact code
//! the REST API runs, so a key behaves identically on every surface (config-key
//! fast path, argon2 rows, expiry, `last_used` touch).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use shisho_api::AppState;
use shisho_api::error::AppError;
use shisho_api::middleware::{authenticate, bearer_token};
use shisho_auth::keys::{Scope, allows};
use shisho_config::AuthMode;

/// Environment variable carrying the API key for `shisho mcp stdio`.
pub const STDIO_KEY_ENV: &str = "SHISHO_API_KEY";

/// How [`crate::McpService`] learns who is calling.
#[derive(Clone, Debug)]
pub enum McpIdentity {
    /// Fixed at process start (stdio): the key was verified before serving, or
    /// `mcp.stdio_no_auth = true` opted into local trust.
    Local(Vec<Scope>),
    /// HTTP transport: [`require_mcp_auth`] authenticates every request and
    /// stores the verified [`shisho_api::middleware::AuthContext`] in the request
    /// extensions, where the service reads it back per tool call.
    PerRequest,
}

/// Whether the held scopes include `required`. Actions are independent (no
/// hierarchy): this is plain set membership.
pub fn scopes_allow(scopes: &[Scope], required: Scope) -> bool {
    allows(scopes, required.resource, required.action)
}

/// Resolve the identity for `shisho mcp stdio` before serving.
///
/// Order: `mcp.stdio_no_auth = true` grants local-trust superuser (the full
/// scope set); otherwise the key comes from `SHISHO_API_KEY`, falling back to
/// `auth.api_key` from the config (the process already read the config file, so
/// the key in it is within the same trust boundary). No key, or an invalid one,
/// is a startup error: failing loudly here beats every tool call failing later.
pub async fn resolve_stdio_scopes(state: &AppState) -> anyhow::Result<Vec<Scope>> {
    let env_key = std::env::var(STDIO_KEY_ENV)
        .ok()
        .filter(|k| !k.trim().is_empty());
    resolve_stdio_scopes_with_key(state, env_key).await
}

/// [`resolve_stdio_scopes`] with the environment read factored out, so tests
/// stay deterministic under parallel execution (process env is global).
async fn resolve_stdio_scopes_with_key(
    state: &AppState,
    env_key: Option<String>,
) -> anyhow::Result<Vec<Scope>> {
    if state.cfg.mcp.stdio_no_auth {
        return Ok(Scope::all());
    }
    let key = env_key.or_else(|| state.cfg.auth.api_key.clone());
    let Some(key) = key else {
        anyhow::bail!(
            "no API key for the MCP stdio transport: set the {STDIO_KEY_ENV} environment \
             variable (or auth.api_key in the config), or opt into local trust with \
             mcp.stdio_no_auth = true"
        );
    };
    match authenticate(state, &key).await {
        Ok(ctx) => Ok(ctx.scopes),
        Err(AppError::Unauthorized) => anyhow::bail!(
            "the API key provided via {STDIO_KEY_ENV} (or auth.api_key) is not valid; \
             create one at /api/v1/keys, or opt into local trust with mcp.stdio_no_auth = true"
        ),
        Err(other) => Err(anyhow::anyhow!(other).context("verifying the MCP stdio API key")),
    }
}

/// Bearer auth for the HTTP-mounted MCP transport. Unlike the REST middleware
/// there is no method-implied scope check here: MCP is a single POST endpoint,
/// so scopes are enforced per tool inside [`crate::McpService`].
///
/// A 401 carries a `WWW-Authenticate: Bearer resource_metadata="…"` header in
/// multi-user mode, the discovery hook an OAuth connector follows to find this
/// server's authorization server. Single-user instances omit it: there is no
/// OAuth there, only API keys.
pub async fn require_mcp_auth(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(token) = bearer_token(req.headers()) else {
        return unauthorized(&state);
    };
    let ctx = match authenticate(&state, token).await {
        Ok(ctx) => ctx,
        Err(AppError::Unauthorized) => return unauthorized(&state),
        Err(other) => return other.into_response(),
    };
    req.extensions_mut().insert(ctx);
    next.run(req).await
}

/// A 401 for the MCP transport, with the OAuth resource-metadata pointer in
/// multi-user mode.
fn unauthorized(state: &AppState) -> Response {
    let mut resp = AppError::Unauthorized.into_response();
    if state.cfg.auth.mode == AuthMode::MultiUser {
        let value = format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
            state.cfg.server.public_base()
        );
        if let Ok(header_value) = HeaderValue::from_str(&value) {
            resp.headers_mut()
                .insert(header::WWW_AUTHENTICATE, header_value);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use shisho_api::test_util::{seed_key, test_state};

    #[tokio::test]
    async fn stdio_no_auth_grants_admin_without_key() {
        let mut cfg = shisho_config::AppConfig::default();
        cfg.mcp.stdio_no_auth = true;
        let state = test_state(cfg).await;

        let scopes = resolve_stdio_scopes_with_key(&state, None).await.unwrap();
        assert_eq!(scopes, Scope::all());
    }

    #[tokio::test]
    async fn stdio_refuses_without_key_by_default() {
        let state = test_state(Default::default()).await;

        let err = resolve_stdio_scopes_with_key(&state, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(STDIO_KEY_ENV), "{err}");
        assert!(err.contains("stdio_no_auth"), "{err}");
    }

    #[tokio::test]
    async fn stdio_env_key_resolves_db_scopes() {
        let state = test_state(Default::default()).await;
        let (key, _) = seed_key(&state.db, "ro", &[Scope::EXAMPLE_READ], None).await;

        let scopes = resolve_stdio_scopes_with_key(&state, Some(key))
            .await
            .unwrap();
        assert_eq!(scopes, vec![Scope::EXAMPLE_READ]);
    }

    #[tokio::test]
    async fn stdio_falls_back_to_config_key() {
        let mut cfg = shisho_config::AppConfig::default();
        cfg.auth.api_key = Some("shisho_config_secret".into());
        let state = test_state(cfg).await;

        let scopes = resolve_stdio_scopes_with_key(&state, None).await.unwrap();
        assert_eq!(
            scopes,
            Scope::all(),
            "config key carries the full scope set"
        );
    }

    #[tokio::test]
    async fn stdio_invalid_key_is_a_startup_error() {
        let state = test_state(Default::default()).await;

        let err = resolve_stdio_scopes_with_key(&state, Some("shisho_bogus".into()))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not valid"), "{err}");
    }

    #[test]
    fn scopes_allow_is_set_membership() {
        assert!(scopes_allow(&Scope::all(), Scope::EXAMPLE_WRITE));
        assert!(scopes_allow(
            &[Scope::EXAMPLE_READ, Scope::EXAMPLE_WRITE],
            Scope::EXAMPLE_READ
        ));
        // No hierarchy: write does not imply read.
        assert!(!scopes_allow(&[Scope::EXAMPLE_WRITE], Scope::EXAMPLE_READ));
        assert!(!scopes_allow(&[Scope::EXAMPLE_READ], Scope::EXAMPLE_WRITE));
        assert!(!scopes_allow(&[], Scope::EXAMPLE_READ));
    }
}
