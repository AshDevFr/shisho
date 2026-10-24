//! Verdict reads for the services layer. The effective-state fold and all
//! verdict queries live in `shisho_db::repos::verdicts` — one
//! implementation shared by candidate subtraction, `lookup`, the UI, and
//! reconciliation. A series is excluded from candidate pools iff its
//! folded state says the question is settled (standing verdict, recorded
//! outcome, or a failed-verification retraction); a `withdrawn`
//! retraction restores candidacy.
//!
//! Feedback writes go through [`record_feedback`] from every surface.

pub use shisho_db::repos::verdicts::{ExclusionIndex as VerdictIndex, history};

use sea_orm::{DatabaseConnection, DbErr};
use shisho_db::entities::verdict;
use shisho_db::repos::reconciliation;
use shisho_db::repos::verdicts::{self, NewVerdict};
use shisho_ids::ExternalId;

/// Session id and tier for feedback rows, which arrive outside any recommend
/// run.
pub const FEEDBACK_SESSION: &str = "feedback";

/// A reading outcome, in the vocabulary the web UI offers. Free text that
/// matches none of them is stored as `Other`, with the text kept in the notes,
/// so outcomes stay aggregable without losing what the user said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Loved,
    Liked,
    Mixed,
    Dropped,
    Dnf,
    Other,
}

impl Outcome {
    pub fn parse(text: &str) -> Self {
        let normalized: String = text
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        match normalized.as_str() {
            "loved" | "love" => Self::Loved,
            "liked" | "like" => Self::Liked,
            "mixed" => Self::Mixed,
            "dropped" | "drop" => Self::Dropped,
            "dnf" | "didnotfinish" => Self::Dnf,
            _ => Self::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loved => "loved",
            Self::Liked => "liked",
            Self::Mixed => "mixed",
            Self::Dropped => "dropped",
            Self::Dnf => "dnf",
            Self::Other => "other",
        }
    }
}

/// What [`record_feedback`] did.
#[derive(Debug, Clone)]
pub struct FeedbackResult {
    /// The feedback row: the new one, or the existing one on a repeat.
    pub row: verdict::Model,
    /// The same outcome and notes were already the series' latest row, so
    /// nothing was appended.
    pub deduplicated: bool,
    /// Pending reconciliation flags this outcome answered.
    pub flags_resolved: u64,
}

/// Record a reading outcome for a series: append a feedback row (unless it
/// would repeat the series' latest row, so retries never grow the log) and
/// close any pending reconciliation question on the series.
pub async fn record_feedback(
    db: &DatabaseConnection,
    user_sub: &str,
    series: &ExternalId,
    outcome: &str,
    notes: Option<String>,
) -> Result<FeedbackResult, DbErr> {
    let parsed = Outcome::parse(outcome);
    let notes = match (parsed, notes) {
        (Outcome::Other, Some(notes)) => Some(format!("{}: {notes}", outcome.trim())),
        (Outcome::Other, None) => Some(outcome.trim().to_owned()),
        (_, notes) => notes,
    };

    let history = history(db, user_sub, [series]).await?;
    let repeat = history
        .last()
        .filter(|row| row.outcome.as_deref() == Some(parsed.as_str()) && row.notes == notes)
        .cloned();
    let deduplicated = repeat.is_some();
    let row = match repeat {
        Some(row) => row,
        None => {
            verdicts::insert(
                db,
                user_sub,
                NewVerdict {
                    external_id: Some(series.clone()),
                    session_id: FEEDBACK_SESSION.into(),
                    tier: FEEDBACK_SESSION.into(),
                    rationale: "user feedback".into(),
                    verified: false,
                    retracted: false,
                    retraction_reason: None,
                    outcome: Some(parsed.as_str().to_owned()),
                    notes,
                },
            )
            .await?
        }
    };

    let flags_resolved = reconciliation::resolve_for_series(db, user_sub, [series]).await?;
    Ok(FeedbackResult {
        row,
        deduplicated,
        flags_resolved,
    })
}
