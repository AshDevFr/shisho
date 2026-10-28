//! The user's Codex want-to-read queue: read it as series (for grading), and
//! add or remove series on request.
//!
//! The queue lives in Codex and only holds things from the user's own Codex
//! library, so every write resolves its target against the library snapshot
//! first: a series the user does not have in Codex is reported, never sent.
//! An entry can flag a whole series or a single book; book entries are
//! folded into their series, which is what gets graded and what a removal
//! clears. Queue order is the user's (Codex's `custom` order) and is never
//! changed.
//!
//! Writes need `want-to-read:write` on the user's Shisho-minted key; without
//! it Codex answers 403 and the report names the fix.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
use serde::Serialize;
use shisho_codex::{CodexClient, CodexCredential, CodexError};

use crate::library_snapshot::{LibrarySnapshot, SnapshotSeries};
use crate::lookup::parse_known_id;

/// Book lookups in flight while folding book entries into series.
const FAN_OUT: usize = 8;

/// One queued series, with every queue entry that points at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedSeries {
    pub codex_series_id: String,
    /// 1-based place of the series' first entry in the user's queue order.
    pub position: usize,
    /// When its first entry was queued.
    pub added_at: DateTime<Utc>,
    /// The series itself is queued (not only some of its books).
    pub series_entry: bool,
    /// Queued books of the series, by Codex book id.
    pub book_entries: Vec<String>,
}

/// The queue as series, in the user's order.
#[derive(Debug, Clone, Default)]
pub struct Queue {
    pub series: Vec<QueuedSeries>,
    /// Book entries whose series could not be looked up.
    pub unresolved_books: usize,
}

impl Queue {
    pub fn get(&self, codex_series_id: &str) -> Option<&QueuedSeries> {
        self.series
            .iter()
            .find(|q| q.codex_series_id == codex_series_id)
    }
}

/// Read the user's queue and fold book entries into their series.
pub async fn load(codex: &CodexClient, credential: &CodexCredential) -> Result<Queue, CodexError> {
    let list = codex.want_to_read(credential).await?;

    // Built up front and only run when the buffered stream polls them, so
    // the future stays `Send` for the MCP handler.
    let lookups: Vec<_> = list
        .items
        .iter()
        .filter(|entry| entry.series_id.is_none())
        .filter_map(|entry| entry.book_id.clone())
        .map(|book| async move {
            let series = codex.book_series_id(credential, &book).await;
            (book, series)
        })
        .collect();
    let mut book_series: HashMap<String, String> = HashMap::new();
    let mut unresolved_books = 0;
    let mut resolved = stream::iter(lookups).buffered(FAN_OUT);
    while let Some((book, series)) = resolved.next().await {
        match series {
            Ok(series) => {
                book_series.insert(book, series);
            }
            Err(err) => {
                tracing::warn!(book, error = %err, "want-to-read book entry has no resolvable series");
                unresolved_books += 1;
            }
        }
    }

    let mut series: Vec<QueuedSeries> = Vec::new();
    for entry in list.items {
        let (series_id, book) = match (entry.series_id, entry.book_id) {
            (Some(series_id), _) => (series_id, None),
            (None, Some(book)) => match book_series.get(&book) {
                Some(series_id) => (series_id.clone(), Some(book)),
                None => continue,
            },
            (None, None) => continue,
        };
        let index = match series.iter().position(|q| q.codex_series_id == series_id) {
            Some(index) => index,
            None => {
                series.push(QueuedSeries {
                    codex_series_id: series_id,
                    position: series.len() + 1,
                    added_at: entry.added_at,
                    series_entry: false,
                    book_entries: Vec::new(),
                });
                series.len() - 1
            }
        };
        match book {
            Some(book) => series[index].book_entries.push(book),
            None => series[index].series_entry = true,
        }
    }
    Ok(Queue {
        series,
        unresolved_books,
    })
}

/// What happened to one requested series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemStatus {
    Added,
    AlreadyQueued,
    Removed,
    NotQueued,
    /// Not in the user's Codex library (or not a known id), so not sent.
    NotInLibrary,
    /// Codex refused or failed the write; see the report's gap.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemResult {
    /// The id as the caller gave it.
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_series_id: Option<String>,
    pub status: ItemStatus,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateReport {
    pub items: Vec<ItemResult>,
    /// Why some writes failed, phrased for the user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap: Option<String>,
}

