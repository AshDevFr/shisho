pub mod encryption;
pub mod mangabaka;
pub mod mcp;
pub mod migrate;
pub mod openapi;
pub mod report;
pub mod serve;
pub mod sweep;

use std::sync::Arc;

use anyhow::Context;
use shisho_api::AppState;
use shisho_api::setup::{self, SetupState};
use shisho_auth::providers::ProviderRegistry;
use shisho_auth::session::SessionKeys;
use shisho_config::AppConfig;
use tracing_subscriber::EnvFilter;

/// Initialize tracing once per process, writing to stdout. `RUST_LOG` overrides
/// the config level. sqlx query logging is pinned to `warn` unless explicitly
/// raised.
pub fn init_tracing(cfg: &AppConfig) {
    init_tracing_inner(cfg, false);
}

/// Like [`init_tracing`] but writes to **stderr**. Required for `mcp stdio`,
/// where stdout carries the JSON-RPC protocol stream and any log line on it
/// would corrupt the exchange.
pub fn init_tracing_stderr(cfg: &AppConfig) {
    init_tracing_inner(cfg, true);
}

fn init_tracing_inner(cfg: &AppConfig, stderr: bool) {
    let filter =
        std::env::var("RUST_LOG").unwrap_or_else(|_| format!("{},sqlx=warn", cfg.logging.level));
    let builder = tracing_subscriber::fmt().with_env_filter(EnvFilter::new(filter));
    match (cfg.logging.json, stderr) {
        (true, true) => builder.json().with_writer(std::io::stderr).init(),
        (true, false) => builder.json().init(),
        (false, true) => builder.with_writer(std::io::stderr).init(),
        (false, false) => builder.init(),
    }
}

/// Open the database (running migrations) and build just the domain
/// [`shisho_services::Services`] — the DB + backend clients + caches — without
/// the full [`AppState`]. Used by the one-shot domain subcommands (`sweep …`),
/// which need the services but not the HTTP surface (providers, sessions, setup
/// state). Errors when the recommendation service is not configured
/// (`codex.base_url` unset), since there is nothing to run without it.
pub async fn build_services_from_config(
    cfg: &AppConfig,
) -> anyhow::Result<shisho_services::Services> {
    let dek = shisho_db::keys::bootstrap_dek(cfg)?;
    let db = shisho_db::connect(cfg, dek.as_ref()).await?;
    shisho_db::run_migrations(&db).await?;
    shisho_api::build_services(cfg, db)
        .await?
        .context("the recommendation service is not configured (set codex.base_url)")
}

/// Open the (optionally encrypted) database, run migrations, and assemble the
/// shared [`AppState`]. Shared by `serve` and `mcp stdio` so both surfaces run
/// over identical state. Mints and logs the first-run admin key + setup token on
/// a fresh, unconfigured instance.
pub async fn build_app_state(cfg: &AppConfig) -> anyhow::Result<Arc<AppState>> {
    let dek = shisho_db::keys::bootstrap_dek(cfg)?;
    let db = shisho_db::connect(cfg, dek.as_ref()).await?;
    shisho_db::run_migrations(&db).await?;

    // OIDC discovery runs here in multi-user mode, so a bad issuer fails at
    // startup, not at first login. Empty in single-user.
    let providers = ProviderRegistry::build(cfg)
        .await
        .context("building login providers")?;
    let session = SessionKeys::from_config(&cfg.auth.session);

    let setup = Arc::new(if setup::is_unconfigured(cfg) {
        SetupState::enabled(setup::generate_token())
    } else {
        SetupState::disabled()
    });
    if let Some(key) = shisho_auth::bootstrap::ensure_first_run_key(&db, cfg).await? {
        tracing::info!("generated first-run admin API key (shown once): {key}");
    }
    if let Some(token) = setup.token_for_display() {
        tracing::info!(
            "first-run setup is open at {}/setup?token={token}",
            cfg.server.public_base()
        );
    }

    let services = shisho_api::build_services(cfg, db.clone())
        .await
        .context("building the domain services")?;

    Ok(Arc::new(AppState {
        db,
        cfg: cfg.clone(),
        providers: Arc::new(providers),
        session,
        setup,
        // Retained (not just used to open the DB) so request handlers can seal
        // the minted Codex key at sign-in.
        dek,
        codex_identity: shisho_api::build_codex_identity(cfg),
        services,
    }))
}
