pub use sea_orm_migration::prelude::*;

mod m20260617_000001_create_users_api_keys;
mod m20260617_000002_create_oauth_tables;

pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260617_000001_create_users_api_keys::Migration),
            Box::new(m20260617_000002_create_oauth_tables::Migration),
        ]
    }
}
