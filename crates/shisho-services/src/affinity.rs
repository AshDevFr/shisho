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

use serde::{Deserialize, Serialize};

use crate::library_snapshot::{LibrarySnapshot, SnapshotSeries};
use crate::read_state::ReadState;
use crate::verdicts::Outcome;

/// The neutral rating the caller's mean is shrunk toward (Codex 1-100 scale).
const NEUTRAL_RATING: f64 = 60.0;
/// How many neutral pseudo-ratings the mean is shrunk with.
const MEAN_SHRINKAGE: f64 = 2.0;
/// Rating points per unit of contribution.
const RATING_SPREAD: f64 = 25.0;
/// Floor on the rating spread used for the neutral point.
const MIN_RATING_SD: f64 = 8.0;
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
    /// The caller's Codex series UUID; absent for a series they recorded an
    /// outcome for but do not own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_series_id: Option<String>,
    /// The caller's rating, 1-100; absent when the anchor is a completed,
    /// unrated series.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub your_rating: Option<i32>,
    /// The reading outcome the caller recorded for it ("loved", "dropped").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub your_outcome: Option<String>,
    /// The genres and tags the candidate shares with it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shared: Vec<String>,
    /// Set for a franchise link: what the candidate is to this series
    /// ("sequel of", "side story of").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
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
    codex_series_id: Option<String>,
    rating: Option<i32>,
    outcome: Option<Outcome>,
    weight: f64,
    /// Lowercased genre and tag keys.
    terms: std::collections::HashSet<String>,
    authors: Vec<String>,
}

/// The knobs of the taste fold, so `shisho eval` can compare settings. The
/// defaults are what the candidate pools use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TasteParams {
    /// A rating's contribution is measured from this point on the 1-100
    /// scale; `None` uses the caller's own scale (see [`RatingScale`]).
    pub rating_origin: Option<f64>,
    /// On the caller's own scale, how many standard deviations below their
    /// mean the neutral point sits (0: the mean itself).
    pub origin_sd: f64,
    /// Weigh terms by how specific they are within the caller's library.
    pub library_specificity: bool,
    /// Rating points per unit of contribution.
    pub rating_spread: f64,
    /// Contribution of a completed, unrated series.
    pub completed_unrated: f64,
    /// Contribution of an unrated series the caller is reading.
    pub in_progress: f64,
    /// Weight of a tag relative to a genre.
    pub tag_factor: f64,
    /// A candidate's summed term weights are divided by its term count to
    /// this power (0.5: long tag lists help, but sub-linearly).
    pub length_norm: f64,
}

impl Default for TasteParams {
    fn default() -> Self {
        Self {
            rating_origin: None,
            origin_sd: 1.0,
            library_specificity: true,
            rating_spread: RATING_SPREAD,
            completed_unrated: COMPLETED_UNRATED,
            in_progress: 0.0,
            tag_factor: TAG_FACTOR,
            length_norm: 0.5,
        }
    }
}

/// The caller's taste, folded from their library.
#[derive(Debug, Clone, Default)]
pub struct TasteVector {
    /// Lowercased term -> weight.
    terms: HashMap<String, f64>,
    sources: Vec<Source>,
    tag_factor: f64,
    length_norm: f64,
}

/// A reading outcome the caller recorded, with the series' metadata, as
/// taste evidence.
#[derive(Debug, Clone)]
pub struct OutcomeSignal {
    pub title: String,
    /// Set when the caller owns the series.
    pub codex_series_id: Option<String>,
    pub outcome: Outcome,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    pub authors: Vec<String>,
}

/// How much a recorded outcome says about taste. Mixed and unrecognized
/// outcomes say nothing either way.
fn outcome_weight(outcome: Outcome) -> Option<f64> {
    match outcome {
        Outcome::Loved => Some(1.0),
        Outcome::Liked => Some(0.5),
        Outcome::Dropped => Some(-0.8),
        Outcome::Dnf => Some(-0.5),
        Outcome::Mixed | Outcome::NotInterested | Outcome::Reopened | Outcome::Other => None,
    }
}

