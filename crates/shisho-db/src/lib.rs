//! Storage layer for shisho.
//!
//! - [`conn`] opens the database, applies and verifies the SQLite pragmas
//!   (WAL, foreign keys), optionally keying it with SQLCipher, and runs
//!   migrations.
//! - [`keys`] turns the operator's KEK configuration into the data key (DEK)
//!   that opens an encrypted database, and rotates the wrapping KEK.
//! - [`entities`] are the sea-orm models for the identity schema in `migration`.
//! - [`repos`] are thin typed query helpers consumed by the auth, API, and MCP
//!   crates. They stay small; orchestration lives in the caller.

pub mod conn;
pub mod entities;
pub mod keys;
pub mod repos;

pub use sea_orm::DatabaseConnection;

// Re-export the common entry points so callers use `shisho_db::connect(..)` etc.
pub use conn::{connect, connect_in_memory, run_migrations};
pub use repos::UserScope;

/// The data-encryption key, re-exported so callers can thread it through
/// [`conn::connect`] without depending on `shisho-crypto` directly.
pub use shisho_crypto::Dek;
