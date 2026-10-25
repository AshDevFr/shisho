use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use shisho_api::AppState;
use shisho_api::router;
use shisho_config::AppConfig;
use shisho_services::Services;
use tokio_cron_scheduler::JobScheduler;
use tokio_util::sync::CancellationToken;

pub async fn run(config_path: PathBuf) -> anyhow::Result<()> {
    let cfg = shisho_config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    super::init_tracing(&cfg);
    cfg.validate()?;

    let state = super::build_app_state(&cfg).await?;

    // Periodic + boot-time domain work (sweeps, MangaBaka refresh, feed poller).
    // Held for the life of `serve`: dropping the handles stops the jobs.
    let _jobs = start_background_jobs(&state).await;

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

/// The background-job handles that must outlive `serve`: the cron scheduler
/// (reconcile / unresolvable sweeps + weekly MangaBaka refresh) and the Tsundoku
/// feed-poller task. Kept in scope by `run`; dropping them stops the jobs.
struct BackgroundJobs {
    _scheduler: Option<JobScheduler>,
    _feed_poller: Option<tokio::task::JoinHandle<()>>,
}

/// The stored-key access the reconcile sweep needs, when this instance can
/// read users' stored Codex keys (encryption on, Codex configured).
fn stored_keys(state: &AppState) -> Option<shisho_services::sweeps::StoredKeys> {
    Some(shisho_services::sweeps::StoredKeys {
        identity: state.codex_identity.clone()?,
        dek: state.dek.clone()?,
    })
}

/// Register the periodic domain jobs and trigger a boot-time MangaBaka refresh
/// when the dump is missing or stale. A no-op when the recommendation service is
/// not configured (no `codex.base_url`). Never propagates an error: a job that
/// fails to register is logged and skipped so `serve` still comes up.
async fn start_background_jobs(state: &AppState) -> BackgroundJobs {
    let Some(services) = state.services.as_ref() else {
        tracing::info!("recommendation service not configured; skipping background jobs");
        return BackgroundJobs {
            _scheduler: None,
            _feed_poller: None,
        };
    };

    boot_refresh_if_stale(services, &state.cfg).await;

    let keys = stored_keys(state);
    if state.cfg.sweeps.reconcile_refresh && keys.is_none() {
        tracing::info!(
            "the reconcile sweep reads cached snapshots only: stored Codex keys need [encryption]"
        );
    }
    let scheduler = match shisho_services::sweeps::spawn_scheduler(
        services.clone(),
        keys.filter(|_| state.cfg.sweeps.reconcile_refresh),
        &state.cfg.sweeps.reconcile_cron,
        &state.cfg.sweeps.unresolvable_cron,
        state.cfg.mangabaka.offline_refresh_cron.as_deref(),
    )
    .await
    {
        Ok(scheduler) => Some(scheduler),
        Err(err) => {
            tracing::error!(error = %err, "failed to start the sweep scheduler; sweeps disabled");
            None
        }
    };

    // The feed poller drives Tsundoku cache invalidation; only meaningful when
    // Tsundoku is configured.
    let feed_poller = services.tsundoku.client().is_configured().then(|| {
        let every = state.cfg.cache.tsundoku_ttl();
        tracing::info!(
            interval_secs = every.as_secs(),
            "starting the Tsundoku feed poller"
        );
        services.tsundoku.spawn_feed_poller(every)
    });

    BackgroundJobs {
        _scheduler: scheduler,
        _feed_poller: feed_poller,
    }
}

/// Kick off a MangaBaka dump refresh at boot when the dump is missing or older
/// than the configured max age, so a cold or stale instance becomes useful
/// without waiting for the next cron tick. Spawned, not awaited: serving starts
/// immediately and the provider serves API-fallback until the dump lands.
async fn boot_refresh_if_stale(services: &Services, cfg: &AppConfig) {
    let status = services.metadata.provider().status().await;
    if !dump_needs_boot_refresh(
        status.dump_loaded,
        status.dump_modified_at,
        cfg.mangabaka.dump_max_age(),
        Utc::now(),
    ) {
        return;
    }
    tracing::info!(
        dump_loaded = status.dump_loaded,
        "MangaBaka dump missing or stale; refreshing at boot"
    );
    let provider = Arc::clone(services.metadata.provider());
    tokio::spawn(async move {
        shisho_services::sweeps::run_mangabaka_refresh(&provider).await;
    });
}

/// Whether a boot-time refresh is warranted: the dump is not loaded, or a max
/// age is configured and the dump is older than it. Pure, for unit testing.
fn dump_needs_boot_refresh(
    dump_loaded: bool,
    modified_at: Option<DateTime<Utc>>,
    max_age: Option<Duration>,
    now: DateTime<Utc>,
) -> bool {
    if !dump_loaded {
        return true;
    }
    match (modified_at, max_age) {
        (Some(modified), Some(max)) => now
            .signed_duration_since(modified)
            .to_std()
            .map(|age| age > max)
            .unwrap_or(false),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_dump_always_refreshes() {
        assert!(dump_needs_boot_refresh(false, None, None, Utc::now()));
        // Even with no max age configured, a missing dump refreshes.
        assert!(dump_needs_boot_refresh(
            false,
            Some(Utc::now()),
            Some(Duration::from_secs(60)),
            Utc::now()
        ));
    }

    #[test]
    fn loaded_dump_without_a_max_age_never_refreshes() {
        assert!(!dump_needs_boot_refresh(
            true,
            Some(Utc::now()),
            None,
            Utc::now()
        ));
    }

    #[test]
    fn a_stale_dump_refreshes_but_a_fresh_one_does_not() {
        let now = Utc::now();
        let max = Duration::from_secs(8 * 24 * 60 * 60); // 8 days
        let stale = now - chrono::Duration::days(9);
        let fresh = now - chrono::Duration::days(1);
        assert!(dump_needs_boot_refresh(true, Some(stale), Some(max), now));
        assert!(!dump_needs_boot_refresh(true, Some(fresh), Some(max), now));
    }
}
