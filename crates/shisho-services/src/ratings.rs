//! Writing the user's rating and notes back to Codex, which owns them.
//!
//! Only ever on an explicit request (the feedback surfaces carry an opt-in
//! flag), and never silently over what the user already wrote in Codex: a
//! different existing rating or note is reported back as a conflict, and only
//! replaced when the caller asks to overwrite. Codex keeps no rating history,
//! so an overwrite is the one destructive write Shisho makes.
//!
//! The push needs `ratings:write` on the user's Shisho-minted key. Keys minted
//! before Shisho asked for it, or minted read-only because the authorizing
//! credential could not grant it, get a 403, which is reported as a gap
//! naming the fix rather than as a failure of the feedback itself.

use shisho_codex::{CodexClient, CodexCredential, CodexError, SetRating};
use shisho_ids::ExternalId;

use crate::library_snapshot::LibrarySnapshot;

/// What a push request asks Codex to hold.
#[derive(Debug, Clone, Default)]
pub struct RatingPush {
    /// 1-100. `None` keeps the rating Codex already has (a notes-only push).
    pub rating: Option<i32>,
    /// `None` keeps the notes Codex already has.
    pub notes: Option<String>,
    /// Replace a different rating or notes already in Codex.
    pub overwrite: bool,
}

/// What [`push`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// Codex now holds this rating and these notes.
    Written { rating: i32, notes: Option<String> },
    /// Codex already held exactly this; nothing was sent.
    Unchanged,
    /// Nothing was written, for this reason.
    Skipped(PushGap),
}

/// Why a push wrote nothing. [`PushGap::message`] is phrased for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushGap {
    /// The series is not in the user's Codex library, so it has nothing to
    /// rate.
    NotInLibrary,
    /// Notes were given without a rating, and Codex has no rating to keep:
    /// Codex stores notes only alongside a rating.
    NeedsRating,
    /// Codex already holds a different rating or notes, and the request did
    /// not ask to overwrite them.
    Conflict { rating: i32, notes: Option<String> },
    /// The user's Codex key lacks `ratings:write`.
    MissingPermission,
    /// Codex failed the read or the write.
    Codex(String),
}

impl PushGap {
    pub fn message(&self) -> String {
        match self {
            PushGap::NotInLibrary => {
                "not written to Codex: the series is not in the user's Codex library".into()
            }
            PushGap::NeedsRating => "not written to Codex: Codex stores notes only with a \
                 rating, and the series has none there; give a rating too"
                .into(),
            PushGap::Conflict { rating, notes } => {
                let notes = match notes {
                    Some(notes) => format!(" with notes {notes:?}"),
                    None => String::new(),
                };
                format!(
                    "not written to Codex: it already holds rating {rating}/100{notes}. Ask the \
                     user, then retry with overwriteCodex to replace it"
                )
            }
            PushGap::MissingPermission => "not written to Codex: the user's Codex key lacks the \
                 ratings:write permission. Regenerate the key from the Codex key panel on \
                 Shisho's Status page, or add the permission to the key in Codex"
                .into(),
            PushGap::Codex(err) => format!("not written to Codex: {err}"),
        }
    }
}

