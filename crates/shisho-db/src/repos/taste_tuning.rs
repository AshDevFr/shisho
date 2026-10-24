//! Per-user ranking parameters: one row per user, replaced on each tuning.

use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, DatabaseConnection, DbErr, EntityTrait};

use crate::entities::taste_tuning;

/// A tuning result to store for a user.
#[derive(Debug, Clone)]
pub struct NewTuning {
    pub params: String,
    pub tuned: bool,
    pub favourites: i32,
    pub baseline_rank_fraction: Option<f64>,
    pub rank_fraction: Option<f64>,
}

pub async fn get(
    db: &DatabaseConnection,
    user_sub: &str,
) -> Result<Option<taste_tuning::Model>, DbErr> {
    taste_tuning::Entity::find_by_id(user_sub.to_owned())
        .one(db)
        .await
}

/// Store `tuning` as the user's current parameters, replacing any earlier.
pub async fn upsert(
    db: &DatabaseConnection,
    user_sub: &str,
    tuning: NewTuning,
) -> Result<(), DbErr> {
    let row = taste_tuning::ActiveModel {
        user_sub: Set(user_sub.to_owned()),
        params: Set(tuning.params),
        tuned: Set(tuning.tuned),
        favourites: Set(tuning.favourites),
        baseline_rank_fraction: Set(tuning.baseline_rank_fraction),
        rank_fraction: Set(tuning.rank_fraction),
        computed_at: Set(chrono::Utc::now().into()),
    };
    taste_tuning::Entity::insert(row)
        .on_conflict(
            OnConflict::column(taste_tuning::Column::UserSub)
                .update_columns([
                    taste_tuning::Column::Params,
                    taste_tuning::Column::Tuned,
                    taste_tuning::Column::Favourites,
                    taste_tuning::Column::BaselineRankFraction,
                    taste_tuning::Column::RankFraction,
                    taste_tuning::Column::ComputedAt,
                ])
                .to_owned(),
        )
        .exec(db)
        .await?;
    Ok(())
}
