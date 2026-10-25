//! `shisho sweep …`: one-shot runs of the housekeeping sweeps that otherwise
//! run on the `serve` scheduler, so the same operations are invokable from an
//! external cron or by hand. Each runs the shared service logic once, prints a
//! summary, and exits non-zero on failure.
//!
//! Note on scope: `reconcile` re-reads each known user's library with their
//! stored Codex key (when encryption is on and `sweeps.reconcile_refresh` is
//! set), so a one-shot run does the full pass. `unresolvable` reads the
//! per-user snapshots that warm up from user activity on a running server; a
//! one-shot process has none, so it sees nothing here. Each summary states
//! what the run could and could not see.

use std::path::PathBuf;

use anyhow::Context;

use shisho_services::Services;
use shisho_services::sweeps::StoredKeys;

/// `shisho sweep reconcile`: the full per-user pass for every known user
/// (or, without stored-key access, the cached-snapshot pass).
pub async fn reconcile(config_path: PathBuf) -> anyhow::Result<()> {
    let (services, keys, refresh) = load(config_path).await?;
    let keys = keys.filter(|_| refresh);
    let report = shisho_services::sweeps::run_reconcile_sweep(&services, keys.as_ref())
        .await
        .context("running the reconcile sweep")?;
    println!(
        "reconcile sweep: {} known users ({} refreshed from Codex, {} could not be refreshed); {} flags raised, {} resolved, {} outcomes inferred",
        report.users,
        report.refreshed,
        report.skipped,
        report.flagged,
        report.resolved,
        report.inferred
    );
    if keys.is_none() && report.users > 0 {
        println!(
            "(libraries were not re-read: that needs [encryption] for the stored Codex keys and \
             sweeps.reconcile_refresh = true)"
        );
    }
    Ok(())
}

/// `shisho sweep unresolvable`: scan warm snapshots for series whose external
/// IDs map to no known provider, recording per-user tallies for Status.
pub async fn unresolvable(config_path: PathBuf) -> anyhow::Result<()> {
    let (services, _, _) = load(config_path).await?;
    let report = shisho_services::sweeps::run_unresolvable_sweep(&services)
        .await
        .context("running the unresolvable sweep")?;
    println!(
        "unresolvable sweep: {} known users ({} with a warm snapshot); {} unresolvable series",
        report.users, report.with_snapshot, report.unresolvable
    );
    if report.with_snapshot == 0 && report.users > 0 {
        println!(
            "(no warm library snapshots in this one-shot run; the scheduled sweep on a running \
             server scans the snapshots built by user activity)"
        );
    }
    Ok(())
}

/// Load config, initialize logging, and build the domain services, the
/// stored-key access, and `sweeps.reconcile_refresh`. Shared by the sweep
/// subcommands; errors when the recommendation service is not configured.
async fn load(config_path: PathBuf) -> anyhow::Result<(Services, Option<StoredKeys>, bool)> {
    let cfg = shisho_config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    super::init_tracing(&cfg);
    cfg.validate()?;
    let (services, keys) = super::build_services_from_config(&cfg).await?;
    Ok((services, keys, cfg.sweeps.reconcile_refresh))
}
