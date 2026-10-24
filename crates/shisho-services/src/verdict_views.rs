//! Display enrichment for verdict-log rows: series title, summary, cover, and
//! deep links into MangaBaka, Codex and Tsundoku. Every field is best-effort;
//! a metadata gap, an unowned series, or an unreachable backend leaves the
//! corresponding field absent rather than failing the view.

use crate::Services;
use crate::library_snapshot::CodexIndex;
use shisho_ids::ExternalId;

/// What one verdict row's series renders with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerdictLinks {
    pub title: Option<String>,
    pub cover_url: Option<String>,
    pub summary: Option<String>,
    pub mangabaka_url: Option<String>,
    pub codex_url: Option<String>,
    pub tsundoku_url: Option<String>,
}

/// The public MangaBaka series URL for a numeric id.
pub fn mangabaka_public_url(id: &str) -> String {
    format!("https://mangabaka.org/{id}")
}

/// Enrich each row's series token (`provider:id`, or `None` for rows without
/// a series identity), returning one [`VerdictLinks`] per input in order.
/// Metadata lookups run concurrently; Tsundoku links resolve in one batched
/// feed pass; Codex links come from the caller's cached external index.
pub async fn verdict_links(
    services: &Services,
    codex_index: &CodexIndex,
    series: &[Option<String>],
) -> Vec<VerdictLinks> {
    let parsed: Vec<Option<ExternalId>> = series
        .iter()
        .map(|token| {
            token
                .as_deref()
                .and_then(|t| ExternalId::from_tsundoku(t).ok())
        })
        .collect();

    let metas = futures_util::future::join_all(parsed.iter().map(|p| async {
        match p {
            Some(eid) => services.metadata.resolve(eid).await.ok().flatten(),
            None => None,
        }
    }))
    .await;

    let tokens: Vec<String> = parsed
        .iter()
        .flatten()
        .filter_map(ExternalId::as_tsundoku)
        .collect();
    let tsundoku = match services.tsundoku.resolve_external_ids(&tokens).await {
        Ok(map) => map,
        Err(err) => {
            tracing::debug!(error = %err, "tsundoku feed resolution failed; tsundoku links omitted");
            Default::default()
        }
    };
    let codex_base = services.snapshots.codex().base_url().trim_end_matches('/');
    let tsundoku_base = services.tsundoku.client().base_url().trim_end_matches('/');

    series
        .iter()
        .zip(parsed)
        .zip(metas)
        .map(|((token, eid), meta)| {
            let mut links = VerdictLinks::default();
            if let Some(meta) = meta {
                links.title = Some(meta.title);
                links.cover_url = meta.cover_url;
                links.summary = meta.description;
            }
            if let Some(id) = token.as_deref().and_then(|t| t.strip_prefix("mangabaka:")) {
                links.mangabaka_url = Some(mangabaka_public_url(id));
            }
            if let Some(eid) = eid {
                let key = eid.canonical_key();
                if !codex_base.is_empty()
                    && let Some(uuid) = codex_index.get(&key)
                {
                    links.codex_url = Some(format!("{codex_base}/series/{uuid}"));
                }
                if !tsundoku_base.is_empty()
                    && let Some(resolved) = tsundoku.get(&key)
                {
                    links.tsundoku_url =
                        Some(format!("{tsundoku_base}/series/{}", resolved.series_id));
                }
            }
            links
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn mangabaka_public_url_is_the_org_site() {
        assert_eq!(
            super::mangabaka_public_url("9741"),
            "https://mangabaka.org/9741"
        );
    }
}
