//! `shisho mangabaka refresh`: manual/initial wholesale ingest of the
//! MangaBaka offline dump. Runs the full pipeline (download -> extract ->
//! validate -> prepare -> atomic promote) and exits non-zero on failure, so
//! deployment scripts can gate on it. The running server picks the new dump
//! up on its next scheduled refresh (or restart); the two processes never
//! write the live file concurrently thanks to the atomic promote.

use std::path::PathBuf;

use anyhow::{Context, bail};

use shisho_mangabaka::{MangabakaProvider, RefreshOutcome};

pub async fn refresh(config_path: PathBuf) -> anyhow::Result<()> {
    let cfg = shisho_config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    super::init_tracing(&cfg);
    cfg.mangabaka
        .validate()
        .map_err(|message| anyhow::anyhow!(message))?;

    let dump_dir = cfg.database.mangabaka_dir();
    let provider = MangabakaProvider::new(&cfg.mangabaka, dump_dir.clone())
        .context("building the MangaBaka provider")?;

    tracing::info!(
        dump_dir = %dump_dir.display(),
        "starting MangaBaka dump refresh (download + ingest can take minutes)"
    );
    match provider.refresh_cache().await? {
        RefreshOutcome::Refreshed(summary) => {
            tracing::info!(
                series_rows = summary.series_rows,
                bytes_downloaded = summary.bytes_downloaded,
                sha1 = %summary.sha1,
                duration_secs = (summary.finished_at - summary.started_at).num_seconds(),
                "MangaBaka dump refreshed"
            );
            Ok(())
        }
        RefreshOutcome::SkippedAlreadyRunning => {
            // Cannot happen here (this process owns the only provider), but
            // exit non-zero rather than claim success.
            bail!("refresh skipped: another refresh is already running");
        }
        RefreshOutcome::NotConfigured => {
            bail!(
                "mangabaka.offline_dump_url is not configured (API-only mode); \
                 set it to enable the offline dump"
            );
        }
    }
}
