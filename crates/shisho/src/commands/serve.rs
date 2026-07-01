use std::path::PathBuf;

use anyhow::Context;
use shisho_api::router;
use tokio_util::sync::CancellationToken;

pub async fn run(config_path: PathBuf) -> anyhow::Result<()> {
    let cfg = shisho_config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    super::init_tracing(&cfg);
    cfg.validate()?;

    let state = super::build_app_state(&cfg).await?;

    // Mount the MCP Streamable HTTP transport at /mcp when mcp.enabled is on.
    // It carries its own bearer-auth layer (per-tool scope, not per-method), so
    // it sits outside /api/v1's REST middleware.
    let ct = CancellationToken::new();
    let app = shisho_mcp::transport::attach(router(state.clone()), state.clone(), ct);

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, mcp = cfg.mcp.enabled, "listening");
    axum::serve(listener, app).await?;
    Ok(())
}
