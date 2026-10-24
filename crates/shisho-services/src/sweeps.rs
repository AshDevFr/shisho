//! In-process housekeeping sweeps (PRD Part 4): outcome reconciliation
//! (weekly) and the unresolvable-series scan (daily), scheduled inside
//! `serve` with `tokio-cron-scheduler`.
//!
//! "Per known user" means users with rows in Shisho's own tables; sweeps
//! read the cached library snapshots only ([`peek`] — no credential, no
//! rebuild), because Shisho can never originate a per-user Codex call.
//! A user without a warm snapshot is skipped here and reconciles lazily on
//! their next authenticated touch (the snapshot build hook).
//!
//! Results live in memory ([`SweepStatus`]) plus the reconciliation flags
//! in the database; a restart costs nothing but the last-run timestamps.
//!
//! [`peek`]: crate::library_snapshot::LibrarySnapshotService::peek

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use tokio_cron_scheduler::{Job, JobScheduler, JobSchedulerError};

use crate::Services;
use crate::reconciliation;
use shisho_db::repos::users;

/// One user's unresolvable-series tally from the last scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnresolvableEntry {
    pub count: usize,
    /// `as_of` of the snapshot the tally was read from.
    pub snapshot_as_of: DateTime<Utc>,
    pub swept_at: DateTime<Utc>,
}

/// Last-run timestamps and per-user unresolvable tallies, shared by the
/// scheduler, the manual triggers, `status()`, and the Status page.
#[derive(Clone, Default)]
pub struct SweepStatus {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    reconcile_last_run: RwLock<Option<DateTime<Utc>>>,
    unresolvable_last_run: RwLock<Option<DateTime<Utc>>>,
    unresolvable: RwLock<HashMap<String, UnresolvableEntry>>,
}

impl SweepStatus {
    pub fn reconcile_last_run(&self) -> Option<DateTime<Utc>> {
        *self.inner.reconcile_last_run.read().expect("sweep lock")
    }

    pub fn unresolvable_last_run(&self) -> Option<DateTime<Utc>> {
        *self.inner.unresolvable_last_run.read().expect("sweep lock")
    }

    /// The caller's tally from the last unresolvable scan, if a warm
    /// snapshot existed for them when it ran.
    pub fn unresolvable_for(&self, user_sub: &str) -> Option<UnresolvableEntry> {
        self.inner
            .unresolvable
            .read()
            .expect("sweep lock")
            .get(user_sub)
            .copied()
    }
}

/// What one reconcile sweep did across all known users.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileSweepReport {
    pub users: usize,
    /// Users with a warm snapshot (detection ran, not just resolution).
    pub with_snapshot: usize,
    pub flagged: usize,
    pub resolved: usize,
    /// Outcomes recorded from Codex ratings.
    pub inferred: usize,
}

/// What one unresolvable scan saw across all known users.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnresolvableSweepReport {
    pub users: usize,
    pub with_snapshot: usize,
    /// Sum of unresolvable series across scanned snapshots.
    pub unresolvable: usize,
}

/// The outcome-reconciliation sweep: per known user, resolve settled
/// flags and (when a warm snapshot exists) detect new ones.
pub async fn run_reconcile_sweep(
    services: &Services,
) -> Result<ReconcileSweepReport, sea_orm::DbErr> {
    let users = users::known_user_subs(&services.db).await?;
    let mut report = ReconcileSweepReport {
        users: users.len(),
        ..Default::default()
    };
    for user_sub in &users {
        let snapshot = services.snapshots.peek(user_sub);
        if snapshot.is_some() {
            report.with_snapshot += 1;
        }
        let outcome = reconciliation::reconcile_user(
            &services.db,
            user_sub,
            snapshot.as_ref().map(|hit| hit.value.as_ref()),
            // No credential here: stall checks run on the user's own
            // snapshot builds, which carry one.
            None,
        )
        .await?;
        report.flagged += outcome.flagged;
        report.resolved += outcome.resolved;
        report.inferred += outcome.inferred;
    }
    *services
        .sweeps
        .inner
        .reconcile_last_run
        .write()
        .expect("sweep lock") = Some(Utc::now());
    tracing::info!(
        users = report.users,
        with_snapshot = report.with_snapshot,
        flagged = report.flagged,
        resolved = report.resolved,
        inferred = report.inferred,
        "reconcile sweep complete"
    );
    Ok(report)
}

