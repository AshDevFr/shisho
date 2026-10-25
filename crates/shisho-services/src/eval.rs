//! Offline evaluation of backlog rankers.
//!
//! Hold-out replay: every series the caller rated in their own top quartile
//! is, one at a time, hidden (rating removed, marked untouched) and dropped
//! into the untouched backlog; the ranker orders that backlog and we record
//! where the hidden favourite landed. A ranker that understands the caller's
//! taste puts their known favourites near the top; snapshot order lands them
//! wherever Codex happens to list them. The hidden series' own rating never
//! reaches the ranker, or the metric would be circular.

use crate::library_snapshot::{LibrarySnapshot, SnapshotSeries};
use crate::read_state::ReadState;

/// Orders a backlog, best first.
pub trait Ranker {
    fn name(&self) -> &str;
    /// Indices into `backlog`, best first. `snapshot` is the caller's library
    /// as the ranker may see it (with any held-out series masked).
    fn rank(&self, snapshot: &LibrarySnapshot, backlog: &[&SnapshotSeries]) -> Vec<usize>;
}

/// The baseline: the backlog in the order Codex lists it.
pub struct SnapshotOrder;

impl Ranker for SnapshotOrder {
    fn name(&self) -> &str {
        "snapshot-order"
    }

    fn rank(&self, _snapshot: &LibrarySnapshot, backlog: &[&SnapshotSeries]) -> Vec<usize> {
        (0..backlog.len()).collect()
    }
}

/// Hold-out metrics for one ranker over one library.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalReport {
    pub ranker: String,
    /// Favourites replayed.
    pub held_out: usize,
    /// Mean backlog size the favourites were ranked within.
    pub mean_backlog: f64,
    /// Share of favourites ranked in the top 10.
    pub recall_at_10: f64,
    /// Share of favourites ranked in the top 40 (the default page).
    pub recall_at_40: f64,
    /// Mean reciprocal rank.
    pub mrr: f64,
    /// Mean rank as a fraction of the backlog: 0.0 = always first, 1.0 =
    /// always last; a ranker with no signal sits near 0.5.
    pub mean_rank_fraction: f64,
}

/// The caller's favourites: rated at or above their own top-quartile rating.
pub fn favourites(snapshot: &LibrarySnapshot) -> Vec<&SnapshotSeries> {
    let mut ratings: Vec<i32> = snapshot.series.iter().filter_map(|s| s.rating).collect();
    if ratings.is_empty() {
        return Vec::new();
    }
    ratings.sort_unstable_by(|a, b| b.cmp(a));
    let threshold = ratings[ratings.len() / 4];
    snapshot
        .series
        .iter()
        .filter(|s| s.rating.is_some_and(|r| r >= threshold))
        .collect()
}

/// Replay every favourite through `ranker`. `None` when the library has no
/// ratings to hold out.
pub fn evaluate(snapshot: &LibrarySnapshot, ranker: &dyn Ranker) -> Option<EvalReport> {
    let favourites: Vec<String> = favourites(snapshot).iter().map(|s| s.id.clone()).collect();
    if favourites.is_empty() {
        return None;
    }

    let mut ranks: Vec<(usize, usize)> = Vec::with_capacity(favourites.len());
    for held in &favourites {
        let masked = mask(snapshot, held);
        let backlog: Vec<&SnapshotSeries> = masked
            .series
            .iter()
            .filter(|s| s.read_state == ReadState::Untouched)
            .collect();
        let order = ranker.rank(&masked, &backlog);
        let position = order
            .iter()
            .position(|&i| backlog[i].id == *held)
            .expect("ranker must return every backlog index");
        ranks.push((position + 1, backlog.len()));
    }

    let n = ranks.len() as f64;
    let share = |k: usize| ranks.iter().filter(|(r, _)| *r <= k).count() as f64 / n;
    Some(EvalReport {
        ranker: ranker.name().to_owned(),
        held_out: ranks.len(),
        mean_backlog: ranks.iter().map(|(_, len)| *len as f64).sum::<f64>() / n,
        recall_at_10: share(10),
        recall_at_40: share(40),
        mrr: ranks.iter().map(|(r, _)| 1.0 / *r as f64).sum::<f64>() / n,
        mean_rank_fraction: ranks
            .iter()
            .map(|(r, len)| {
                if *len <= 1 {
                    0.0
                } else {
                    (*r - 1) as f64 / (*len - 1) as f64
                }
            })
            .sum::<f64>()
            / n,
    })
}

