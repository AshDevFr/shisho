//! Fuzzy-title scoring built on the Sørensen–Dice coefficient over
//! character bigrams. Used by the offline store to re-rank FTS5 hits,
//! whose BM25 ordering is unstable for the short-title, high-overlap
//! domain manga titles live in.

use std::collections::HashSet;

/// Dice coefficient between two strings over their character-bigram sets.
/// Both inputs are lowercased and stripped of non-alphanumeric characters
/// before bigram extraction, so "Chainsaw Man" and "chainsawman!" score
/// identically.
pub fn dice(a: &str, b: &str) -> f32 {
    let a_grams = bigrams(a);
    let b_grams = bigrams(b);
    if a_grams.is_empty() || b_grams.is_empty() {
        return if a_grams.is_empty() && b_grams.is_empty() {
            1.0
        } else {
            0.0
        };
    }
    let intersection = a_grams.intersection(&b_grams).count();
    (2.0 * intersection as f32) / (a_grams.len() + b_grams.len()) as f32
}

/// Best Dice score for `query` against every title in `candidates`. Empty
/// candidate set returns `0.0`.
pub fn best_dice<I, S>(query: &str, candidates: I) -> f32
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    candidates
        .into_iter()
        .map(|c| dice(query, c.as_ref()))
        .fold(0.0_f32, f32::max)
}

fn bigrams(s: &str) -> HashSet<[char; 2]> {
    let normalized: Vec<char> = s
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else {
                None
            }
        })
        .collect();
    let mut out = HashSet::with_capacity(normalized.len().saturating_sub(1));
    for window in normalized.windows(2) {
        out.insert([window[0], window[1]]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dice_of_identical_strings_is_one() {
        assert!((dice("Chainsaw Man", "Chainsaw Man") - 1.0).abs() < 1e-6);
    }

    #[test]
    fn dice_is_case_and_punctuation_insensitive() {
        let a = dice("Chainsaw Man", "chainsawman!");
        assert!((a - 1.0).abs() < 1e-6, "got {a}");
    }

    #[test]
    fn closer_titles_score_higher() {
        let exact = dice("Chainsaw Man", "Chainsaw Man");
        let extended = dice("Chainsaw Man", "Chainsaw Man: Buddy Stories");
        let unrelated = dice("Chainsaw Man", "Berserk");
        assert!(exact > extended);
        assert!(extended > unrelated);
    }

    #[test]
    fn best_dice_takes_the_maximum() {
        let score = best_dice("CSM", ["Chainsaw Man", "CSM", "Berserk"]);
        assert!((score - 1.0).abs() < 1e-6);
        assert_eq!(best_dice("anything", Vec::<&str>::new()), 0.0);
    }
}
