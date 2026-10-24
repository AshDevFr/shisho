//! Per-user taste scoring: how well a series' genres and tags match what the
//! caller rated highly or finished, with the evidence ("anchors") that
//! explains each score. Pure and deterministic; the candidate pools rank with
//! it and the model reads the anchors instead of re-deriving the match from
//! descriptions in context.
//!
//! A series the caller rated contributes `(rating - mean) / 25`, clamped to
//! [-1, 1], where `mean` is their own average rating shrunk toward a neutral
//! 60 (so one rating still reads as liked or disliked). A completed, unrated
//! series contributes a mild positive. Each genre or tag then carries the sum
//! of its series' contributions divided by the square root of how many of
//! those series carry it, so one tag on one favourite does not dominate, and
//! a candidate's affinity is its terms' weights (tags discounted against
//! genres) normalized by the square root of its term count.

use std::collections::HashMap;

use serde::Serialize;

use crate::library_snapshot::{LibrarySnapshot, SnapshotSeries};
use crate::read_state::ReadState;

/// The neutral rating the caller's mean is shrunk toward (Codex 1-100 scale).
const NEUTRAL_RATING: f64 = 60.0;
/// How many neutral pseudo-ratings the mean is shrunk with.
const MEAN_SHRINKAGE: f64 = 2.0;
/// Rating points per unit of contribution.
const RATING_SPREAD: f64 = 25.0;
/// Contribution of a completed series the caller did not rate.
const COMPLETED_UNRATED: f64 = 0.3;
/// Tags are noisier than genres.
const TAG_FACTOR: f64 = 0.6;
/// Anchors returned per candidate.
const MAX_ANCHORS: usize = 2;

/// A series of the caller's that explains a candidate's score.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Anchor {
    pub title: String,
    /// The caller's Codex series UUID.
    pub codex_series_id: String,
    /// The caller's rating, 1-100; absent when the anchor is a completed,
    /// unrated series.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub your_rating: Option<i32>,
    /// The genres and tags the candidate shares with it.
    pub shared: Vec<String>,
}

/// A candidate's score.
#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    pub affinity: f64,
    pub anchors: Vec<Anchor>,
}

/// One series that shaped the taste, kept for anchor lookup.
#[derive(Debug, Clone)]
struct Source {
    title: String,
    codex_series_id: String,
    rating: Option<i32>,
    weight: f64,
    /// Lowercased genre and tag keys.
    terms: Vec<String>,
}

/// The caller's taste, folded from their library.
#[derive(Debug, Clone, Default)]
pub struct TasteVector {
    /// Lowercased term -> weight.
    terms: HashMap<String, f64>,
    sources: Vec<Source>,
}

impl TasteVector {
    pub fn from_snapshot(snapshot: &LibrarySnapshot) -> Self {
        let ratings: Vec<f64> = snapshot
            .series
            .iter()
            .filter_map(|s| s.rating.map(f64::from))
            .collect();
        let mean = (ratings.iter().sum::<f64>() + NEUTRAL_RATING * MEAN_SHRINKAGE)
            / (ratings.len() as f64 + MEAN_SHRINKAGE);

        let sources: Vec<Source> = snapshot
            .series
            .iter()
            .filter_map(|s| {
                let weight = series_weight(s, mean)?;
                Some(Source {
                    title: s.title.clone(),
                    codex_series_id: s.id.clone(),
                    rating: s.rating,
                    weight,
                    terms: term_keys(&s.genres, &s.tags),
                })
            })
            .collect();

        let mut sums: HashMap<String, (f64, usize)> = HashMap::new();
        for source in &sources {
            for term in &source.terms {
                let entry = sums.entry(term.clone()).or_default();
                entry.0 += source.weight;
                entry.1 += 1;
            }
        }
        let terms = sums
            .into_iter()
            .map(|(term, (sum, count))| (term, sum / (count as f64).sqrt()))
            .collect();
        Self { terms, sources }
    }

    /// No rating or completion to learn from (cold start).
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// The caller's weight for one genre or tag (0 when unknown).
    pub fn term_weight(&self, term: &str) -> f64 {
        self.terms.get(&term.to_lowercase()).copied().unwrap_or(0.0)
    }

