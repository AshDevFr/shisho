//! `record_verdict` and `record_feedback`: appends to the verdict log.
//!
//! Feedback records the outcome on the local verdict log (consolidation
//! evidence) and closes any reconciliation flag it answers. A rating and
//! notes reach the user's Codex library only when the call opts in with
//! `pushToCodex`, after the outcome is saved, so a failed push never loses
//! the outcome: it comes back as a `codexGap` to relay.

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::{Deserialize, Serialize};

use super::{tool_error, tool_json};
use crate::McpService;
use shisho_api::middleware::AuthContext;
use shisho_db::repos::verdicts::{self, NewVerdict, RetractionReason};
use shisho_services::lookup::parse_known_id;
use shisho_services::ratings::{self, PushGap, PushOutcome, RatingPush};

/// Why a supplied rating went nowhere: Shisho keeps no ratings of its own.
const RATING_NOT_PUSHED_GAP: &str = "the rating was not saved: Shisho keeps ratings only in Codex, and \
     pushToCodex was not set. Ask the user whether to write it to Codex, then retry with \
     pushToCodex: true.";

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
    /// What happened: "loved", "liked", "mixed", "dropped", "dnf" (did not
    /// finish), or "not interested" (turned down without reading). "reopened"
    /// withdraws an earlier outcome. Other text is stored as "other" with the
    /// text kept in notes.
    pub outcome: String,
    /// Rating for the series, 1-100 (Codex displays 1-10 with one decimal).
    /// Only kept when written to Codex with pushToCodex.
    #[serde(default)]
    pub rating: Option<i32>,
    /// Free-form notes on why. Saved with the outcome; also written as the
    /// Codex rating notes with pushToCodex.
    #[serde(default)]
    pub notes: Option<String>,
    /// Write the rating and notes to the user's Codex library. Set it only
    /// when the user asked for that.
    #[serde(default)]
    pub push_to_codex: bool,
    /// With pushToCodex, replace a different rating or notes already in
    /// Codex. Set it only after the user confirmed replacing what the
    /// codexGap reported.
    #[serde(default)]
    pub overwrite_codex: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackAck {
    verdict_id: i64,
    outcome: String,
    /// Whether the rating/notes reached Codex (or Codex already held them).
    /// The outcome is recorded locally regardless.
    pushed_to_codex: bool,
    /// Why the Codex push did not happen, when a rating was supplied. Relay it
    /// to the user; the outcome above is saved either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_gap: Option<String>,
    /// Pending reconciliation flags this outcome answered, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    reconciliation_flags_resolved: Option<u64>,
}

#[tool_router(router = verdicts_router, vis = "pub(crate)")]
impl McpService {
    #[tool(
        name = "record_verdict",
        // Append-only: writes, but never destroys (a retraction is a new row).
        annotations(read_only_hint = false, destructive_hint = false),
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
        // Additive and idempotent: re-recording the same outcome is a no-op.
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "Record the user's reading outcome for a series ('loved', 'dropped', ...). The outcome and notes are stored on the local verdict log (consolidation evidence) and close any open reconciliation question on the series. To also write a rating (1-100) and the notes to the user's Codex library, set pushToCodex: true, only when the user asked for it. A different rating or notes already in Codex are not replaced unless overwriteCodex is also set: relay the codexGap and ask the user first. The ack's pushedToCodex says whether Codex holds them; codexGap says why not."
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

        // Local outcome first, always, and idempotently: retrying the same
        // feedback must not grow the log. The outcome also answers any
        // pending reconciliation question on the series.
        let ack = shisho_services::verdicts::record_feedback(
            &self.state.db,
            &user.user_sub,
            &external_id,
            &params.outcome,
            params.notes.clone(),
        )
        .await
        .map_err(db_error)?;

        let (pushed_to_codex, codex_gap) = if params.push_to_codex {
            let push = RatingPush {
                rating: params.rating,
                notes: params.notes,
                overwrite: params.overwrite_codex,
            };
            match self.push_rating(&user, &external_id, push).await? {
                PushOutcome::Written { .. } | PushOutcome::Unchanged => (true, None),
                PushOutcome::Skipped(gap) => (false, Some(gap.message())),
            }
        } else {
            (
                false,
                params.rating.map(|_| RATING_NOT_PUSHED_GAP.to_owned()),
            )
        };

        tool_json(&FeedbackAck {
            verdict_id: ack.row.id,
            outcome: ack.row.outcome.unwrap_or_default(),
            pushed_to_codex,
            codex_gap,
            reconciliation_flags_resolved: (ack.flags_resolved > 0).then_some(ack.flags_resolved),
        })
    }
}

impl McpService {
    /// Push a rating to Codex against the caller's library snapshot (the
    /// series must be in it to have a Codex id to write to).
    async fn push_rating(
        &self,
        user: &shisho_codex::UserCtx,
        series: &shisho_ids::ExternalId,
        push: RatingPush,
    ) -> Result<PushOutcome, ErrorData> {
        let services = self.services()?;
        let snapshot = match services
            .snapshots
            .get(&user.user_sub, &user.credential)
            .await
        {
            Ok(hit) => hit.value,
            Err(err) => return Ok(PushOutcome::Skipped(PushGap::Codex(err.to_string()))),
        };
        Ok(ratings::push(
            services.snapshots.codex(),
            &user.credential,
            &snapshot,
            series,
            push,
        )
        .await)
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
