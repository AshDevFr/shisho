//! MangaBaka metadata provider — the non-optional metadata backbone.
//!
//! Self-contained port of tsundoku's `td-metadata-mangabaka` crate (same
//! upstream contract: the published dump format and the v1 REST API), with
//! the tsundoku-internal couplings replaced: own config struct
//! (`shisho_config::MangabakaConfig`), plain `reqwest`, and local metadata
//! types keyed by `shisho_ids::Provider`.
//!
//! Resolution order inside [`MangabakaProvider`]: negative cache
//! (miss-fast) → offline dump store (if loaded) → live API (only when
//! `api_fallback` and an API key are configured) → record a negative entry
//! on a confirmed miss.

pub mod client;
pub mod error;
#[cfg(feature = "test-fixtures")]
pub mod fixtures;
pub mod mapping;
mod metrics;
pub mod negative_cache;
pub mod offline;
pub mod provider;
pub mod scoring;
pub mod types;

pub use client::MangabakaClient;
pub use error::MangabakaError;
pub use negative_cache::NegativeCache;
pub use offline::OfflineStore;
pub use provider::{BackboneStatus, MangabakaProvider, RefreshAttempt};
pub use types::{
    ContentRating, MetadataOrigin, RefreshOutcome, RefreshSummary, SearchHit, SeriesKind,
    SeriesMetadata, SeriesStatus,
};

/// Canonical provider token (matches `shisho_ids::Provider::Mangabaka`).
pub const PROVIDER_ID: &str = "mangabaka";
