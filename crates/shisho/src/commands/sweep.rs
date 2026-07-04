//! `shisho sweep …`: one-shot runs of the housekeeping sweeps that otherwise
//! run on the `serve` scheduler, so the same operations are invokable from an
//! external cron or by hand. Each runs the shared service logic once, prints a
//! summary, and exits non-zero on failure.
//!
//! Note on scope: the sweeps read the per-user library snapshots that warm up
//! from user activity on a running server. A one-shot CLI process has no warm
//! snapshots, so the detection half (new reconciliation flags, unresolvable
//! tallies) sees nothing here; the resolution half of `reconcile` (closing flags
//! whose series now carries a recorded outcome) is snapshot-independent and does
//! run. The summary states the warm-snapshot count so the output is honest about
//! what a one-shot run could and could not see.

use std::path::PathBuf;

use anyhow::Context;

use shisho_services::Services;

/// `shisho sweep reconcile`: resolve settled reconciliation flags and (for any
/// warm snapshot) detect new ones.
pub async fn reconcile(config_path: PathBuf) -> anyhow::Result<()> {
    let services = load(config_path).await?;
    let report = shisho_services::sweeps::run_reconcile_sweep(&services)
        .await
        .context("running the reconcile sweep")?;
    println!(
        "reconcile sweep: {} known users ({} with a warm snapshot); {} flags raised, {} resolved",
        report.users, report.with_snapshot, report.flagged, report.resolved
    );
    Ok(())
}

/// `shisho sweep unresolvable`: scan warm snapshots for series whose external
/// IDs map to no known provider, recording per-user tallies for Status.
pub async fn unresolvable(config_path: PathBuf) -> anyhow::Result<()> {
    let services = load(config_path).await?;
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

/// Load config, initialize logging, and build the domain services. Shared by the
/// sweep subcommands; errors when the recommendation service is not configured.
async fn load(config_path: PathBuf) -> anyhow::Result<Services> {
    let cfg = shisho_config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    super::init_tracing(&cfg);
    cfg.validate()?;
    super::build_services_from_config(&cfg).await
}
