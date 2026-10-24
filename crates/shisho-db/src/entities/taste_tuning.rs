//! Per-user ranking parameters chosen from the user's own library. `params`
//! is opaque JSON owned by the services layer.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "taste_tuning")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_sub: String,
    pub params: String,
    /// Whether `params` beat the defaults; `false` stores the defaults.
    pub tuned: bool,
    /// Favourites the evaluation held out.
    pub favourites: i32,
    pub baseline_rank_fraction: Option<f64>,
    pub rank_fraction: Option<f64>,
    pub computed_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
