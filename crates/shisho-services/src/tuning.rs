//! Per-user ranking parameters. Each user's [`TasteParams`] are chosen by
//! replaying their own library through the hold-out evaluation (see
//! [`crate::eval`]) over a small fixed grid, so no user's ranking depends on
//! anyone else's data. The defaults are a cold-start prior: a user keeps them
//! until they have enough ratings to measure, and whenever no grid point
//! beats them by a clear margin.

use std::sync::Arc;

use sea_orm::{DatabaseConnection, DbErr};

use crate::affinity::{AffinityRanker, TasteParams};
use crate::eval::{EvalReport, evaluate, favourites};
use crate::library_snapshot::LibrarySnapshot;
use shisho_db::repos::taste_tuning::{self, NewTuning};

/// Fewer ratings than this and the defaults are kept: too few favourites to
/// tell grid points apart from noise.
pub const MIN_RATINGS: usize = 30;
/// How much lower (better) a grid point's mean rank fraction must be than
/// the defaults' to replace them.
pub const MARGIN: f64 = 0.02;
/// A stored tuning older than this is recomputed on the next snapshot build.
pub const RETUNE_AFTER: chrono::TimeDelta = chrono::TimeDelta::days(7);

/// The parameter sets a user can be tuned to: each structural choice in both
/// directions around the defaults.
pub fn grid() -> Vec<TasteParams> {
    let mut points = Vec::new();
    for origin_sd in [0.0, 1.0] {
        for library_specificity in [false, true] {
            for tag_factor in [0.4, 0.6, 1.0] {
                for length_norm in [0.5, 1.0] {
                    points.push(TasteParams {
                        origin_sd,
                        library_specificity,
                        tag_factor,
                        length_norm,
                        ..TasteParams::default()
                    });
                }
            }
        }
    }
    points
}

/// What tuning one library found.
#[derive(Debug, Clone, PartialEq)]
pub struct Tuning {
    /// The parameters to rank with: the winner, or the defaults.
    pub params: TasteParams,
    /// Whether a grid point beat the defaults by [`MARGIN`].
    pub tuned: bool,
    /// Favourites the evaluation held out (0 when it did not run).
    pub favourites: usize,
    pub baseline: Option<EvalReport>,
    pub best: Option<EvalReport>,
}

/// Choose parameters for `snapshot` from its own favourites. CPU-bound: run
/// it off the async runtime.
pub fn tune(snapshot: &LibrarySnapshot) -> Tuning {
    let defaults = Tuning {
        params: TasteParams::default(),
        tuned: false,
        favourites: 0,
        baseline: None,
        best: None,
    };
    let ratings = snapshot
        .series
        .iter()
        .filter(|s| s.rating.is_some())
        .count();
    if ratings < MIN_RATINGS {
        return defaults;
    }
    let run = |params: &TasteParams| {
        evaluate(
            snapshot,
            &AffinityRanker {
                params: params.clone(),
                label: None,
            },
        )
    };
    let Some(baseline) = run(&TasteParams::default()) else {
        return defaults;
    };
    let best = grid()
        .into_iter()
        .filter_map(|params| run(&params).map(|report| (params, report)))
        .min_by(|a, b| {
            a.1.mean_rank_fraction
                .total_cmp(&b.1.mean_rank_fraction)
                .then(b.1.recall_at_40.total_cmp(&a.1.recall_at_40))
        });
    let favourites = favourites(snapshot).len();
    match best {
        Some((params, report))
            if report.mean_rank_fraction <= baseline.mean_rank_fraction - MARGIN =>
        {
            Tuning {
                params,
                tuned: true,
                favourites,
                baseline: Some(baseline),
                best: Some(report),
            }
        }
        _ => Tuning {
            favourites,
            baseline: Some(baseline),
            ..defaults
        },
    }
}

/// The parameters to rank `user_sub` with: their stored tuning, else the
/// defaults. A row that no longer parses reads as the defaults.
pub async fn params_for(db: &DatabaseConnection, user_sub: &str) -> TasteParams {
    match taste_tuning::get(db, user_sub).await {
        Ok(Some(row)) => serde_json::from_str(&row.params).unwrap_or_default(),
        Ok(None) => TasteParams::default(),
        Err(err) => {
            tracing::warn!(user_sub, error = %err, "reading tuned parameters failed; using defaults");
            TasteParams::default()
        }
    }
}

/// Tune `user_sub` from `snapshot` and store the result, unless a tuning
/// newer than [`RETUNE_AFTER`] is already stored. `None` when it was fresh.
pub async fn retune_if_stale(
    db: &DatabaseConnection,
    user_sub: &str,
    snapshot: Arc<LibrarySnapshot>,
) -> Result<Option<Tuning>, DbErr> {
    if let Some(row) = taste_tuning::get(db, user_sub).await?
        && chrono::Utc::now() - row.computed_at.to_utc() < RETUNE_AFTER
    {
        return Ok(None);
    }
    let tuning = tokio::task::spawn_blocking(move || tune(&snapshot))
        .await
        .map_err(|err| DbErr::Custom(format!("tuning task failed: {err}")))?;
    store(db, user_sub, &tuning).await?;
    Ok(Some(tuning))
}

/// Store `tuning` as `user_sub`'s parameters.
pub async fn store(db: &DatabaseConnection, user_sub: &str, tuning: &Tuning) -> Result<(), DbErr> {
    let params = serde_json::to_string(&tuning.params)
        .map_err(|err| DbErr::Custom(format!("serializing parameters: {err}")))?;
    taste_tuning::upsert(
        db,
        user_sub,
        NewTuning {
            params,
            tuned: tuning.tuned,
            favourites: i32::try_from(tuning.favourites).unwrap_or(i32::MAX),
            baseline_rank_fraction: tuning.baseline.as_ref().map(|r| r.mean_rank_fraction),
            rank_fraction: tuning
                .best
                .as_ref()
                .or(tuning.baseline.as_ref())
                .map(|r| r.mean_rank_fraction),
        },
    )
    .await
}