/// Add and remove series, each named by an external id in any known notation
/// or by its Codex series id. Never errors: failures are per item, with the
/// reason in the report's gap.
pub async fn update(
    codex: &CodexClient,
    credential: &CodexCredential,
    snapshot: &LibrarySnapshot,
    add: &[String],
    remove: &[String],
) -> UpdateReport {
    let mut report = UpdateReport::default();
    if add.is_empty() && remove.is_empty() {
        return report;
    }
    // The current queue says what is already there, and which book entries
    // a removal has to clear.
    let queue = match load(codex, credential).await {
        Ok(queue) => queue,
        Err(err) => {
            report.gap = Some(gap(&err));
            report.items = add
                .iter()
                .chain(remove)
                .map(|id| item(id, None, ItemStatus::Failed))
                .collect();
            return report;
        }
    };

    // Adds go out in one bulk call, so their results are settled after it.
    let mut pending: Vec<(usize, String)> = Vec::new();
    for raw in add {
        let copies = copies(snapshot, raw);
        if let Some(queued) = copies.iter().find(|s| queue.get(&s.id).is_some()) {
            report
                .items
                .push(item(raw, Some(queued), ItemStatus::AlreadyQueued));
        } else if let Some(first) = copies.first() {
            pending.push((report.items.len(), first.id.clone()));
            report.items.push(item(raw, Some(first), ItemStatus::Added));
        } else {
            report.items.push(item(raw, None, ItemStatus::NotInLibrary));
        }
    }
    if !pending.is_empty() {
        let mut ids: Vec<String> = Vec::new();
        for (_, id) in &pending {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        if let Err(err) = codex.add_want_to_read(credential, &ids).await {
            report.gap = Some(gap(&err));
            for (index, _) in &pending {
                report.items[*index].status = ItemStatus::Failed;
            }
        }
    }

    for raw in remove {
        let copies = copies(snapshot, raw);
        let queued: Vec<(&SnapshotSeries, &QueuedSeries)> = copies
            .iter()
            .filter_map(|s| queue.get(&s.id).map(|q| (*s, q)))
            .collect();
        let Some((first, _)) = queued.first() else {
            let status = if copies.is_empty() {
                ItemStatus::NotInLibrary
            } else {
                ItemStatus::NotQueued
            };
            report
                .items
                .push(item(raw, copies.first().copied(), status));
            continue;
        };
        let mut status = ItemStatus::Removed;
        for (_, entry) in &queued {
            if let Err(err) = remove_entries(codex, credential, entry).await {
                report.gap = Some(gap(&err));
                status = ItemStatus::Failed;
                break;
            }
        }
        report.items.push(item(raw, Some(first), status));
    }
    report
}

/// Every library series the caller's id names: a Codex series id names one;
/// an external id names every copy carrying it (chapter and volume editions
/// are separate Codex series sharing external ids).
fn copies<'a>(snapshot: &'a LibrarySnapshot, raw: &str) -> Vec<&'a SnapshotSeries> {
    if let Some(series) = snapshot.series_by_uuid(raw.trim()) {
        return vec![series];
    }
    let Some(id) = parse_known_id(raw) else {
        return Vec::new();
    };
    snapshot
        .series
        .iter()
        .filter(|s| s.external_ids.iter().any(|e| e.key() == id.key()))
        .collect()
}

async fn remove_entries(
    codex: &CodexClient,
    credential: &CodexCredential,
    entry: &QueuedSeries,
) -> Result<(), CodexError> {
    if entry.series_entry {
        codex
            .remove_want_to_read_series(credential, &entry.codex_series_id)
            .await?;
    }
    for book in &entry.book_entries {
        codex.remove_want_to_read_book(credential, book).await?;
    }
    Ok(())
}

fn item(raw: &str, series: Option<&SnapshotSeries>, status: ItemStatus) -> ItemResult {
    ItemResult {
        id: raw.to_owned(),
        title: series.map(|s| s.title.clone()),
        codex_series_id: series.map(|s| s.id.clone()),
        status,
    }
}