/// Where a ranker places series the caller later judged, for owned series
/// with a recorded outcome: each is hidden (unrated, untouched) in turn and
/// ranked within the backlog. A ranker that tracks taste puts the liked ones
/// high and the dropped ones low.
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeReport {
    pub ranker: String,
    pub liked: usize,
    pub disliked: usize,
    /// Mean rank fraction (0 = first, 1 = last) of loved and liked series.
    pub liked_rank_fraction: f64,
    /// Mean rank fraction of dropped and did-not-finish series.
    pub disliked_rank_fraction: f64,
}

/// Replay recorded outcomes (`(codex series id, outcome)`) through `ranker`.
/// `None` when no owned series carries a decisive outcome.
pub fn evaluate_outcomes(
    snapshot: &LibrarySnapshot,
    outcomes: &[(String, crate::verdicts::Outcome)],
    ranker: &dyn Ranker,
) -> Option<OutcomeReport> {
    use crate::verdicts::Outcome;
    let mut liked: Vec<f64> = Vec::new();
    let mut disliked: Vec<f64> = Vec::new();
    for (series_id, outcome) in outcomes {
        let bucket = match outcome {
            Outcome::Loved | Outcome::Liked => &mut liked,
            Outcome::Dropped | Outcome::Dnf => &mut disliked,
            Outcome::Mixed | Outcome::NotInterested | Outcome::Reopened | Outcome::Other => {
                continue;
            }
        };
        if snapshot.series_by_uuid(series_id).is_none() {
            continue;
        }
        let masked = mask(snapshot, series_id);
        let backlog: Vec<&SnapshotSeries> = masked
            .series
            .iter()
            .filter(|s| s.read_state == ReadState::Untouched)
            .collect();
        let order = ranker.rank(&masked, &backlog);
        let Some(position) = order.iter().position(|&i| backlog[i].id == *series_id) else {
            continue;
        };
        let fraction = if backlog.len() <= 1 {
            0.0
        } else {
            position as f64 / (backlog.len() - 1) as f64
        };
        bucket.push(fraction);
    }
    if liked.is_empty() && disliked.is_empty() {
        return None;
    }
    let mean = |v: &[f64]| {
        if v.is_empty() {
            f64::NAN
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    Some(OutcomeReport {
        ranker: ranker.name().to_owned(),
        liked: liked.len(),
        disliked: disliked.len(),
        liked_rank_fraction: mean(&liked),
        disliked_rank_fraction: mean(&disliked),
    })
}

/// The library as an eval fixture: only what rankers read. Titles,
/// summaries, rating notes, library names, Codex ids, external ids, status
/// and year are dropped; each author becomes a token salted per export, so
/// shared authors still match within the file but names cannot be recovered
/// by hashing a list of known authors.
pub fn deidentify(snapshot: &LibrarySnapshot) -> Vec<SnapshotSeries> {
    use rand::Rng;
    use sha2::{Digest, Sha256};

    let mut salt = [0u8; 16];
    rand::rng().fill_bytes(&mut salt);
    let token = |author: &str| {
        let digest = Sha256::new()
            .chain_update(salt)
            .chain_update(author.as_bytes())
            .finalize();
        format!("author-{}", &hex::encode(digest)[..12])
    };
    snapshot
        .series
        .iter()
        .enumerate()
        .map(|(i, s)| SnapshotSeries {
            id: format!("s{i}"),
            library_id: "lib".into(),
            library_name: "Library".into(),
            title: format!("series {i}"),
            summary: None,
            status: None,
            year: None,
            genres: s.genres.clone(),
            tags: s.tags.clone(),
            authors: s.authors.iter().map(|a| token(a)).collect(),
            book_count: s.book_count,
            unread_count: s.unread_count,
            read_state: s.read_state,
            external_ids: Vec::new(),
            rating: s.rating,
            rating_notes: None,
            rating_updated_at: None,
        })
        .collect()
}

/// The library with `held` hidden: unrated, notes dropped, and untouched.
fn mask(snapshot: &LibrarySnapshot, held: &str) -> LibrarySnapshot {
    let series = snapshot
        .series
        .iter()
        .map(|s| {
            if s.id == held {
                SnapshotSeries {
                    rating: None,
                    rating_notes: None,
                    read_state: ReadState::Untouched,
                    unread_count: Some(s.book_count),
                    ..s.clone()
                }
            } else {
                s.clone()
            }
        })
        .collect();
    LibrarySnapshot::from_series(series)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn series(id: &str, state: ReadState, rating: Option<i32>) -> SnapshotSeries {
        SnapshotSeries {
            id: id.into(),
            library_id: "lib".into(),
            library_name: "Manga".into(),
            title: id.into(),
            summary: None,
            status: None,
            year: None,
            genres: Vec::new(),
            tags: Vec::new(),
            authors: Vec::new(),
            book_count: 5,
            unread_count: Some(if state == ReadState::Completed { 0 } else { 5 }),
            read_state: state,
            external_ids: Vec::new(),
            rating,
            rating_notes: None,
            rating_updated_at: None,
        }
    }

    /// Ranks the held-out favourite first by cheating on its id.
    struct Oracle(&'static str);
    impl Ranker for Oracle {
        fn name(&self) -> &str {
            "oracle"
        }
        fn rank(&self, _: &LibrarySnapshot, backlog: &[&SnapshotSeries]) -> Vec<usize> {
            let mut order: Vec<usize> = (0..backlog.len()).collect();
            order.sort_by_key(|&i| backlog[i].id != self.0);
            order
        }
    }

    fn library() -> LibrarySnapshot {
        let mut all: Vec<SnapshotSeries> = (0..19)
            .map(|i| series(&format!("u{i}"), ReadState::Untouched, None))
            .collect();
        // The only rating, listed last: snapshot order ranks it last.
        all.push(series("fav", ReadState::Completed, Some(95)));
        LibrarySnapshot::from_series(all)
    }

    #[test]
    fn eval_snapshot_order_ranks_a_last_listed_favourite_last() {
        let report = evaluate(&library(), &SnapshotOrder).expect("one favourite");
        assert_eq!(report.held_out, 1);
        assert_eq!(report.mean_backlog, 20.0);
        assert_eq!(report.recall_at_10, 0.0);
        assert_eq!(report.mean_rank_fraction, 1.0);
        assert!((report.mrr - 1.0 / 20.0).abs() < 1e-9);
    }

    #[test]
    fn eval_a_perfect_ranker_scores_one() {
        let report = evaluate(&library(), &Oracle("fav")).expect("one favourite");
        assert_eq!(report.recall_at_10, 1.0);
        assert_eq!(report.mrr, 1.0);
        assert_eq!(report.mean_rank_fraction, 0.0);
    }

    #[test]
    fn eval_hides_the_held_out_rating_from_the_ranker() {
        struct Peek;
        impl Ranker for Peek {
            fn name(&self) -> &str {
                "peek"
            }
            fn rank(&self, snapshot: &LibrarySnapshot, backlog: &[&SnapshotSeries]) -> Vec<usize> {
                let fav = snapshot.series_by_uuid("fav").unwrap();
                assert_eq!(fav.rating, None, "held-out rating leaked");
                assert_eq!(fav.read_state, ReadState::Untouched);
                (0..backlog.len()).collect()
            }
        }
        evaluate(&library(), &Peek).unwrap();
    }

    #[test]
    fn eval_favourites_are_the_top_quartile() {
        let lib = LibrarySnapshot::from_series(vec![
            series("a", ReadState::Completed, Some(90)),
            series("b", ReadState::Completed, Some(80)),
            series("c", ReadState::Completed, Some(60)),
            series("d", ReadState::Completed, Some(40)),
            series("e", ReadState::Completed, Some(20)),
        ]);
        let ids: Vec<&str> = favourites(&lib).iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn eval_fixture_roundtrips_through_json() {
        let mut fav = series("fav", ReadState::Completed, Some(95));
        fav.external_ids =
            vec![shisho_ids::ExternalId::from_codex("plugin:myanimelist:42").unwrap()];
        let json = serde_json::to_string(&vec![fav]).unwrap();
        assert!(
            json.contains("\"mal:42\""),
            "ids serialize canonically: {json}"
        );
        let back: Vec<SnapshotSeries> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[0].external_ids[0].canonical_key(), "mal:42");
        assert_eq!(back[0].rating, Some(95));
    }

    #[test]
    fn eval_outcome_replay_separates_liked_from_disliked() {
        use crate::verdicts::Outcome;
        let mut series_list: Vec<SnapshotSeries> = (0..8)
            .map(|i| series(&format!("u{i}"), ReadState::Untouched, None))
            .collect();
        series_list.push(series("good", ReadState::Completed, None));
        series_list.push(series("bad", ReadState::Completed, None));
        let lib = LibrarySnapshot::from_series(series_list);
        let report = evaluate_outcomes(
            &lib,
            &[
                ("good".into(), Outcome::Loved),
                ("bad".into(), Outcome::Dropped),
            ],
            &Oracle("good"),
        )
        .unwrap();
        assert_eq!((report.liked, report.disliked), (1, 1));
        assert_eq!(report.liked_rank_fraction, 0.0);
        assert!(report.disliked_rank_fraction > 0.0);
    }

    #[test]
    fn eval_without_ratings_has_nothing_to_replay() {
        let lib = LibrarySnapshot::from_series(vec![series("a", ReadState::Untouched, None)]);
        assert!(evaluate(&lib, &SnapshotOrder).is_none());
    }

    /// A small library carrying everything a real export would.
    fn identifiable_library() -> LibrarySnapshot {
        let genres = ["Fantasy", "Romance", "Horror", "Comedy"];
        let series = (0..24)
            .map(|i| {
                let state = match i % 3 {
                    0 => ReadState::Completed,
                    1 => ReadState::InProgress,
                    _ => ReadState::Untouched,
                };
                let rating = (state == ReadState::Completed).then_some(60 + i * 2);
                SnapshotSeries {
                    library_name: "Private Shelf".into(),
                    title: format!("Secret Title {i}"),
                    summary: Some(format!("Plot of secret title {i}")),
                    status: Some("ongoing".into()),
                    year: Some(2000 + i),
                    genres: vec![genres[(i % 4) as usize].into()],
                    tags: vec![format!("tag-{}", i % 5)],
                    authors: vec![format!("Author Name {}", i % 6)],
                    external_ids: vec![
                        shisho_ids::ExternalId::from_tsundoku(&format!("mal:{}", 1000 + i))
                            .unwrap(),
                    ],
                    rating_notes: rating.map(|_| format!("my private note {i}")),
                    ..series(&format!("uuid-secret-{i}"), state, rating)
                }
            })
            .collect();
        LibrarySnapshot::from_series(series)
    }

    #[test]
    fn deidentified_fixtures_keep_only_what_rankers_read() {
        let raw = identifiable_library();
        let clean = deidentify(&raw);
        let json = serde_json::to_string(&clean).unwrap();
        for leak in [
            "Secret Title",
            "secret title",
            "private note",
            "Private Shelf",
            "uuid-secret",
            "mal:",
            "Author Name",
            "ongoing",
        ] {
            assert!(!json.contains(leak), "{leak} leaked: {json}");
        }

        // Authors still match within the file, and only within it.
        assert_eq!(
            clean[0].authors, clean[6].authors,
            "same author, same token"
        );
        assert_ne!(clean[0].authors, clean[1].authors);
        assert_ne!(
            deidentify(&raw)[0].authors,
            clean[0].authors,
            "salted per export"
        );

        // The rankers see the same library.
        let ranker = crate::affinity::AffinityRanker::default();
        let clean = LibrarySnapshot::from_series(clean);
        assert!(
            evaluate(&raw, &ranker).is_some(),
            "the library has favourites"
        );
        assert_eq!(
            evaluate(&raw, &ranker).map(|r| (
                r.recall_at_10,
                r.recall_at_40,
                r.mrr,
                r.mean_rank_fraction
            )),
            evaluate(&clean, &ranker).map(|r| (
                r.recall_at_10,
                r.recall_at_40,
                r.mrr,
                r.mean_rank_fraction
            ))
        );
    }
}