/// Write `request` to the Codex rating of the library series carrying
/// `series`. Reads the live Codex rating first (the snapshot's copy may be
/// minutes old) to keep existing notes and to refuse an unrequested
/// overwrite. Never errors: every failure is a [`PushGap`].
pub async fn push(
    codex: &CodexClient,
    credential: &CodexCredential,
    snapshot: &LibrarySnapshot,
    series: &ExternalId,
    request: RatingPush,
) -> PushOutcome {
    let Some(owned) = snapshot.series_by_external_id(series) else {
        return PushOutcome::Skipped(PushGap::NotInLibrary);
    };
    let current = match codex.series_rating(credential, &owned.id).await {
        Ok(current) => current,
        Err(err) => return PushOutcome::Skipped(gap(err)),
    };

    if let Some(current) = &current {
        let rating_differs = request.rating.is_some_and(|r| r != current.rating);
        let notes_differ =
            request.notes.is_some() && current.notes.is_some() && request.notes != current.notes;
        if (rating_differs || notes_differ) && !request.overwrite {
            return PushOutcome::Skipped(PushGap::Conflict {
                rating: current.rating,
                notes: current.notes.clone(),
            });
        }
    }

    // Codex replaces both fields on write, so whatever the request leaves out
    // is carried over from what Codex holds.
    let Some(rating) = request.rating.or(current.as_ref().map(|c| c.rating)) else {
        return PushOutcome::Skipped(PushGap::NeedsRating);
    };
    let notes = request
        .notes
        .or_else(|| current.as_ref().and_then(|c| c.notes.clone()));
    if current
        .as_ref()
        .is_some_and(|c| c.rating == rating && c.notes == notes)
    {
        return PushOutcome::Unchanged;
    }

    let body = SetRating {
        rating,
        notes: notes.clone(),
    };
    match codex.set_series_rating(credential, &owned.id, &body).await {
        Ok(_) => PushOutcome::Written { rating, notes },
        Err(err) => PushOutcome::Skipped(gap(err)),
    }
}

