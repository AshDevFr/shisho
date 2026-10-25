//! The full per-user feedback pass: reconcile the verdict log against what
//! Codex shows (ratings, stalled and resumed reads, read progress), then
//! retune the user's ranking parameters if their tuning is stale.
//!
//! Three triggers run it: a snapshot build (the user's own traffic), the
//! scheduled reconcile sweep, and the `reconcile` tool. They can overlap for
//! one user, and inference reads the log before writing to it, so passes are
//! serialized per user by [`PassLocks`]; otherwise two concurrent passes
//! could both record the same stall.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sea_orm::{DatabaseConnection, DbErr};
use tokio::sync::OwnedMutexGuard;

use crate::library_snapshot::LibrarySnapshot;
use crate::reconciliation::{self, CodexAccess, ReconcileOutcome};
use crate::tuning::{self, Tuning};

/// One async lock per user, created on first use.
#[derive(Clone, Default)]
pub struct PassLocks {
    inner: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl PassLocks {
    /// Wait for any other pass for `user_sub` to finish, then hold the lock.
    pub async fn lock(&self, user_sub: &str) -> OwnedMutexGuard<()> {
        let lock = Arc::clone(
            self.inner
                .lock()
                .expect("pass lock map")
                .entry(user_sub.to_owned())
                .or_default(),
        );
        lock.lock_owned().await
    }
}

/// What one full pass did.
#[derive(Debug, Clone, Default)]
pub struct PassOutcome {
    pub reconcile: ReconcileOutcome,
    /// The new tuning, when the stored one was stale and was recomputed.
    pub retuned: Option<Tuning>,
}

/// Run the full pass for `user_sub` over `snapshot`. `codex` enables the
/// checks that need a live Codex read (stalled reads); without it only the
/// snapshot is consulted.
pub async fn run(
    db: &DatabaseConnection,
    locks: &PassLocks,
    user_sub: &str,
    snapshot: Arc<LibrarySnapshot>,
    codex: Option<CodexAccess<'_>>,
) -> Result<PassOutcome, DbErr> {
    let _guard = locks.lock(user_sub).await;
    let reconcile = reconciliation::reconcile_user(db, user_sub, Some(&snapshot), codex).await?;
    let retuned = tuning::retune_if_stale(db, user_sub, snapshot).await?;
    Ok(PassOutcome { reconcile, retuned })
}
