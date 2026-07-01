//! Repository layer: all domain queries live here, taking a plain
//! `&DatabaseConnection`, so REST handlers, MCP tools, and the CLI share one
//! implementation without axum types in the signatures.

pub mod scope;
pub mod users;

pub use scope::UserScope;