fn gap(err: CodexError) -> PushGap {
    if err.is_forbidden() {
        PushGap::MissingPermission
    } else {
        PushGap::Codex(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library_snapshot::SnapshotSeries;
    use crate::read_state::ReadState;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SERIES: &str = "s-1";

    fn id() -> ExternalId {
        ExternalId::from_tsundoku("mangabaka:42").unwrap()
    }

    fn snapshot() -> LibrarySnapshot {
        LibrarySnapshot::from_series(vec![SnapshotSeries {
            id: SERIES.into(),
            library_id: "lib".into(),
            library_name: "Manga".into(),
            title: "A Series".into(),
            summary: None,
            status: None,
            year: None,
            genres: Vec::new(),
            tags: Vec::new(),
            authors: Vec::new(),
            book_count: 3,
            unread_count: Some(0),
            read_state: ReadState::Completed,
            external_ids: vec![id()],
            rating: None,
            rating_notes: None,
            rating_updated_at: None,
        }])
    }

    fn credential() -> CodexCredential {
        CodexCredential::ApiKey("codex_key".into())
    }

    fn rating_body(rating: i32, notes: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "id": "r-1", "seriesId": SERIES, "rating": rating, "notes": notes,
            "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z"
        })
    }

    /// Codex's current rating for the series: `None` answers 204.
    async fn current(server: &MockServer, existing: Option<(i32, Option<&str>)>) {
        let response = match existing {
            Some((rating, notes)) => {
                ResponseTemplate::new(200).set_body_json(rating_body(rating, notes))
            }
            None => ResponseTemplate::new(204),
        };
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/series/{SERIES}/rating")))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn expect_put(server: &MockServer, body: serde_json::Value, times: u64) {
        let rating = body["rating"].as_i64().unwrap_or(0) as i32;
        let notes = body["notes"].as_str().map(str::to_owned);
        Mock::given(method("PUT"))
            .and(path(format!("/api/v1/series/{SERIES}/rating")))
            .and(body_json(body))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(rating_body(rating, notes.as_deref())),
            )
            .expect(times)
            .mount(server)
            .await;
    }

    async fn run(server: &MockServer, request: RatingPush) -> PushOutcome {
        let codex = CodexClient::new(&server.uri(), reqwest::Client::new());
        push(&codex, &credential(), &snapshot(), &id(), request).await
    }

    fn rate(rating: i32, notes: Option<&str>) -> RatingPush {
        RatingPush {
            rating: Some(rating),
            notes: notes.map(str::to_owned),
            overwrite: false,
        }
    }

    #[tokio::test]
    async fn writes_a_first_rating_with_notes() {
        let server = MockServer::start().await;
        current(&server, None).await;
        expect_put(
            &server,
            serde_json::json!({"rating": 90, "notes": "great"}),
            1,
        )
        .await;

        let out = run(&server, rate(90, Some("great"))).await;
        assert_eq!(
            out,
            PushOutcome::Written {
                rating: 90,
                notes: Some("great".into())
            }
        );
    }

    #[tokio::test]
    async fn a_rating_alone_keeps_the_notes_codex_has() {
        // Codex replaces both fields, so the existing notes are sent back.
        let server = MockServer::start().await;
        current(&server, Some((80, Some("mine")))).await;
        expect_put(
            &server,
            serde_json::json!({"rating": 90, "notes": "mine"}),
            1,
        )
        .await;

        let out = run(
            &server,
            RatingPush {
                overwrite: true,
                ..rate(90, None)
            },
        )
        .await;
        assert!(
            matches!(out, PushOutcome::Written { rating: 90, .. }),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn a_different_existing_rating_is_a_conflict_unless_overwriting() {
        let server = MockServer::start().await;
        current(&server, Some((70, None))).await;
        expect_put(&server, serde_json::json!({"rating": 90}), 0).await;

        let out = run(&server, rate(90, None)).await;
        assert_eq!(
            out,
            PushOutcome::Skipped(PushGap::Conflict {
                rating: 70,
                notes: None
            })
        );
    }

    #[tokio::test]
    async fn different_existing_notes_are_a_conflict_too() {
        let server = MockServer::start().await;
        current(&server, Some((90, Some("mine")))).await;

        let out = run(&server, rate(90, Some("theirs"))).await;
        assert!(
            matches!(
                out,
                PushOutcome::Skipped(PushGap::Conflict { rating: 90, .. })
            ),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn the_same_rating_and_notes_send_nothing() {
        let server = MockServer::start().await;
        current(&server, Some((90, Some("great")))).await;
        expect_put(
            &server,
            serde_json::json!({"rating": 90, "notes": "great"}),
            0,
        )
        .await;

        assert_eq!(
            run(&server, rate(90, Some("great"))).await,
            PushOutcome::Unchanged
        );
    }

    #[tokio::test]
    async fn notes_alone_attach_to_the_existing_rating() {
        let server = MockServer::start().await;
        current(&server, Some((75, None))).await;
        expect_put(
            &server,
            serde_json::json!({"rating": 75, "notes": "why"}),
            1,
        )
        .await;

        let out = run(
            &server,
            RatingPush {
                rating: None,
                notes: Some("why".into()),
                overwrite: false,
            },
        )
        .await;
        assert!(
            matches!(out, PushOutcome::Written { rating: 75, .. }),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn notes_without_any_rating_need_one() {
        let server = MockServer::start().await;
        current(&server, None).await;

        let out = run(
            &server,
            RatingPush {
                rating: None,
                notes: Some("why".into()),
                overwrite: false,
            },
        )
        .await;
        assert_eq!(out, PushOutcome::Skipped(PushGap::NeedsRating));
    }

    #[tokio::test]
    async fn a_series_outside_the_library_is_not_written() {
        let server = MockServer::start().await;
        let codex = CodexClient::new(&server.uri(), reqwest::Client::new());
        let other = ExternalId::from_tsundoku("mangabaka:7").unwrap();

        let out = push(&codex, &credential(), &snapshot(), &other, rate(90, None)).await;
        assert_eq!(out, PushOutcome::Skipped(PushGap::NotInLibrary));
    }

    #[tokio::test]
    async fn a_key_without_ratings_write_names_the_fix() {
        let server = MockServer::start().await;
        current(&server, None).await;
        Mock::given(method("PUT"))
            .and(path(format!("/api/v1/series/{SERIES}/rating")))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let out = run(&server, rate(90, None)).await;
        assert_eq!(out, PushOutcome::Skipped(PushGap::MissingPermission));
        assert!(
            PushGap::MissingPermission
                .message()
                .contains("ratings:write")
        );
    }
}
