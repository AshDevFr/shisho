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
pub mod recommendations;
pub mod reconciliation;
pub mod sweeps;
pub mod tsundoku_cache;
pub mod tuning;
pub mod user_pass;
pub mod verdict_views;
pub mod verdicts;

use std::sync::Arc;
use std::time::Duration;

use sea_orm::DatabaseConnection;
use shisho_config::CacheConfig;

use shisho_cache::SwrConfig;
pub use shisho_codex::UserCtx;
use shisho_codex::{CodexClient, CodexCredential};
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
    /// Serializes the full per-user pass across its triggers.
    pub passes: user_pass::PassLocks,
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
        // authenticated touch) runs the full pass for that user, so users
        // without warm snapshots at sweep time still reconcile.
        let passes = user_pass::PassLocks::default();
        let hook_db = db.clone();
        let hook_passes = passes.clone();
        let build_hook: library_snapshot::SnapshotBuildHook = std::sync::Arc::new(
            move |user_sub: &str, codex: &CodexClient, credential: &CodexCredential, snapshot| {
                let db = hook_db.clone();
                let passes = hook_passes.clone();
                let user_sub = user_sub.to_owned();
                let codex = codex.clone();
                let credential = credential.clone();
                let snapshot = std::sync::Arc::clone(snapshot);
                tokio::spawn(async move {
                    let access = reconciliation::CodexAccess {
                        client: &codex,
                        credential: &credential,
                    };
                    match user_pass::run(&db, &passes, &user_sub, snapshot, Some(access)).await {
                        Ok(outcome) => log_pass(&user_sub, "snapshot build", &outcome),
                        Err(err) => {
                            tracing::warn!(user_sub, error = %err, "lazy reconciliation failed");
                        }
                    }
                });
            },
        );

        Self {
            tsundoku_owned_prefilter,
            metadata: metadata::MetadataService::new(mangabaka),
            sweeps: sweeps::SweepStatus::default(),
            passes,
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

/// A full pass that could not run: Codex refused or failed the library read,
/// or the verdict log could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum PassError {
    #[error("reading the library from Codex failed")]
    Codex(#[from] shisho_codex::CodexError),
    #[error("reconciliation failed")]
    Db(#[from] sea_orm::DbErr),
}

impl Services {
    /// Re-read `user_sub`'s library from Codex with `credential` and run the
    /// full per-user pass over it. Returns the fresh snapshot's `as_of` with
    /// what the pass did.
    pub async fn reconcile_now(
        &self,
        user_sub: &str,
        credential: &CodexCredential,
    ) -> Result<(chrono::DateTime<chrono::Utc>, user_pass::PassOutcome), PassError> {
        let hit = self.snapshots.refresh(user_sub, credential).await?;
        let access = reconciliation::CodexAccess {
            client: self.snapshots.codex(),
            credential,
        };
        let outcome =
            user_pass::run(&self.db, &self.passes, user_sub, hit.value, Some(access)).await?;
        log_pass(user_sub, "on demand", &outcome);
        Ok((hit.as_of, outcome))
    }
}

/// Log a pass that changed something; a pass that found nothing is silent.
fn log_pass(user_sub: &str, trigger: &str, outcome: &user_pass::PassOutcome) {
    let r = outcome.reconcile;
    if r.flagged > 0 || r.resolved > 0 || r.inferred > 0 {
        tracing::info!(
            user_sub,
            trigger,
            flagged = r.flagged,
            resolved = r.resolved,
            inferred = r.inferred,
            "reconciliation recorded changes"
        );
    }
    if let Some(t) = &outcome.retuned {
        tracing::info!(
            user_sub,
            trigger,
            tuned = t.tuned,
            favourites = t.favourites,
            "ranking parameters retuned"
        );
    }
}
