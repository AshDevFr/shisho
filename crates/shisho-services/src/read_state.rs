//! Read-state derivation from Codex per-user counts (PRD §2.5).
//!
//! `unread == total` is the only trustworthy "never read" signal Codex
//! gives us. Note the reads-to-~90% pattern: a series sitting just short of
//! complete is near-complete, not stalled — `InProgress` covers both and
//! the model phrases progress from the live counts, not from this enum.

use serde::{Deserialize, Serialize};

/// A series' read progress.
//
// Schema note: variant docs are plain `//` comments, and the type is
// `#[schemars(inline)]`, on purpose. Per-variant `///` docs make schemars
// emit a `oneOf` of `const`s, and the claude.ai tool-input serializer drops
// such enum values to `null`; a flat, inline `{type: string, enum: [...]}`
// is what every client handles. Keep the human descriptions as `//`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub enum ReadState {
    // Never opened: every book is unread.
    Untouched,
    // Everything read.
    Completed,
    // Anything in between (including near-complete).
    InProgress,
}

impl ReadState {
    /// Derive from Codex's per-series book counts. `unread == total` is the
    /// only condition allowed to claim `Untouched`; a missing `unreadCount`
    /// proves nothing, so it lands in `InProgress` (the "otherwise" bucket)
    /// rather than risking an already-read series in the untouched pool.
    /// Defensive bounds: counts are clamped so `unread > total` (a Codex
    /// inconsistency) still derives `Untouched` rather than panicking or
    /// inventing progress.
    pub fn derive(total: i64, unread: Option<i64>) -> Self {
        let total = total.max(0);
        let Some(unread) = unread else {
            return ReadState::InProgress;
        };
        let unread = unread.clamp(0, total);
        if total == 0 {
            // An empty series carries no read signal; nothing has been
            // read because nothing exists.
            return ReadState::Untouched;
        }
        if unread == total {
            ReadState::Untouched
        } else if unread == 0 {
            ReadState::Completed
        } else {
            ReadState::InProgress
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_a_flat_inline_string_enum_not_oneof() {
        // claude.ai's tool-input serializer drops `oneOf`/`const` enums to
        // null; the schema must be a flat string enum.
        let schema = serde_json::to_value(schemars::schema_for!(ReadState)).unwrap();
        assert!(
            schema.get("oneOf").is_none(),
            "ReadState must not use oneOf: {schema}"
        );
        assert_eq!(schema["type"], "string");
        let values: Vec<&str> = schema["enum"]
            .as_array()
            .expect("flat enum array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(values, ["untouched", "completed", "inProgress"]);
    }

    #[test]
    fn derivation_table() {
        // (total, unread, expected)
        let table: &[(i64, Option<i64>, ReadState)] = &[
            // The PRD §2.5 mapping.
            (10, Some(10), ReadState::Untouched),
            (10, Some(0), ReadState::Completed),
            (10, Some(3), ReadState::InProgress),
            // Near-complete is in-progress, not stalled/completed.
            (10, Some(1), ReadState::InProgress),
            // Single-book series.
            (1, Some(1), ReadState::Untouched),
            (1, Some(0), ReadState::Completed),
            // Empty series: no signal, honest default.
            (0, Some(0), ReadState::Untouched),
            // Missing unread count proves nothing: never claim untouched
            // without the one trustworthy signal.
            (10, None, ReadState::InProgress),
            (0, None, ReadState::InProgress),
            // Defensive: unread > total clamps to untouched.
            (5, Some(9), ReadState::Untouched),
            // Defensive: negative values clamp instead of corrupting state.
            (5, Some(-1), ReadState::Completed),
            (-3, Some(2), ReadState::Untouched),
        ];
        for &(total, unread, expected) in table {
            assert_eq!(
                ReadState::derive(total, unread),
                expected,
                "total={total} unread={unread:?}"
            );
        }
    }
}
