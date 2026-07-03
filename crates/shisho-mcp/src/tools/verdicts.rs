//! `record_verdict` and `record_feedback`: appends to the verdict log.
//!
//! Feedback records the outcome on the local verdict log (consolidation
//! evidence) and closes any reconciliation flag it answers. The Codex
//! rating push is deferred (N-RATINGWRITE): Codex currently gates rating-write
//! under `series:read`, so the Shisho-minted key is read-only and pushing a
//! rating would leak write access. The ack keeps `pushedToCodex`/`codexGap`
//! stable so re-enabling the push (once Codex separates that permission) is a
//! one-line change; for now `pushedToCodex` is always false and a rating is
//! recorded locally with the gap explained.

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::{Deserialize, Serialize};

use super::{tool_error, tool_json};
use crate::McpService;
use shisho_api::middleware::AuthContext;
use shisho_db::repos::reconciliation;
use shisho_db::repos::verdicts::{self, NewVerdict, RetractionReason};
use shisho_services::lookup::parse_known_id;

/// Why a supplied rating did not reach Codex. Recorded locally regardless.
const RATING_DEFERRED_GAP: &str = "the rating was recorded in Shisho but not pushed to Codex: rating-write is not yet enabled \
     for the Shisho-minted key (Codex gates it under series:read). The outcome is saved as \
     consolidation evidence; the rating will sync once Codex separates that permission.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordVerdictParams {
    /// The series' external ID in any known notation (e.g. "mangabaka:112092",
    /// "mal:126287", "plugin:mangabaka:112092").
    pub id: String,
    /// Recommendation tier (e.g. "S", "A", "B", "pass").
    pub tier: String,
    /// Why: the reasoning behind this verdict, tied to the taste profile.
    pub rationale: String,
    /// Whether the verification pass (allergens, red flags) was completed.
    pub verified: bool,
    /// Groups the verdicts of one recommend run; generated when omitted.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Record as retracted; requires retractionReason.
    #[serde(default)]
    pub retracted: bool,
    /// Why the retraction: "failedVerification" (verified and rejected — the
    /// series stays excluded from candidate pools for good) or "withdrawn" (a
    /// session's surplus taken back — the series returns to candidacy).
    #[serde(default)]
    pub retraction_reason: Option<RetractionReasonParam>,
    /// Free-form notes.
    #[serde(default)]
    pub notes: Option<String>,
}

/// MCP-facing mirror of [`RetractionReason`] (camelCase wire values).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RetractionReasonParam {
    FailedVerification,
    Withdrawn,
}

