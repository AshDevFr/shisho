pub use sea_orm_migration::prelude::*;

mod m20260617_000001_create_users_api_keys;
mod m20260617_000002_create_oauth_tables;
mod m20260701_000001_create_verdicts;
mod m20260701_000002_create_profiles;
mod m20260701_000003_create_profile_versions;
mod m20260701_000004_create_reconciliation_flags;
mod m20260701_000005_create_feed_cursor;
mod m20260701_000006_users_codex_credential;
mod m20261024_000001_create_taste_tuning;

pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260617_000001_create_users_api_keys::Migration),
            Box::new(m20260617_000002_create_oauth_tables::Migration),
            Box::new(m20260701_000001_create_verdicts::Migration),
            Box::new(m20260701_000002_create_profiles::Migration),
            Box::new(m20260701_000003_create_profile_versions::Migration),
            Box::new(m20260701_000004_create_reconciliation_flags::Migration),
            Box::new(m20260701_000005_create_feed_cursor::Migration),
            Box::new(m20260701_000006_users_codex_credential::Migration),
            Box::new(m20261024_000001_create_taste_tuning::Migration),
        ]
    }
}