/// The unresolvable-series scan: per known user with a warm snapshot,
/// record the tally for Status. Nothing is ever patched (Principle 8);
/// the fix is Codex's metadata-match flow, deep-linked from Status.
pub async fn run_unresolvable_sweep(
    services: &Services,
) -> Result<UnresolvableSweepReport, sea_orm::DbErr> {
    let users = users::known_user_subs(&services.db).await?;
    let mut report = UnresolvableSweepReport {
        users: users.len(),
        ..Default::default()
    };
    let now = Utc::now();
    for user_sub in &users {
        let Some(hit) = services.snapshots.peek(user_sub) else {
            continue;
        };
        report.with_snapshot += 1;
        report.unresolvable += hit.value.unresolvable.len();
        services
            .sweeps
            .inner
            .unresolvable
            .write()
            .expect("sweep lock")
            .insert(
                user_sub.clone(),
                UnresolvableEntry {
                    count: hit.value.unresolvable.len(),
                    snapshot_as_of: hit.as_of,
                    swept_at: now,
                },
            );
    }
    *services
        .sweeps
        .inner
        .unresolvable_last_run
        .write()
        .expect("sweep lock") = Some(now);
    tracing::info!(
        users = report.users,
        with_snapshot = report.with_snapshot,
        unresolvable = report.unresolvable,
        "unresolvable sweep complete"
    );
    Ok(report)
}

/// Register the sweeps (and the MangaBaka dump refresh, when scheduled)
/// with their cron schedules and start the scheduler. Keep the returned
/// handle alive for the life of `serve`; an invalid cron expression fails
/// here, at startup, never silently at fire time.
pub async fn spawn_scheduler(
    services: Services,
    reconcile_cron: &str,
    unresolvable_cron: &str,
    mangabaka_refresh_cron: Option<&str>,
) -> Result<JobScheduler, JobSchedulerError> {
    let scheduler = JobScheduler::new().await?;

    let svc = services.clone();
    scheduler
        .add(Job::new_async(reconcile_cron, move |_uuid, _lock| {
            let services = svc.clone();
            Box::pin(async move {
                if let Err(err) = run_reconcile_sweep(&services).await {
                    tracing::warn!(error = %err, "reconcile sweep failed");
                }
            })
        })?)
        .await?;

    let svc = services.clone();
    scheduler
        .add(Job::new_async(unresolvable_cron, move |_uuid, _lock| {
            let services = svc.clone();
            Box::pin(async move {
                if let Err(err) = run_unresolvable_sweep(&services).await {
                    tracing::warn!(error = %err, "unresolvable sweep failed");
                }
            })
        })?)
        .await?;

    // The MangaBaka dump refresh: wholesale, serialized inside the
    // provider (an overlapping fire is skipped with a warning).
    if let Some(refresh_cron) = mangabaka_refresh_cron {
        let provider = std::sync::Arc::clone(services.metadata.provider());
        scheduler
            .add(Job::new_async(refresh_cron, move |_uuid, _lock| {
                let provider = std::sync::Arc::clone(&provider);
                Box::pin(async move { run_mangabaka_refresh(&provider).await })
            })?)
            .await?;
    }

    scheduler.start().await?;
    tracing::info!(
        reconcile_cron,
        unresolvable_cron,
        mangabaka_refresh_cron,
        "sweep scheduler started"
    );
    Ok(scheduler)
}

/// Run a wholesale MangaBaka dump refresh and log the outcome. Shared by
/// the scheduled job and the boot-time download `serve` kicks off when no
/// dump is on disk yet. The provider serializes overlapping runs itself.
pub async fn run_mangabaka_refresh(provider: &Arc<shisho_mangabaka::MangabakaProvider>) {
    match provider.refresh_cache().await {
        Ok(shisho_mangabaka::RefreshOutcome::Refreshed(summary)) => {
            tracing::info!(
                series_rows = summary.series_rows,
                bytes = summary.bytes_downloaded,
                sha1 = %summary.sha1,
                "MangaBaka dump refresh complete"
            );
        }
        // The provider already warned (overlapping run) or reported
        // NotConfigured (API-only mode, nothing to download).
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(error = %err, "MangaBaka dump refresh failed; the previous dump (if any) keeps serving");
        }
    }
}
