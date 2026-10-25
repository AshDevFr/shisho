//! `reconcile`: run the full per-user feedback pass for the caller now,
//! instead of waiting for their next snapshot build or the weekly sweep.
//! Re-reads their library from Codex, records what it shows (ratings set
//! after a recommendation, stalled and resumed reads), raises or closes
//! reconciliation questions, and retunes ranking if the tuning is stale.

use chrono::{DateTime, Utc};
use rmcp::handler::server::tool::Extension;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::Serialize;

use super::{CODEX_KEY_REVOKED, error_chain, tool_error, tool_json};
use crate::McpService;
use shisho_api::middleware::AuthContext;
use shisho_codex::CodexError;
use shisho_db::repos::reconciliation;
use shisho_services::PassError;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReconcileReport {
    /// When the library was read from Codex for this pass.
    library_as_of: DateTime<Utc>,
    /// Outcomes recorded (or withdrawn) from what Codex shows. Fetch them via
    /// get_profile(includeRecentFeedback: true); they carry `inferred: true`.
    outcomes_inferred: usize,
    /// Reconciliation questions newly raised (recommended series the user
    /// started reading without an outcome).
    questions_raised: usize,
    /// Questions closed because their series now has an outcome.
    questions_resolved: usize,
    /// The caller's open reconciliation questions after this pass.
    open_questions: u64,
    /// Whether ranking parameters were recomputed (they are at most weekly).
    retuned: bool,
}

#[tool_router(router = reconcile_router, vis = "pub(crate)")]
impl McpService {
    #[tool(
        name = "reconcile",
        // Re-running records nothing new: inference only writes when what
        // Codex shows differs from the log.
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        ),
        description = "Sync the feedback loop with Codex now: re-read the caller's library, record outcomes Codex shows for recommended series (a rating set after the recommendation, a read stalled for 90 days as dnf, a resumed read reopening it), raise or close reconciliation questions, and retune ranking if due. Use it when the user says they rated, finished or dropped something, or before a consolidation session. Outcomes the user recorded themselves are never changed."
    )]
    async fn reconcile(
        &self,
        Extension(auth): Extension<AuthContext>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = self.user_ctx(&auth).await?;
        let services = self.services()?;
        let (as_of, outcome) = match services
            .reconcile_now(&user.user_sub, &user.credential)
            .await
        {
            Ok(done) => done,
            Err(PassError::Codex(CodexError::Unauthorized { .. })) => {
                return Ok(tool_error(CODEX_KEY_REVOKED));
            }
            Err(err @ PassError::Codex(_)) => return Ok(tool_error(error_chain(&err))),
            Err(err @ PassError::Db(_)) => {
                return Err(ErrorData::internal_error(error_chain(&err), None));
            }
        };
        let open_questions = reconciliation::pending_count(&self.state.db, &user.user_sub)
            .await
            .map_err(|err| {
                ErrorData::internal_error(format!("reconciliation query failed: {err}"), None)
            })?;
        tool_json(&ReconcileReport {
            library_as_of: as_of,
            outcomes_inferred: outcome.reconcile.inferred,
            questions_raised: outcome.reconcile.flagged,
            questions_resolved: outcome.reconcile.resolved,
            open_questions,
            retuned: outcome.retuned.is_some(),
        })
    }
}
