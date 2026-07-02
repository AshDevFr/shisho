//! `MetadataService`: the backbone facade plus the merge order used
//! everywhere — MangaBaka is authoritative for canonical metadata (title,
//! description, genres/tags, rating); Tsundoku contributes coverage,
//! presence, and acquirability; Codex contributes the caller's per-user
//! state. Consumers (`lookup`, candidate enrichment, UI verdict rows) go
//! through this service, not the crate directly.

use std::sync::Arc;

use shisho_ids::{ExternalId, Provider};
use shisho_mangabaka::{MangabakaError, MangabakaProvider, SearchHit, SeriesMetadata};

#[derive(Clone)]
pub struct MetadataService {
    provider: Arc<MangabakaProvider>,
}

impl MetadataService {
    pub fn new(provider: Arc<MangabakaProvider>) -> Self {
        Self { provider }
    }

    pub fn provider(&self) -> &Arc<MangabakaProvider> {
        &self.provider
    }

    /// Resolve any known external identity to MangaBaka metadata: native
    /// `mangabaka:` ids resolve directly, every other known provider
    /// pivots through the foreign-ID columns/endpoint. `Provider::Other`
    /// returns `Ok(None)` (excluded from joins, as everywhere).
    pub async fn resolve(&self, id: &ExternalId) -> Result<Option<SeriesMetadata>, MangabakaError> {
        match &id.provider {
            Provider::Mangabaka => self.provider.get(&id.id).await,
            Provider::Other(_) => Ok(None),
            foreign => self.provider.resolve_by_foreign_id(foreign, &id.id).await,
        }
    }

    /// First match across a series' external identities, trying the native
    /// `mangabaka:` id first (cheapest, always a column hit when present).
    /// Resolution failures degrade to `None` per-id: a metadata gap, not
    /// an error for the caller's whole view.
    pub async fn resolve_any<'a, I>(&self, ids: I) -> Option<SeriesMetadata>
    where
        I: IntoIterator<Item = &'a ExternalId>,
    {
        let mut native: Vec<&ExternalId> = Vec::new();
        let mut foreign: Vec<&ExternalId> = Vec::new();
        for id in ids {
            match id.provider {
                Provider::Mangabaka => native.push(id),
                Provider::Other(_) => {}
                _ => foreign.push(id),
            }
        }
        for id in native.into_iter().chain(foreign) {
            match self.resolve(id).await {
                Ok(Some(meta)) => return Some(meta),
                Ok(None) => {}
                Err(err) => {
                    tracing::debug!(id = %format!("{}:{}", id.provider, id.id), error = %err,
                        "MangaBaka resolution failed for one identity");
                }
            }
        }
        None
    }

    /// Title search through the backbone (dump FTS, Dice re-ranked; API
    /// search in API-only mode).
    pub async fn search_title(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<SearchHit>, MangabakaError> {
        self.provider.search(query, limit).await
    }
}
