//! In-memory SWR caches (PRD Principle 2). The registry of concrete caches
//! is assembled by the services layer; this crate owns the primitive.

pub mod swr;

pub use swr::{Cached, Freshness, SwrCache, SwrConfig, SwrError};