fn gap(err: &CodexError) -> String {
    if err.is_forbidden() {
        "the user's Codex key lacks the want-to-read:write permission. Regenerate the key from \
         the Codex key panel on Shisho's Status page, or add the permission to the key in Codex"
            .into()
    } else {
        format!("Codex failed the want-to-read update: {err}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_state::ReadState;
    use shisho_ids::ExternalId;
    use wiremock::matchers::{body_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn series(id: &str, title: &str, external: &str) -> SnapshotSeries {
        SnapshotSeries {
            id: id.into(),
            library_id: "lib".into(),
            library_name: "Manga".into(),
            title: title.into(),
            summary: None,
            status: None,
            year: None,
            genres: Vec::new(),
            tags: Vec::new(),
            authors: Vec::new(),
            book_count: 3,
            unread_count: Some(3),
            read_state: ReadState::Untouched,
            external_ids: vec![ExternalId::from_tsundoku(external).unwrap()],
            rating: None,
            rating_notes: None,
            rating_updated_at: None,
        }
    }

    fn snapshot() -> LibrarySnapshot {
        LibrarySnapshot::from_series(vec![
            series("s-1", "Queued Series", "mangabaka:1"),
            series("s-2", "Book Queued", "mangabaka:2"),
            series("s-3", "Not Queued", "mangabaka:3"),
        ])
    }

    fn credential() -> CodexCredential {
        CodexCredential::ApiKey("codex_key".into())
    }

    fn codex(server: &MockServer) -> CodexClient {
        CodexClient::new(&server.uri(), reqwest::Client::new())
    }

    /// s-1 queued as a series, then two books of s-2.
    async fn mount_queue(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/v1/want-to-read"))
            .and(query_param("sort", "custom"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [
                    {"id": "e-1", "itemType": "series", "seriesId": "s-1", "addedAt": "2026-10-01T00:00:00Z"},
                    {"id": "e-2", "itemType": "book", "bookId": "b-21", "addedAt": "2026-10-02T00:00:00Z"},
                    {"id": "e-3", "itemType": "book", "bookId": "b-22", "addedAt": "2026-10-03T00:00:00Z"}
                ],
                "total": 3
            })))
            .mount(server)
            .await;
        for book in ["b-21", "b-22"] {
            Mock::given(method("GET"))
                .and(path(format!("/api/v1/books/{book}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"id": book, "seriesId": "s-2"})),
                )
                .mount(server)
                .await;
        }
    }

    #[tokio::test]
    async fn book_entries_fold_into_their_series_in_queue_order() {
        let server = MockServer::start().await;
        mount_queue(&server).await;

        let queue = load(&codex(&server), &credential()).await.unwrap();
        assert_eq!(queue.series.len(), 2);
        assert_eq!(queue.series[0].codex_series_id, "s-1");
        assert_eq!(queue.series[0].position, 1);
        assert!(queue.series[0].series_entry);
        assert_eq!(queue.series[1].codex_series_id, "s-2");
        assert_eq!(queue.series[1].position, 2);
        assert!(!queue.series[1].series_entry);
        assert_eq!(queue.series[1].book_entries, vec!["b-21", "b-22"]);
        assert_eq!(queue.unresolved_books, 0);
    }

    #[tokio::test]
    async fn a_book_that_cannot_be_looked_up_is_counted_not_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/want-to-read"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [{"id": "e-1", "itemType": "book", "bookId": "gone", "addedAt": "2026-10-01T00:00:00Z"}],
                "total": 1
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/books/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let queue = load(&codex(&server), &credential()).await.unwrap();
        assert!(queue.series.is_empty());
        assert_eq!(queue.unresolved_books, 1);
    }

    #[tokio::test]
    async fn adds_only_series_in_the_library_and_not_already_queued() {
        let server = MockServer::start().await;
        mount_queue(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/v1/want-to-read/bulk"))
            .and(body_json(serde_json::json!({"seriesIds": ["s-3"]})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"added": 1, "alreadyPresent": 0})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let add = ["mangabaka:3", "s-1", "mangabaka:99"].map(String::from);
        let report = update(&codex(&server), &credential(), &snapshot(), &add, &[]).await;
        let statuses: Vec<_> = report.items.iter().map(|i| i.status).collect();
        assert_eq!(
            statuses,
            vec![
                ItemStatus::Added,
                ItemStatus::AlreadyQueued,
                ItemStatus::NotInLibrary
            ]
        );
        assert_eq!(report.items[0].title.as_deref(), Some("Not Queued"));
        assert_eq!(report.gap, None);
    }

    #[tokio::test]
    async fn removing_a_series_clears_its_book_entries_too() {
        let server = MockServer::start().await;
        mount_queue(&server).await;
        for book in ["b-21", "b-22"] {
            Mock::given(method("DELETE"))
                .and(path(format!("/api/v1/want-to-read/books/{book}")))
                .respond_with(ResponseTemplate::new(204))
                .expect(1)
                .mount(&server)
                .await;
        }
        Mock::given(method("DELETE"))
            .and(path("/api/v1/want-to-read/series/s-2"))
            .respond_with(ResponseTemplate::new(204))
            .expect(0)
            .mount(&server)
            .await;

        let remove = ["mangabaka:2", "mangabaka:3"].map(String::from);
        let report = update(&codex(&server), &credential(), &snapshot(), &[], &remove).await;
        let statuses: Vec<_> = report.items.iter().map(|i| i.status).collect();
        assert_eq!(statuses, vec![ItemStatus::Removed, ItemStatus::NotQueued]);
    }

    #[tokio::test]
    async fn a_key_without_the_permission_names_the_fix() {
        let server = MockServer::start().await;
        mount_queue(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/v1/want-to-read/bulk"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let add = ["mangabaka:3".to_owned()];
        let report = update(&codex(&server), &credential(), &snapshot(), &add, &[]).await;
        assert_eq!(report.items[0].status, ItemStatus::Failed);
        let gap = report.gap.expect("gap");
        assert!(gap.contains("want-to-read:write"), "{gap}");
    }
}
