//! `status()`: backend reachability, cache freshness, per-pool readiness,
//! unresolvable series, and sweep state — so the model can qualify staleness
//! and degradation honestly instead of guessing.

use chrono::{DateTime, Utc};
use rmcp::handler::server::tool::Extension;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::Serialize;

use super::{CODEX_KEY_REVOKED, swr_codex_unauthorized, tool_json};
use crate::McpService;
use shisho_api::middleware::AuthContext;
use shisho_db::repos::reconciliation;
use shisho_services::health::{self, HealthResponse};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusReport {
    /// db, per-backend reachability, cache last-warm.
    health: HealthResponse,
    pools: PoolsStatus,
    /// The caller's library snapshot view; absent when it could not be built
    /// (the failure is then named in `gaps`).
    #[serde(skip_serializing_if = "Option::is_none")]
    library: Option<LibraryStatus>,
    /// Named failures (never silent absences).
    gaps: Vec<String>,
    /// Housekeeping-sweep state: last runs and the caller's open reconciliation
    /// questions.
    sweeps: SweepsStatus,
}

/// In-process housekeeping sweeps (reconciliation, unresolvable scan).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SweepsStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    reconcile_last_ran_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unresolvable_last_ran_at: Option<DateTime<Utc>>,
    /// The caller's open reconciliation questions (recommended series they
    /// started reading without recording an outcome). Fetch them via
    /// get_profile(includeRecentFeedback: true).
    pending_reconciliation_count: u64,
}

/// Plain-language readiness per candidate pool.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PoolsStatus {
    library: String,
    acquirable: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LibraryStatus {
    as_of: DateTime<Utc>,
    series_count: usize,
    /// Series whose external IDs map to no known provider: they cannot join
    /// with Tsundoku. The fix is Codex's metadata-match flow (linked), never a
    /// local patch.
    unresolvable_count: usize,
    unresolvable: Vec<UnresolvableEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UnresolvableEntry {
    series_id: String,
    title: String,
    /// The raw `source:externalId` strings that failed to map, verbatim.
    raw_sources: Vec<String>,
    /// Deep link to the series in Codex, where metadata matching is fixed.
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_url: Option<String>,
}

#[tool_router(router = status_router, vis = "pub(crate)")]
impl McpService {
    #[tool(
        name = "status",
        description = "Service health from the caller's perspective: backend reachability, per-pool readiness, library snapshot age, series that cannot be matched to the catalog (with deep links to fix them in Codex), and housekeeping-sweep state (last runs, the caller's open reconciliation questions). Call this when data seems stale or a pool errored, and qualify recommendations accordingly."
    )]
    async fn status(
        &self,
        Extension(auth): Extension<AuthContext>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = self.user_ctx(&auth).await?;
        let services = self.services()?;
        let health = health::probe(&self.state.db, services).await;
        let mut gaps = Vec::new();

        let library = match services
            .snapshots
            .get(&user.user_sub, &user.credential)
            .await
        {
            Ok(hit) => {
                let codex_base = services.snapshots.codex().base_url().to_owned();
                let snapshot = hit.value;
                Some(LibraryStatus {
                    as_of: hit.as_of,
                    series_count: snapshot.series.len(),
                    unresolvable_count: snapshot.unresolvable.len(),
                    unresolvable: snapshot
                        .unresolvable
                        .iter()
                        .map(|u| UnresolvableEntry {
                            series_id: u.series_id.clone(),
                            title: u.title.clone(),
                            raw_sources: u.raw_sources.clone(),
                            codex_url: (!codex_base.is_empty())
                                .then(|| format!("{codex_base}/series/{}", u.series_id)),
                        })
                        .collect(),
                })
            }
            Err(err) if swr_codex_unauthorized(&err) => {
                gaps.push(format!("library snapshot: {CODEX_KEY_REVOKED}"));
                None
            }
            Err(err) => {
                gaps.push(format!("library snapshot unavailable: {err}"));
                None
            }
        };

        let pools = PoolsStatus {
            library: if health.codex.reachable {
                "ok".into()
            } else {
                "unavailable: Codex (the required tier) is unreachable".into()
            },
            acquirable: if !health.tsundoku.configured {
                "not configured (Tier 0: Codex-only deployment)".into()
            } else if health.tsundoku.reachable {
                "ok".into()
            } else {
                "degraded: Tsundoku is unreachable".into()
            },
        };

        let pending_reconciliation_count =
            reconciliation::pending_count(&self.state.db, &user.user_sub)
                .await
                .map_err(|err| {
                    ErrorData::internal_error(format!("reconciliation query failed: {err}"), None)
                })?;

        tool_json(&StatusReport {
            health,
            pools,
            library,
            gaps,
            sweeps: SweepsStatus {
                reconcile_last_ran_at: services.sweeps.reconcile_last_run(),
                unresolvable_last_ran_at: services.sweeps.unresolvable_last_run(),
                pending_reconciliation_count,
            },
        })
    }
}