impl From<RetractionReasonParam> for RetractionReason {
    fn from(param: RetractionReasonParam) -> Self {
        match param {
            RetractionReasonParam::FailedVerification => RetractionReason::FailedVerification,
            RetractionReasonParam::Withdrawn => RetractionReason::Withdrawn,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VerdictAck {
    verdict_id: i64,
    session_id: String,
    retracted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    retraction_reason: Option<RetractionReasonParam>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordFeedbackParams {
    /// The series' external ID in any known notation.
    pub id: String,
    /// What happened (e.g. "loved", "liked", "dropped", "mixed").
    pub outcome: String,
    /// Rating for the series, 1-100 (Codex displays 1-10 with one decimal).
    /// Recorded as evidence; the Codex push is deferred (see codexGap).
    #[serde(default)]
    pub rating: Option<i32>,
    /// Free-form notes on why.
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackAck {
    verdict_id: i64,
    outcome: String,
    /// Whether the rating/notes reached Codex. Always false for now (the push
    /// is deferred); the outcome is recorded locally regardless.
    pushed_to_codex: bool,
    /// Why the Codex push did not happen, when a rating was supplied. Relay it
    /// to the user; the outcome above is saved either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_gap: Option<String>,
    /// Pending reconciliation flags this outcome answered, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    reconciliation_flags_resolved: Option<u64>,
}

/// Session ID for feedback rows, which arrive outside any recommend run.
const FEEDBACK_SESSION: &str = "feedback";

#[tool_router(router = verdicts_router, vis = "pub(crate)")]
impl McpService {
    #[tool(
        name = "record_verdict",
        description = "Append one recommendation verdict to the log. Record a verdict for EVERY candidate surfaced to the user, including rejections (tier 'pass') and candidates that failed verification (retracted: true, retractionReason: 'failedVerification'). The log is append-only: to retract an earlier verdict, record a new one with retracted: true and a retractionReason — 'failedVerification' keeps the series excluded from future pools, 'withdrawn' returns it to candidacy."
    )]
    async fn record_verdict(
        &self,
        Parameters(params): Parameters<RecordVerdictParams>,
        Extension(auth): Extension<AuthContext>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = self.user_ctx(&auth).await?;
        let Some(external_id) = parse_known_id(&params.id) else {
            return Ok(unknown_id_error(&params.id));
        };
        if params.retracted && params.retraction_reason.is_none() {
            return Ok(tool_error(
                "a retraction needs retractionReason: 'failedVerification' (verified and \
                 rejected; stays excluded from candidate pools) or 'withdrawn' (session \
                 surplus taken back; returns to candidacy)",
            ));
        }
        if !params.retracted && params.retraction_reason.is_some() {
            return Ok(tool_error(
                "retractionReason only applies to a retraction (retracted: true)",
            ));
        }
        let session_id = params
            .session_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let row = verdicts::insert(
            &self.state.db,
            &user.user_sub,
            NewVerdict {
                external_id: Some(external_id),
                session_id: session_id.clone(),
                tier: params.tier,
                rationale: params.rationale,
                verified: params.verified,
                retracted: params.retracted,
                retraction_reason: params.retraction_reason.map(Into::into),
                outcome: None,
                notes: params.notes,
            },
        )
        .await
        .map_err(db_error)?;

        tool_json(&VerdictAck {
            verdict_id: row.id,
            session_id,
            retracted: row.retracted,
            retraction_reason: params.retraction_reason,
        })
    }

    #[tool(
        name = "record_feedback",
        description = "Record the user's reading outcome for a series ('loved', 'dropped', ...). The outcome is stored on the local verdict log (consolidation evidence) and closes any open reconciliation question on the series. A rating (1-100) is recorded as evidence; pushing it to the user's Codex library is deferred, so the ack carries pushedToCodex: false plus a codexGap to relay."
    )]
    async fn record_feedback(
        &self,
        Parameters(params): Parameters<RecordFeedbackParams>,
        Extension(auth): Extension<AuthContext>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = self.user_ctx(&auth).await?;
        let Some(external_id) = parse_known_id(&params.id) else {
            return Ok(unknown_id_error(&params.id));
        };
        if let Some(rating) = params.rating
            && !(1..=100).contains(&rating)
        {
            return Ok(tool_error(format!(
                "rating must be 1-100 (Codex's scale; 10 display units × 10), got {rating}"
            )));
        }

        // Local outcome first — always, and idempotently: retrying the same
        // feedback must not grow the log.
        let history = verdicts::history(&self.state.db, &user.user_sub, [&external_id])
            .await
            .map_err(db_error)?;
        let duplicate = history.last().filter(|row| {
            row.outcome.as_deref() == Some(params.outcome.as_str()) && row.notes == params.notes
        });
        let verdict_id = match duplicate {
            Some(row) => row.id,
            None => {
                verdicts::insert(
                    &self.state.db,
                    &user.user_sub,
                    NewVerdict {
                        external_id: Some(external_id.clone()),
                        session_id: FEEDBACK_SESSION.into(),
                        tier: "feedback".into(),
                        rationale: "user feedback".into(),
                        verified: false,
                        retracted: false,
                        retraction_reason: None,
                        outcome: Some(params.outcome.clone()),
                        notes: params.notes.clone(),
                    },
                )
                .await
                .map_err(db_error)?
                .id
            }
        };

        // An outcome answers any pending reconciliation question on the series.
        let flags_resolved =
            reconciliation::resolve_for_series(&self.state.db, &user.user_sub, [&external_id])
                .await
                .map_err(db_error)?;

        // The Codex rating push stays deferred (N-RATINGWRITE): name the gap
        // when a rating was supplied, but never lose the outcome.
        let codex_gap = params.rating.map(|_| RATING_DEFERRED_GAP.to_owned());

        tool_json(&FeedbackAck {
            verdict_id,
            outcome: params.outcome,
            pushed_to_codex: false,
            codex_gap,
            reconciliation_flags_resolved: (flags_resolved > 0).then_some(flags_resolved),
        })
    }
}

fn unknown_id_error(input: &str) -> CallToolResult {
    tool_error(format!(
        "{input:?} did not parse to a known provider; verdicts need a canonical identity \
         (e.g. \"mangabaka:112092\"). Use lookup to resolve a title first."
    ))
}

fn db_error(err: sea_orm::DbErr) -> ErrorData {
    ErrorData::internal_error(format!("verdict write failed: {err}"), None)
}
