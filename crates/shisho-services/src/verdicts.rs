//! Verdict reads for the services layer. The effective-state fold and all
//! verdict queries live in `shisho_db::repos::verdicts` — one
//! implementation shared by candidate subtraction, `lookup`, the UI, and
//! reconciliation. A series is excluded from candidate pools iff its
//! folded state says the question is settled (standing verdict, recorded
//! outcome, or a failed-verification retraction); a `withdrawn`
//! retraction restores candidacy.

pub use shisho_db::repos::verdicts::{ExclusionIndex as VerdictIndex, history};