impl TasteVector {
    pub fn from_snapshot(snapshot: &LibrarySnapshot) -> Self {
        Self::from_library(snapshot, &[])
    }

    /// The caller's taste from their library and their recorded outcomes. A
    /// series they rated speaks through its rating; an outcome on it is
    /// ignored.
    pub fn from_library(snapshot: &LibrarySnapshot, outcomes: &[OutcomeSignal]) -> Self {
        Self::from_library_with(snapshot, outcomes, &TasteParams::default())
    }

    /// [`Self::from_library`] with explicit [`TasteParams`].
    pub fn from_library_with(
        snapshot: &LibrarySnapshot,
        outcomes: &[OutcomeSignal],
        params: &TasteParams,
    ) -> Self {
        let ratings: Vec<f64> = snapshot
            .series
            .iter()
            .filter_map(|s| s.rating.map(f64::from))
            .collect();
        let origin = params
            .rating_origin
            .unwrap_or_else(|| RatingScale::with_sd(&ratings, params.origin_sd).origin);

        let sources: Vec<Source> = snapshot
            .series
            .iter()
            .filter_map(|s| {
                let weight = series_weight(s, origin, params)?;
                Some(Source {
                    title: s.title.clone(),
                    codex_series_id: Some(s.id.clone()),
                    rating: s.rating,
                    outcome: None,
                    weight,
                    terms: term_keys(&s.genres, &s.tags).into_iter().collect(),
                    authors: s.authors.clone(),
                })
            })
            .chain(outcomes.iter().filter_map(|o| {
                let rated = o
                    .codex_series_id
                    .as_deref()
                    .and_then(|id| snapshot.series_by_uuid(id))
                    .is_some_and(|s| s.rating.is_some());
                if rated {
                    return None;
                }
                Some(Source {
                    title: o.title.clone(),
                    codex_series_id: o.codex_series_id.clone(),
                    rating: None,
                    outcome: Some(o.outcome),
                    weight: outcome_weight(o.outcome)?,
                    terms: term_keys(&o.genres, &o.tags).into_iter().collect(),
                    authors: o.authors.clone(),
                })
            }))
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
        Self {
            terms,
            sources,
            tag_factor: params.tag_factor,
            length_norm: params.length_norm,
        }
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

    /// Every term the taste carries a weight for (lowercased).
    pub fn term_keys(&self) -> Vec<String> {
        self.terms.keys().cloned().collect()
    }

    /// Weigh terms by how specific they are: a term most series carry
    /// ("Shounen") says little, a rare one ("Elf Protagonist") a lot. Each
    /// weight is scaled by `ln(corpus / series_count)` relative to a term
    /// carried by one series in a hundred, clamped to [0.5, 2]. `counts` maps
    /// lowercased terms to how many corpus series carry them; terms missing
    /// from it keep their weight.
    pub fn apply_specificity(&mut self, counts: &HashMap<String, i64>, corpus: u64) {
        if corpus == 0 {
            return;
        }
        for (term, weight) in &mut self.terms {
            if let Some(&count) = counts.get(term)
                && count > 0
            {
                let idf = (corpus as f64 / count as f64).ln();
                *weight *= (idf / REFERENCE_IDF).clamp(MIN_SPECIFICITY, MAX_SPECIFICITY);
            }
        }
    }

    /// [`Self::apply_specificity`] with the caller's own library as the
    /// corpus: a term most of their shelf carries separates nothing, a rare
    /// one does. The MangaBaka dump is not used as the corpus because it
    /// misses many Codex tag names.
    pub fn apply_library_specificity(&mut self, snapshot: &LibrarySnapshot) {
        let mut counts: HashMap<String, i64> = HashMap::new();
        for series in &snapshot.series {
            for term in term_keys(&series.genres, &series.tags) {
                *counts.entry(term).or_default() += 1;
            }
        }
        self.apply_specificity(&counts, snapshot.series.len() as u64);
    }

    pub fn score(&self, genres: &[String], tags: &[String]) -> Score {
        self.score_with_authors(genres, tags, &[])
    }

    /// [`Self::score`], plus a boost (and a "same author as" anchor) when the
    /// candidate shares an author with a series the caller clearly liked.
    pub fn score_with_authors(
        &self,
        genres: &[String],
        tags: &[String],
        authors: &[String],
    ) -> Score {
        Score {
            affinity: self.affinity(genres, tags, authors),
            anchors: self.explain(genres, tags, authors),
        }
    }

    /// The affinity alone: what ranking needs, without building anchors.
    pub fn affinity(&self, genres: &[String], tags: &[String], authors: &[String]) -> f64 {
        let weighted = genres
            .iter()
            .map(|g| (g, 1.0))
            .chain(tags.iter().map(|t| (t, self.tag_factor)));
        let mut total = 0.0;
        let mut count = 0usize;
        for (term, factor) in weighted {
            total += factor * self.term_weight(term);
            count += 1;
        }
        let mut affinity = if count == 0 {
            0.0
        } else {
            total / (count as f64).powf(self.length_norm)
        };
        if self.shared_author(authors).is_some() {
            affinity += AUTHOR_BOOST;
        }
        affinity
    }

    /// The caller's series that explain a score: a shared author first, then
    /// the liked series sharing the most terms. Only worth computing for the
    /// candidates actually returned.
    pub fn explain(&self, genres: &[String], tags: &[String], authors: &[String]) -> Vec<Anchor> {
        let mut anchors = self.anchors(genres, tags);
        if let Some((source, author)) = self.shared_author(authors) {
            anchors.insert(
                0,
                Anchor {
                    title: source.title.clone(),
                    codex_series_id: source.codex_series_id.clone(),
                    your_rating: source.rating,
                    your_outcome: source.outcome.map(|o| o.as_str().to_owned()),
                    shared: vec![author],
                    relation: Some("same author as".to_owned()),
                },
            );
            anchors.truncate(MAX_ANCHORS + 1);
        }
        anchors
    }

    /// The best-liked series sharing an author with the candidate.
    fn shared_author(&self, authors: &[String]) -> Option<(&Source, String)> {
        self.sources
            .iter()
            .filter(|s| s.weight >= AUTHOR_MIN_WEIGHT)
            .filter_map(|source| {
                let author = authors
                    .iter()
                    .find(|a| source.authors.iter().any(|b| b.eq_ignore_ascii_case(a)))?;
                Some((source, author.clone()))
            })
            .max_by(|a, b| a.0.weight.total_cmp(&b.0.weight))
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
                        your_outcome: source.outcome.map(|o| o.as_str().to_owned()),
                        shared,
                        relation: None,
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

/// A term carried by one series in a hundred keeps its weight under
/// [`TasteVector::apply_specificity`]; rarer terms gain, commoner ones lose.
const REFERENCE_IDF: f64 = 4.605_170_185_988_091; // ln(100)
const MIN_SPECIFICITY: f64 = 0.5;
const MAX_SPECIFICITY: f64 = 2.0;
/// Score boost for sharing an author with a clearly liked series.
const AUTHOR_BOOST: f64 = 0.5;
/// How strongly a series must be liked for its authors to count.
const AUTHOR_MIN_WEIGHT: f64 = 0.5;

/// Score shift for a sequel or side story of a series the caller finished and
/// did not dislike: the highest-precision recommendation there is.
const FRANCHISE_BOOST: f64 = 0.5;
/// Score shift for a candidate whose earlier entry the caller has not
/// finished (or finished and disliked): read the original first.
const FRANCHISE_DEMOTE: f64 = -0.5;

/// How a candidate's franchise links bear on recommending it now. `links`
/// are its `(kind, mangabaka id)` pairs, where `kind` is what the linked
/// series is to the candidate; only earlier entries (`prequel`, `main_story`)
/// matter. `library` maps the caller's series by MangaBaka id. Returns the
/// score shift and, for a boost, the anchor naming the finished entry.
pub fn franchise_adjustment(
    links: &[(String, i64)],
    library: &HashMap<i64, &SnapshotSeries>,
) -> (f64, Option<Anchor>) {
    let mut demote = false;
    for (kind, related) in links {
        let relation = match kind.as_str() {
            "prequel" => "sequel of",
            "main_story" => "side story of",
            _ => continue,
        };
        match library.get(related) {
            Some(earlier)
                if earlier.read_state == ReadState::Completed
                    && earlier
                        .rating
                        .is_none_or(|r| f64::from(r) >= NEUTRAL_RATING) =>
            {
                return (
                    FRANCHISE_BOOST,
                    Some(Anchor {
                        title: earlier.title.clone(),
                        codex_series_id: Some(earlier.id.clone()),
                        your_rating: earlier.rating,
                        your_outcome: None,
                        shared: Vec::new(),
                        relation: Some(relation.to_owned()),
                    }),
                );
            }
            _ => demote = true,
        }
    }
    (if demote { FRANCHISE_DEMOTE } else { 0.0 }, None)
}

/// The caller's series keyed by MangaBaka id, for franchise lookups.
pub fn by_mangabaka_id(snapshot: &LibrarySnapshot) -> HashMap<i64, &SnapshotSeries> {
    snapshot
        .series
        .iter()
        .filter_map(|s| {
            let id = s
                .external_ids
                .iter()
                .find(|e| e.provider == shisho_ids::Provider::Mangabaka)?
                .id
                .parse()
                .ok()?;
            Some((id, s))
        })
        .collect()
}

/// A caller's own rating scale, derived from their ratings alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RatingScale {
    /// Their mean, shrunk toward [`NEUTRAL_RATING`] so a handful of ratings
    /// stays sane.
    pub mean: f64,
    /// The neutral point: the shrunk mean less one standard deviation (at
    /// least [`MIN_RATING_SD`]). Generous raters cluster high, so only a
    /// rating clearly below their norm counts against a series; measured
    /// from the plain mean, a generous rater's 80 read as dislike.
    pub origin: f64,
}

impl RatingScale {
    pub fn of(ratings: &[f64]) -> Self {
        Self::with_sd(ratings, 1.0)
    }

    /// The scale with the neutral point `origin_sd` standard deviations below
    /// the mean (the floor applies whenever the offset is non-zero).
    pub fn with_sd(ratings: &[f64], origin_sd: f64) -> Self {
        let n = ratings.len() as f64;
        let mean =
            (ratings.iter().sum::<f64>() + NEUTRAL_RATING * MEAN_SHRINKAGE) / (n + MEAN_SHRINKAGE);
        let sd = if ratings.len() < 2 {
            0.0
        } else {
            let raw_mean = ratings.iter().sum::<f64>() / n;
            (ratings.iter().map(|r| (r - raw_mean).powi(2)).sum::<f64>() / n).sqrt()
        };
        Self {
            mean,
            origin: mean - origin_sd * sd.max(MIN_RATING_SD),
        }
    }

    /// The scale of every rating in `snapshot`.
    pub fn of_snapshot(snapshot: &LibrarySnapshot) -> Self {
        let ratings: Vec<f64> = snapshot
            .series
            .iter()
            .filter_map(|s| s.rating.map(f64::from))
            .collect();
        Self::of(&ratings)
    }

    /// What a rating says about how a series went, on this scale. A low
    /// rating reads as mixed, never as dropped: finishing and rating a
    /// series is not abandoning it.
    pub fn outcome(&self, rating: i32) -> Outcome {
        let rating = f64::from(rating);
        if rating >= self.mean {
            Outcome::Loved
        } else if rating >= self.origin {
            Outcome::Liked
        } else {
            Outcome::Mixed
        }
    }
}

/// How much one of the caller's series says about their taste, if anything.
fn series_weight(series: &SnapshotSeries, origin: f64, params: &TasteParams) -> Option<f64> {
    match (series.rating, series.read_state) {
        (Some(rating), _) => {
            Some(((f64::from(rating) - origin) / params.rating_spread).clamp(-1.0, 1.0))
        }
        (None, ReadState::Completed) => Some(params.completed_unrated),
        (None, ReadState::InProgress) if params.in_progress != 0.0 => Some(params.in_progress),
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

/// Ranks a backlog by [`TasteVector`] affinity, for offline evaluation. The
/// default is the configuration the candidate pools use.
#[derive(Debug, Clone, Default)]
pub struct AffinityRanker {
    pub params: TasteParams,
    pub label: Option<String>,
}

impl crate::eval::Ranker for AffinityRanker {
    fn name(&self) -> &str {
        self.label.as_deref().unwrap_or("affinity")
    }

    fn rank(&self, snapshot: &LibrarySnapshot, backlog: &[&SnapshotSeries]) -> Vec<usize> {
        let mut taste = TasteVector::from_library_with(snapshot, &[], &self.params);
        if self.params.library_specificity {
            taste.apply_library_specificity(snapshot);
        }
        let scores: Vec<f64> = backlog
            .iter()
            .map(|s| taste.affinity(&s.genres, &s.tags, &s.authors))
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
    fn affinity_a_generous_raters_good_ratings_all_count_as_liked() {
        // Ratings clustered high: measured from the plain mean (86) an 80
        // would read as dislike; from mean - sd it is still liked.
        let library = LibrarySnapshot::from_series(vec![
            series("a", &["Mystery"], &[], ReadState::Completed, Some(80)),
            series("b", &["Fantasy"], &[], ReadState::Completed, Some(85)),
            series("c", &["Fantasy"], &[], ReadState::Completed, Some(90)),
            series("d", &["Romance"], &[], ReadState::Completed, Some(90)),
        ]);
        let taste = TasteVector::from_snapshot(&library);
        assert!(taste.term_weight("Mystery") > 0.0);
        assert!(taste.term_weight("Fantasy") > taste.term_weight("Mystery"));
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
    fn rating_scale_maps_ratings_to_outcomes_on_the_callers_own_scale() {
        // A high rater: ratings 76..=92, mean 84.
        let high: Vec<f64> = (0..=16).map(|i| 76.0 + f64::from(i)).collect();
        let scale = RatingScale::of(&high);
        assert!(scale.mean > 80.0 && scale.origin < scale.mean - 7.9);
        assert_eq!(scale.outcome(90), Outcome::Loved);
        assert_eq!(scale.outcome(78), Outcome::Liked);
        assert_eq!(scale.outcome(60), Outcome::Mixed);
        assert_eq!(scale.outcome(1), Outcome::Mixed, "never dropped");

        // A low rater: the same 60 is a good score for them.
        let low: Vec<f64> = (0..=16).map(|i| 40.0 + f64::from(i)).collect();
        assert_eq!(RatingScale::of(&low).outcome(60), Outcome::Loved);
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
    fn franchise_boosts_a_sequel_of_a_finished_favourite() {
        let fav = series("a", &["Fantasy"], &[], ReadState::Completed, Some(95));
        let library = HashMap::from([(1_i64, &fav)]);
        let (shift, anchor) = franchise_adjustment(&[("prequel".into(), 1)], &library);
        assert!(shift > 0.0);
        let anchor = anchor.unwrap();
        assert_eq!(anchor.relation.as_deref(), Some("sequel of"));
        assert_eq!(anchor.your_rating, Some(95));
    }

    #[test]
    fn franchise_demotes_when_the_original_is_unread_unowned_or_disliked() {
        let unread = series("a", &[], &[], ReadState::Untouched, None);
        let disliked = series("b", &[], &[], ReadState::Completed, Some(20));
        let library = HashMap::from([(1_i64, &unread), (2_i64, &disliked)]);
        for links in [
            vec![("prequel".to_owned(), 1)],
            vec![("prequel".to_owned(), 2)],
            vec![("main_story".to_owned(), 99)],
        ] {
            let (shift, anchor) = franchise_adjustment(&links, &library);
            assert!(shift < 0.0, "{links:?}");
            assert!(anchor.is_none());
        }
    }

    #[test]
    fn franchise_ignores_later_entries() {
        let library = HashMap::new();
        let (shift, _) =
            franchise_adjustment(&[("sequel".into(), 5), ("spin_off".into(), 6)], &library);
        assert_eq!(shift, 0.0);
    }

    #[test]
    fn affinity_idf_rare_shared_tag_outweighs_a_common_one() {
        let mut taste = TasteVector::from_snapshot(&LibrarySnapshot::from_series(vec![series(
            "fav",
            &[],
            &["Elf Protagonist", "Shounen"],
            ReadState::Completed,
            Some(90),
        )]));
        let counts = HashMap::from([
            ("elf protagonist".to_owned(), 50_i64),
            ("shounen".to_owned(), 50_000_i64),
        ]);
        taste.apply_specificity(&counts, 300_000);
        let rare = taste.score(&[], &strings(&["Elf Protagonist"]));
        let common = taste.score(&[], &strings(&["Shounen"]));
        assert!(rare.affinity > common.affinity, "{rare:?} vs {common:?}");
    }

    #[test]
    fn affinity_author_shared_with_a_favourite_boosts_and_anchors() {
        let mut fav = series("fav", &["Fantasy"], &[], ReadState::Completed, Some(90));
        fav.authors = vec!["Kanehito Yamada".into()];
        let taste = TasteVector::from_snapshot(&LibrarySnapshot::from_series(vec![fav]));
        let with =
            taste.score_with_authors(&strings(&["Romance"]), &[], &strings(&["kanehito yamada"]));
        let without = taste.score(&strings(&["Romance"]), &[]);
        assert!(with.affinity > without.affinity);
        let anchor = &with.anchors[0];
        assert_eq!(anchor.relation.as_deref(), Some("same author as"));
        assert_eq!(anchor.shared, strings(&["kanehito yamada"]));
    }

    fn outcome(title: &str, outcome: Outcome, genres: &[&str]) -> OutcomeSignal {
        OutcomeSignal {
            title: title.into(),
            codex_series_id: None,
            outcome,
            genres: strings(genres),
            tags: Vec::new(),
            authors: Vec::new(),
        }
    }

    #[test]
    fn outcome_dropped_pushes_its_genre_negative() {
        let library = LibrarySnapshot::from_series(vec![]);
        let taste = TasteVector::from_library(
            &library,
            &[outcome("Gorefest", Outcome::Dropped, &["Horror"])],
        );
        assert!(taste.term_weight("Horror") < 0.0);
        let horror = taste.score(&strings(&["Horror"]), &[]);
        let neutral = taste.score(&strings(&["Sports"]), &[]);
        assert!(horror.affinity < neutral.affinity);
    }

    #[test]
    fn outcome_loved_anchors_with_the_outcome() {
        let library = LibrarySnapshot::from_series(vec![]);
        let taste = TasteVector::from_library(
            &library,
            &[outcome("Kimi ni Todoke", Outcome::Loved, &["Romance"])],
        );
        let score = taste.score(&strings(&["Romance"]), &[]);
        let anchor = &score.anchors[0];
        assert_eq!(anchor.title, "Kimi ni Todoke");
        assert_eq!(anchor.your_outcome.as_deref(), Some("loved"));
        assert_eq!(anchor.codex_series_id, None);
    }

    #[test]
    fn outcome_yields_to_a_rating_of_the_same_series() {
        let rated = series("s1", &["Fantasy"], &[], ReadState::Completed, Some(95));
        let library = LibrarySnapshot::from_series(vec![rated]);
        let mut dropped = outcome("s1", Outcome::Dropped, &["Fantasy"]);
        dropped.codex_series_id = Some("s1".into());
        let taste = TasteVector::from_library(&library, &[dropped]);
        assert!(taste.term_weight("Fantasy") > 0.0, "the 95 rating wins");
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
        let affinity = crate::eval::evaluate(&library, &AffinityRanker::default()).unwrap();
        assert!(
            affinity.mean_rank_fraction < baseline.mean_rank_fraction,
            "affinity {affinity:?} vs baseline {baseline:?}"
        );
        assert!(affinity.recall_at_10 > baseline.recall_at_10);
    }
}