    /// The `n` most positively weighted terms, strongest first: what to ask
    /// a discovery query for.
    pub fn top_terms(&self, n: usize) -> Vec<(String, f64)> {
        let mut liked: Vec<(String, f64)> = self
            .terms
            .iter()
            .filter(|(_, w)| **w > 0.0)
            .map(|(t, w)| (t.clone(), *w))
            .collect();
        liked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        liked.truncate(n);
        liked
    }

    pub fn score(&self, genres: &[String], tags: &[String]) -> Score {
        let weighted = genres
            .iter()
            .map(|g| (g, 1.0))
            .chain(tags.iter().map(|t| (t, TAG_FACTOR)));
        let mut total = 0.0;
        let mut count = 0usize;
        for (term, factor) in weighted {
            total += factor * self.term_weight(term);
            count += 1;
        }
        let affinity = if count == 0 {
            0.0
        } else {
            total / (count as f64).sqrt()
        };
        Score {
            affinity,
            anchors: self.anchors(genres, tags),
        }
    }

    /// The liked series sharing the most (weighted) terms with the candidate.
    fn anchors(&self, genres: &[String], tags: &[String]) -> Vec<Anchor> {
        let candidate: Vec<(&String, String)> = genres
            .iter()
            .chain(tags)
            .map(|t| (t, t.to_lowercase()))
            .collect();
        let mut ranked: Vec<(f64, Anchor)> = self
            .sources
            .iter()
            .filter(|s| s.weight > 0.0)
            .filter_map(|source| {
                let mut shared: Vec<String> = Vec::new();
                for (display, key) in &candidate {
                    if source.terms.contains(key) && !shared.contains(display) {
                        shared.push((*display).clone());
                    }
                }
                if shared.is_empty() {
                    return None;
                }
                let strength = source.weight * shared.len() as f64;
                Some((
                    strength,
                    Anchor {
                        title: source.title.clone(),
                        codex_series_id: source.codex_series_id.clone(),
                        your_rating: source.rating,
                        shared,
                    },
                ))
            })
            .collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
        ranked
            .into_iter()
            .take(MAX_ANCHORS)
            .map(|(_, anchor)| anchor)
            .collect()
    }
}

/// How much one of the caller's series says about their taste, if anything.
fn series_weight(series: &SnapshotSeries, mean: f64) -> Option<f64> {
    match (series.rating, series.read_state) {
        (Some(rating), _) => Some(((f64::from(rating) - mean) / RATING_SPREAD).clamp(-1.0, 1.0)),
        (None, ReadState::Completed) => Some(COMPLETED_UNRATED),
        (None, _) => None,
    }
}

