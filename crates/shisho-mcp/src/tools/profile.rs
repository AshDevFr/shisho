//! `get_profile` and `propose_profile_update`: the versioned taste profile.
//! Proposals are stored pending and never change the current profile from a
//! chat session; approval is out-of-band (the web UI) — PRD Principle 6.

use chrono::{DateTime, Utc};
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::{Deserialize, Serialize};

use super::{tool_error, tool_json};
use crate::McpService;
use shisho_api::middleware::AuthContext;
use shisho_db::repos::profiles::{self, PendingProposal, ProfileError, ProfileView};
use shisho_db::repos::verdicts::verdict_series_token;
use shisho_db::repos::{reconciliation, verdicts};

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct GetProfileParams {
    /// Also return the outcomes recorded since the profile last changed — the
    /// evidence a consolidation session works from.
    pub include_recent_feedback: bool,
}

/// One recorded outcome, compact (use `lookup` for the full picture).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackView {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    recorded_at: DateTime<Utc>,
}

/// One open reconciliation question: a recommended series the user has started
/// reading without recording an outcome.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReconciliationView {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    tier: String,
    recommended_at: DateTime<Utc>,
    detected_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileResponse {
    #[serde(flatten)]
    profile: ProfileView,
    #[serde(skip_serializing_if = "Option::is_none")]
    recent_feedback: Option<Vec<FeedbackView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_reconciliation: Option<Vec<ReconciliationView>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProposeProfileUpdateParams {
    /// The full proposed profile content (Markdown).
    pub diff: String,
    /// One-line summary of what changed and why.
    pub summary: String,
    /// The profile version this proposal was drafted against (from get_profile).
    /// A mismatch with the current version is rejected, so a proposal built on a
    /// stale read is never stored.
    #[serde(default)]
    pub base_version: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProposalAck {
    #[serde(flatten)]
    proposal: PendingProposal,
    /// Proposals never change the current profile from a chat session; approval
    /// happens out-of-band (web UI).
    status: &'static str,
}

#[tool_router(router = profile_router, vis = "pub(crate)")]
impl McpService {
    #[tool(
        name = "get_profile",
        annotations(read_only_hint = true),
        description = "The caller's current taste profile (Markdown) with its version. New users get a well-defined empty profile at version 0. Read this before recommending; it is the standing description of what the user likes, hates, and is allergic to. Set includeRecentFeedback to also get the outcomes recorded since the profile last changed (consolidation evidence) plus pendingReconciliation: recommended series the user has started reading without recording an outcome — ask for those outcomes and record them via record_feedback."
    )]
    async fn get_profile(
        &self,
        Parameters(params): Parameters<GetProfileParams>,
        Extension(auth): Extension<AuthContext>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = self.user_ctx(&auth).await?;
        let profile = profiles::get_or_default(&self.state.db, &user.user_sub)
            .await
            .map_err(db_error)?;

        let (recent_feedback, pending_reconciliation) = if params.include_recent_feedback {
            let rows =
                verdicts::recent_outcomes(&self.state.db, &user.user_sub, profile.updated_at)
                    .await
                    .map_err(db_error)?;
            let feedback = rows
                .into_iter()
                .map(|row| FeedbackView {
                    id: verdict_series_token(&row),
                    outcome: row.outcome.unwrap_or_default(),
                    notes: row.notes,
                    recorded_at: row.created_at.to_utc(),
                })
                .collect();

            let pending = reconciliation::pending_with_verdicts(&self.state.db, &user.user_sub)
                .await
                .map_err(db_error)?
                .into_iter()
                .map(|(flag, row)| ReconciliationView {
                    id: verdict_series_token(&row),
                    tier: row.tier,
                    recommended_at: row.created_at.to_utc(),
                    detected_at: flag.detected_at.to_utc(),
                })
                .collect();
            (Some(feedback), Some(pending))
        } else {
            (None, None)
        };

        tool_json(&ProfileResponse {
            profile,
            recent_feedback,
            pending_reconciliation,
        })
    }

    #[tool(
        name = "propose_profile_update",
        // Stores a pending proposal; never mutates the live profile (approval is
        // out-of-band), so additive rather than destructive.
        annotations(read_only_hint = false, destructive_hint = false),
        description = "Propose an update to the taste profile (e.g. after consolidating recent feedback). Pass the full proposed Markdown content as 'diff', a one-line 'summary', and the baseVersion you read via get_profile (a stale base is rejected). The proposal is stored pending; it never becomes the current profile until the user approves it out-of-band."
    )]
    async fn propose_profile_update(
        &self,
        Parameters(params): Parameters<ProposeProfileUpdateParams>,
        Extension(auth): Extension<AuthContext>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = self.user_ctx(&auth).await?;
        let proposal = match profiles::propose(
            &self.state.db,
            &user.user_sub,
            &params.diff,
            &params.summary,
            params.base_version,
        )
        .await
        {
            Ok(proposal) => proposal,
            // Stale base is the model's mistake to correct: tool-level.
            Err(err @ ProfileError::StaleBase { .. }) => return Ok(tool_error(err.to_string())),
            Err(ProfileError::Db(err)) => return Err(db_error(err)),
            Err(err) => return Err(ErrorData::internal_error(err.to_string(), None)),
        };
        tool_json(&ProposalAck {
            proposal,
            status: "pending",
        })
    }
}

fn db_error(err: sea_orm::DbErr) -> ErrorData {
    ErrorData::internal_error(format!("profile query failed: {err}"), None)
}
