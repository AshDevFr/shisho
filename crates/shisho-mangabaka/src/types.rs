//! Canonical metadata types owned by this crate.
//!
//! These are the local replacements for `td-metadata`'s types: only the
//! fields Shisho consumes survive the port (no `raw` payload, no
//! `content_hash` — Shisho persists no metadata, so there is nothing to
//! dedupe writes against). Foreign IDs are typed `shisho_ids::ExternalId`
//! so consumers join on the canonical `(Provider, id)` pair directly.

use chrono::{DateTime, Utc};
use shisho_ids::ExternalId;

/// Which layer of the backbone answered: the offline dump or the live API.
/// Surfaced on responses so the model can phrase provenance honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataOrigin {
    Dump,
    Api,
}

impl MetadataOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            MetadataOrigin::Dump => "mangabaka-dump",
            MetadataOrigin::Api => "mangabaka-api",
        }
    }
}

/// Canonical series metadata for one MangaBaka row.
#[derive(Debug, Clone)]
pub struct SeriesMetadata {
    pub mangabaka_id: i64,
    pub title: String,
    pub alternate_titles: Vec<String>,
    pub kind: Option<SeriesKind>,
    pub status: Option<SeriesStatus>,
    pub year: Option<i32>,
    pub cover_url: Option<String>,
    pub total_volumes: Option<i32>,
    pub total_chapters: Option<i32>,
    /// Normalized to a 0-10 scale (MangaBaka publishes 0-100).
    pub rating: Option<f64>,
    pub description: Option<String>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    /// Cross-references to other providers, as canonical external IDs
    /// (the `mangabaka:<id>` identity itself is `mangabaka_id`).
    pub foreign_ids: Vec<ExternalId>,
    pub origin: MetadataOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeriesKind {
    Manga,
    Manhwa,
    Manhua,
    Novel,
    OneShot,
    /// Original English-language.
    Oel,
    Other(String),
}

impl SeriesKind {
    pub fn as_str(&self) -> &str {
        match self {
            SeriesKind::Manga => "manga",
            SeriesKind::Manhwa => "manhwa",
            SeriesKind::Manhua => "manhua",
            SeriesKind::Novel => "novel",
            SeriesKind::OneShot => "one_shot",
            SeriesKind::Oel => "oel",
            SeriesKind::Other(s) => s,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesStatus {
    Ongoing,
    Completed,
    Hiatus,
    Cancelled,
    Upcoming,
    Unknown,
}

impl SeriesStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SeriesStatus::Ongoing => "ongoing",
            SeriesStatus::Completed => "completed",
            SeriesStatus::Hiatus => "hiatus",
            SeriesStatus::Cancelled => "cancelled",
            SeriesStatus::Upcoming => "upcoming",
            SeriesStatus::Unknown => "unknown",
        }
    }
}

/// One title-search hit (dump FTS or API search).
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub mangabaka_id: i64,
    pub title: String,
    pub year: Option<i32>,
    pub cover_url: Option<String>,
    pub kind: Option<SeriesKind>,
    /// Relevance in `[0.0, 1.0]` when scored (dump FTS hits are Dice
    /// re-ranked; the API returns none).
    pub score: Option<f32>,
}

/// What a `refresh_cache()` call did.
#[derive(Debug, Clone)]
pub enum RefreshOutcome {
    Refreshed(RefreshSummary),
    /// Another refresh is in flight; this fire was skipped.
    SkippedAlreadyRunning,
    /// No `offline_dump_url` is configured (API-only mode).
    NotConfigured,
}

#[derive(Debug, Clone)]
pub struct RefreshSummary {
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub bytes_downloaded: u64,
    /// Active `series` rows in the promoted dump.
    pub series_rows: u64,
    /// SHA-1 of the downloaded tarball (doubles as a dump version).
    pub sha1: String,
}
