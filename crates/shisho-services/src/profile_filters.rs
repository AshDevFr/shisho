//! Hard filters declared in the taste profile.
//!
//! Allergens written as prose rely on the model reading them; this block is
//! enforced server-side, so a series carrying a blocked genre or tag never
//! reaches the model from any pool. It lives inside the profile document as a
//! fenced code block, so versioning, diffs and the proposal-approval flow
//! cover it like the rest of the profile:
//!
//! ````markdown
//! ```shisho-filters
//! never: Gore, Harem
//! ```
//! ````
//!
//! One `key: comma, separated, values` line per rule; `never` is the only key
//! today and unknown keys are ignored, so a typo never blocks everything.

/// The fence info string that marks the filter block.
const FENCE_INFO: &str = "shisho-filters";

/// The profile's hard filters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileFilters {
    /// Genres and tags that exclude a series outright (case-insensitive).
    pub never: Vec<String>,
}

impl ProfileFilters {
    /// Parse every `shisho-filters` block in the profile Markdown. No block
    /// means no filters.
    pub fn parse(markdown: &str) -> Self {
        let mut filters = Self::default();
        let mut inside = false;
        for line in markdown.lines() {
            let trimmed = line.trim();
            if !inside {
                if let Some(info) = trimmed.strip_prefix("```")
                    && info.trim().eq_ignore_ascii_case(FENCE_INFO)
                {
                    inside = true;
                }
                continue;
            }
            if trimmed.starts_with("```") {
                inside = false;
                continue;
            }
            let Some((key, values)) = trimmed.split_once(':') else {
                continue;
            };
            if key.trim().eq_ignore_ascii_case("never") {
                for value in values.split(',').map(str::trim).filter(|v| !v.is_empty()) {
                    if !filters.never.iter().any(|v| v.eq_ignore_ascii_case(value)) {
                        filters.never.push(value.to_owned());
                    }
                }
            }
        }
        filters
    }

    /// The first blocked term among a series' genres and tags, if any.
    pub fn blocks<'a>(&self, genres: &'a [String], tags: &'a [String]) -> Option<&'a str> {
        genres
            .iter()
            .chain(tags)
            .find(|term| self.never.iter().any(|n| n.eq_ignore_ascii_case(term)))
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::ProfileFilters;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn profile_filter_parses_the_never_list() {
        let profile = "# Taste\n\nLoves slow fantasy.\n\n```shisho-filters\nnever: Gore, Harem\nsomething-else: ignored\n```\n";
        assert_eq!(ProfileFilters::parse(profile).never, s(&["Gore", "Harem"]));
    }

    #[test]
    fn profile_filter_without_a_block_is_empty() {
        let profile = "# Taste\n\n```\nnever: Gore\n```\nAvoid: Gore";
        assert!(ProfileFilters::parse(profile).never.is_empty());
    }

    #[test]
    fn profile_filter_matches_genres_and_tags_case_insensitively() {
        let filters = ProfileFilters::parse("```shisho-filters\nnever: horror, Elves\n```");
        assert_eq!(filters.blocks(&s(&["Horror"]), &[]), Some("Horror"));
        assert_eq!(filters.blocks(&[], &s(&["elves"])), Some("elves"));
        assert_eq!(filters.blocks(&s(&["Fantasy"]), &s(&["Magic"])), None);
    }
}
