//! Domain services layered over the Codex/Tsundoku/MangaBaka clients and the
//! database: the Codex identity broker, the per-user library snapshot, the two
//! candidate pools, lookup, library search, and the caches that bound their
//! backend load.

pub mod affinity;
pub mod candidates;
pub mod codex_identity;
pub mod eval;
pub mod health;
pub mod library;
pub mod library_snapshot;
pub mod lookup;
pub mod metadata;
pub mod profile_filters;
pub mod read_state;
pub mod reconciliation;
pub mod sweeps;
pub mod tsundoku_cache;
pub mod verdict_views;
pub mod verdicts;

use std::sync::Arc;
use std::time::Duration;

use sea_orm::DatabaseConnection;
use shisho_config::CacheConfig;

use shisho_cache::SwrConfig;
use shisho_codex::CodexClient;
pub use shisho_codex::UserCtx;
use shisho_mangabaka::MangabakaProvider;
use shisho_tsundoku::TsundokuClient;

/// Overall wall-clock budget for one inline cache build. Generous next to
/// the ~1-2 s cold library build; per-page timeouts come from the HTTP
/// client.
const BUILD_BUDGET: Duration = Duration::from_secs(30);

/// The cache registry: every SWR-cached service, constructed once at
/// `serve` and shared (cheap clones) by the tool handlers, the feed
/// poller, and `/healthz`.
#[derive(Clone)]
pub struct Services {
    pub snapshots: library_snapshot::LibrarySnapshotService,
    pub tsundoku: tsundoku_cache::TsundokuCache,
    /// The MangaBaka metadata backbone (resolution + merge order).
    pub metadata: metadata::MetadataService,
    pub db: DatabaseConnection,
    /// Housekeeping-sweep state: last-run timestamps and per-user
    /// unresolvable tallies, for `status()` and the Status page.
    pub sweeps: sweeps::SweepStatus,
    /// `tsundoku.owned_prefilter` from config: opt-in `owned=false`
    /// bandwidth pre-filter on acquirable-pool queries.
    pub tsundoku_owned_prefilter: bool,
}

impl Services {
    pub fn new(
        codex: CodexClient,
        tsundoku: TsundokuClient,
        mangabaka: Arc<MangabakaProvider>,
        db: DatabaseConnection,
        cache_cfg: &CacheConfig,
        tsundoku_owned_prefilter: bool,
    ) -> Self {
        // Lazy reconciliation: every snapshot build (the user's next
        // authenticated touch) re-runs flag detection for that user, so
        // users without warm snapshots at sweep time still reconcile.
        let hook_db = db.clone();
        let build_hook: library_snapshot::SnapshotBuildHook =
            std::sync::Arc::new(move |user_sub: &str, snapshot| {
                let db = hook_db.clone();
                let user_sub = user_sub.to_owned();
                let snapshot = std::sync::Arc::clone(snapshot);
                tokio::spawn(async move {
                    match reconciliation::reconcile_user(&db, &user_sub, Some(&snapshot)).await {
                        Ok(outcome)
                            if outcome.flagged > 0
                                || outcome.resolved > 0
                                || outcome.inferred > 0 =>
                        {
                            tracing::info!(
                                user_sub,
                                flagged = outcome.flagged,
                                resolved = outcome.resolved,
                                inferred = outcome.inferred,
                                "lazy reconciliation on snapshot build"
                            );
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::warn!(user_sub, error = %err, "lazy reconciliation failed");
                        }
                    }
                });
            });

        Self {
            tsundoku_owned_prefilter,
            metadata: metadata::MetadataService::new(mangabaka),
            sweeps: sweeps::SweepStatus::default(),
            snapshots: library_snapshot::LibrarySnapshotService::new(
                codex,
                SwrConfig {
                    revalidate_after: cache_cfg.library_snapshot_ttl(),
                    build_budget: BUILD_BUDGET,
                },
            )
            .with_build_hook(build_hook),
            tsundoku: tsundoku_cache::TsundokuCache::new(
                tsundoku,
                db.clone(),
                SwrConfig {
                    revalidate_after: cache_cfg.tsundoku_ttl(),
                    build_budget: BUILD_BUDGET,
                },
            ),
            db,
        }
    }
}