fn term_keys(genres: &[String], tags: &[String]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for term in genres.iter().chain(tags) {
        let key = term.to_lowercase();
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// Ranks a backlog by [`TasteVector`] affinity, for offline evaluation.
pub struct AffinityRanker;

impl crate::eval::Ranker for AffinityRanker {
    fn name(&self) -> &str {
        "affinity"
    }

    fn rank(&self, snapshot: &LibrarySnapshot, backlog: &[&SnapshotSeries]) -> Vec<usize> {
        let taste = TasteVector::from_snapshot(snapshot);
        let scores: Vec<f64> = backlog
            .iter()
            .map(|s| taste.score(&s.genres, &s.tags).affinity)
            .collect();
        let mut order: Vec<usize> = (0..backlog.len()).collect();
        // Stable: ties keep the library's own order.
        order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]));
        order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(
        id: &str,
        genres: &[&str],
        tags: &[&str],
        state: ReadState,
        rating: Option<i32>,
    ) -> SnapshotSeries {
        SnapshotSeries {
            id: id.into(),
            library_id: "lib".into(),
            library_name: "Manga".into(),
            title: id.into(),
            summary: None,
            status: None,
            year: None,
            genres: genres.iter().map(|s| (*s).to_owned()).collect(),
            tags: tags.iter().map(|s| (*s).to_owned()).collect(),
            book_count: 5,
            unread_count: Some(if state == ReadState::Completed { 0 } else { 5 }),
            read_state: state,
            external_ids: Vec::new(),
            rating,
            rating_notes: None,
        }
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn fantasy_reader() -> TasteVector {
        TasteVector::from_snapshot(&LibrarySnapshot::from_series(vec![
            series(
                "frieren",
                &["Fantasy"],
                &["Elves"],
                ReadState::Completed,
                Some(90),
            ),
            series(
                "mushoku",
                &["Fantasy"],
                &["Magic"],
                ReadState::Completed,
                Some(90),
            ),
            series(
                "gore",
                &["Horror"],
                &["Gore"],
                ReadState::Completed,
                Some(20),
            ),
        ]))
    }

    #[test]
    fn affinity_prefers_what_the_reader_rated_highly() {
        let taste = fantasy_reader();
        let fantasy = taste.score(&strings(&["Fantasy"]), &[]);
        let horror = taste.score(&strings(&["Horror"]), &[]);
        assert!(fantasy.affinity > 0.0);
        assert!(
            horror.affinity < 0.0,
            "a 20/100 rating pushes its genre negative"
        );
    }

    #[test]
    fn affinity_anchors_cite_the_rated_series_and_shared_terms() {
        let taste = fantasy_reader();
        let score = taste.score(&strings(&["Fantasy"]), &strings(&["Elves"]));
        let first = &score.anchors[0];
        assert_eq!(first.title, "frieren", "shares two terms, ranks first");
        assert_eq!(first.your_rating, Some(90));
        assert_eq!(first.shared, strings(&["Fantasy", "Elves"]));
        assert_eq!(score.anchors.len(), 2);
    }

    #[test]
    fn affinity_never_anchors_on_a_disliked_series() {
        let score = fantasy_reader().score(&strings(&["Horror"]), &strings(&["Gore"]));
        assert!(score.anchors.is_empty());
    }

    #[test]
    fn affinity_matching_is_case_insensitive() {
        let taste = fantasy_reader();
        assert_eq!(taste.term_weight("FANTASY"), taste.term_weight("fantasy"));
    }

    #[test]
    fn affinity_single_rating_still_reads_as_liked() {
        let taste = TasteVector::from_snapshot(&LibrarySnapshot::from_series(vec![series(
            "only",
            &["Fantasy"],
            &[],
            ReadState::Completed,
            Some(95),
        )]));
        assert!(taste.term_weight("Fantasy") > 0.5);
    }

    #[test]
    fn affinity_completed_unrated_counts_as_mild_liking() {
        let taste = TasteVector::from_snapshot(&LibrarySnapshot::from_series(vec![series(
            "done",
            &["Mystery"],
            &[],
            ReadState::Completed,
            None,
        )]));
        let w = taste.term_weight("Mystery");
        assert!(w > 0.0 && w < 0.5);
    }

    #[test]
    fn affinity_cold_start_is_empty_and_scores_zero() {
        let taste = TasteVector::from_snapshot(&LibrarySnapshot::from_series(vec![series(
            "unread",
            &["Fantasy"],
            &[],
            ReadState::Untouched,
            None,
        )]));
        assert!(taste.is_empty());
        let score = taste.score(&strings(&["Fantasy"]), &[]);
        assert_eq!(score.affinity, 0.0);
        assert!(score.anchors.is_empty());
    }

    #[test]
    fn affinity_ranker_beats_snapshot_order_on_the_eval_fixture() {
        let path = format!(
            "{}/../../tests/fixtures/eval/library.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let series: Vec<SnapshotSeries> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let library = LibrarySnapshot::from_series(series);
        let baseline = crate::eval::evaluate(&library, &crate::eval::SnapshotOrder).unwrap();
        let affinity = crate::eval::evaluate(&library, &AffinityRanker).unwrap();
        assert!(
            affinity.mean_rank_fraction < baseline.mean_rank_fraction,
            "affinity {affinity:?} vs baseline {baseline:?}"
        );
        assert!(affinity.recall_at_10 > baseline.recall_at_10);
    }
}
