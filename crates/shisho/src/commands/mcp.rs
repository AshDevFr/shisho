//! `shisho mcp stdio`: serve the MCP protocol over stdin/stdout for a local
//! client (Claude Code/Desktop). Logs go to stderr so they don't corrupt the
//! JSON-RPC stream on stdout.

use std::path::PathBuf;

use anyhow::Context;
use rmcp::ServiceExt;
use shisho_mcp::McpService;
use shisho_mcp::auth::{McpIdentity, resolve_stdio_scopes};

pub async fn stdio(config_path: PathBuf) -> anyhow::Result<()> {
    let cfg = shisho_config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    // stdout is the protocol stream; all logging must go to stderr.
    super::init_tracing_stderr(&cfg);
    cfg.validate()?;

    let state = super::build_app_state(&cfg).await?;
    // Verify the key (or local-trust opt-in) before serving, so a missing/invalid
    // credential is a clear startup error rather than a failure on every call.
    let scopes = resolve_stdio_scopes(&state).await?;

    tracing::info!("serving MCP over stdio");
    let service = McpService::new(state, McpIdentity::Local(scopes));
    let running = service
        .serve(rmcp::transport::stdio())
        .await
        .context("starting the MCP stdio transport")?;
    running.waiting().await?;
    Ok(())
}
