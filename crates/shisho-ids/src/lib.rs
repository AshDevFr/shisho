//! ID normalization: the one mapping Shisho owns.
//!
//! Codex stores external source IDs as `plugin:<source>:<id>` or
//! `api:<source>:<id>`, where the first token is *provenance* (how the
//! mapping was established), not identity. Tsundoku stores `<source>:<id>`
//! with exact lowercase canonical tokens. Shisho's canonical internal form is
//! the typed pair `(Provider, id)` using Tsundoku's tokens (the strictest
//! contract); provenance is retained separately and never joins.
//!
//! Adding a provider means adding an enum variant: every match in this module
//! is exhaustive, so the compiler walks you through the rest.

use std::fmt;

/// External metadata providers, named by Tsundoku's canonical tokens.
/// Unknown sources land in `Other` and are excluded from joins.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Provider {
    Mangabaka,
    Mangaupdates,
    Mal,
    Anilist,
    Mangadex,
    Kitsu,
    AnimePlanet,
    AnimeNewsNetwork,
    Shikimori,
    Other(String),
}

impl Provider {
    /// The verbatim Tsundoku token (`mal`, `anime_planet`, ...). `None` for
    /// `Other`: unknown providers never reach a Tsundoku query.
    pub fn as_tsundoku_token(&self) -> Option<&str> {
        match self {
            Provider::Mangabaka => Some("mangabaka"),
            Provider::Mangaupdates => Some("mangaupdates"),
            Provider::Mal => Some("mal"),
            Provider::Anilist => Some("anilist"),
            Provider::Mangadex => Some("mangadex"),
            Provider::Kitsu => Some("kitsu"),
            Provider::AnimePlanet => Some("anime_planet"),
            Provider::AnimeNewsNetwork => Some("anime_news_network"),
            Provider::Shikimori => Some("shikimori"),
            Provider::Other(_) => None,
        }
    }

    /// Parse a Tsundoku token. Tsundoku's tokens are already canonical, so
    /// this is the identity table; anything else is `Other`.
    pub fn from_tsundoku_token(token: &str) -> Self {
        match token {
            "mangabaka" => Provider::Mangabaka,
            "mangaupdates" => Provider::Mangaupdates,
            "mal" => Provider::Mal,
            "anilist" => Provider::Anilist,
            "mangadex" => Provider::Mangadex,
            "kitsu" => Provider::Kitsu,
            "anime_planet" => Provider::AnimePlanet,
            "anime_news_network" => Provider::AnimeNewsNetwork,
            "shikimori" => Provider::Shikimori,
            other => Provider::Other(other.to_owned()),
        }
    }

    /// Map a Codex source name to a provider (the Codex-to-Tsundoku
    /// conversion table: three renames, the rest identity).
    pub fn from_codex_source(source: &str) -> Self {
        match source {
            "myanimelist" => Provider::Mal,
            "animeplanet" => Provider::AnimePlanet,
            "animenewsnetwork" => Provider::AnimeNewsNetwork,
            "mangabaka" => Provider::Mangabaka,
            "mangaupdates" => Provider::Mangaupdates,
            "anilist" => Provider::Anilist,
            "mangadex" => Provider::Mangadex,
            "kitsu" => Provider::Kitsu,
            "shikimori" => Provider::Shikimori,
            other => Provider::Other(other.to_owned()),
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_tsundoku_token() {
            Some(token) => f.write_str(token),
            None => match self {
                Provider::Other(name) => write!(f, "other({name})"),
                _ => unreachable!("every known provider has a token"),
            },
        }
    }
}

/// How a Codex external-ID mapping was established. Diagnostic only — never
/// part of the join key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Established by a Codex metadata plugin (`plugin:` prefix).
    Plugin,
    /// Established via a Codex API match (`api:` prefix).
    Api,
}

/// A normalized external series identity: the canonical `(provider, id)`
/// join key plus optional Codex provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalId {
    pub provider: Provider,
    pub id: String,
    pub provenance: Option<Provenance>,
}

/// The input string did not contain a `<source>:<id>` pair.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed external id: {input:?} (expected [plugin:|api:]<source>:<id>)")]
pub struct InvalidExternalId {
    pub input: String,
}

impl ExternalId {
    /// Parse Codex notation: `plugin:<source>:<id>`, `api:<source>:<id>`, or
    /// bare `<source>:<id>`. The provenance prefix is stripped at this parse
    /// boundary and retained only as a diagnostic field. Unknown sources
    /// parse to `Provider::Other` (logged; excluded from joins).
    pub fn from_codex(raw: &str) -> Result<Self, InvalidExternalId> {
        let (provenance, rest) = match raw.split_once(':') {
            Some(("plugin", rest)) => (Some(Provenance::Plugin), rest),
            Some(("api", rest)) => (Some(Provenance::Api), rest),
            _ => (None, raw),
        };

        let (source, id) = rest.split_once(':').ok_or_else(|| InvalidExternalId {
            input: raw.to_owned(),
        })?;
        if source.is_empty() || id.is_empty() {
            return Err(InvalidExternalId {
                input: raw.to_owned(),
            });
        }

        let provider = Provider::from_codex_source(&source.to_ascii_lowercase());
        if let Provider::Other(name) = &provider {
            tracing::debug!(source = %name, "unknown Codex external-id source; excluded from joins");
        }

        Ok(Self {
            provider,
            id: id.to_owned(),
            provenance,
        })
    }

    /// Parse Codex's two-field form, as returned by its API: `source` is
    /// `plugin:<source>`, `api:<source>`, or a bare source name, and the id
    /// arrives separately. Same semantics as [`ExternalId::from_codex`].
    pub fn from_codex_parts(source: &str, id: &str) -> Result<Self, InvalidExternalId> {
        if id.is_empty() {
            return Err(InvalidExternalId {
                input: format!("{source}:{id}"),
            });
        }
        let (provenance, source_name) = match source.split_once(':') {
            Some(("plugin", rest)) => (Some(Provenance::Plugin), rest),
            Some(("api", rest)) => (Some(Provenance::Api), rest),
            _ => (None, source),
        };
        if source_name.is_empty() {
            return Err(InvalidExternalId {
                input: format!("{source}:{id}"),
            });
        }

        let provider = Provider::from_codex_source(&source_name.to_ascii_lowercase());
        if let Provider::Other(name) = &provider {
            tracing::debug!(source = %name, "unknown Codex external-id source; excluded from joins");
        }

        Ok(Self {
            provider,
            id: id.to_owned(),
            provenance,
        })
    }

    /// Parse a Tsundoku external-ID string (`<token>:<id>`, verbatim
    /// lowercase tokens). No provenance: Tsundoku doesn't carry one.
    pub fn from_tsundoku(raw: &str) -> Result<Self, InvalidExternalId> {
        let (token, id) = raw.split_once(':').ok_or_else(|| InvalidExternalId {
            input: raw.to_owned(),
        })?;
        if token.is_empty() || id.is_empty() {
            return Err(InvalidExternalId {
                input: raw.to_owned(),
            });
        }
        Ok(Self {
            provider: Provider::from_tsundoku_token(token),
            id: id.to_owned(),
            provenance: None,
        })
    }

    /// The verbatim Tsundoku form (`mal:123`). `None` for `Other`: the feed
    /// API rejects non-canonical tokens, so unknown providers never leave
    /// Shisho.
    pub fn as_tsundoku(&self) -> Option<String> {
        self.provider
            .as_tsundoku_token()
            .map(|token| format!("{token}:{}", self.id))
    }

    /// The canonical join key. Provenance is deliberately absent.
    pub fn key(&self) -> (&Provider, &str) {
        (&self.provider, &self.id)
    }

    /// The canonical `<token>:<id>` string every cross-system map is keyed
    /// by: the Tsundoku form for known providers, `<name>:<id>` otherwise.
    pub fn canonical_key(&self) -> String {
        self.as_tsundoku()
            .unwrap_or_else(|| format!("{}:{}", self.provider, self.id))
    }

    /// A known provider: one that participates in joins (`Other` never does).
    pub fn is_known(&self) -> bool {
        !matches!(self.provider, Provider::Other(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_key_uses_the_tsundoku_form_for_known_providers() {
        let mal = ExternalId::from_codex("plugin:myanimelist:42").unwrap();
        assert_eq!(mal.canonical_key(), "mal:42");
        assert!(mal.is_known());
        let other = ExternalId::from_codex("plugin:weird:7").unwrap();
        assert!(!other.is_known());
        assert!(other.canonical_key().ends_with(":7"));
    }

    /// The full PRD §2.3 table: (Codex source, Tsundoku token).
    const CONVERSION_TABLE: &[(&str, &str)] = &[
        ("mangabaka", "mangabaka"),
        ("anilist", "anilist"),
        ("myanimelist", "mal"),
        ("mangaupdates", "mangaupdates"),
        ("mangadex", "mangadex"),
        ("kitsu", "kitsu"),
        ("shikimori", "shikimori"),
        ("animeplanet", "anime_planet"),
        ("animenewsnetwork", "anime_news_network"),
    ];

    #[test]
    fn full_conversion_table_round_trips() {
        for (codex_source, tsundoku_token) in CONVERSION_TABLE {
            // Codex notation -> canonical -> Tsundoku notation.
            let parsed = ExternalId::from_codex(&format!("plugin:{codex_source}:42")).unwrap();
            assert_eq!(
                parsed.as_tsundoku().as_deref(),
                Some(format!("{tsundoku_token}:42").as_str()),
                "codex source {codex_source} should emit token {tsundoku_token}"
            );

            // Tsundoku notation -> canonical -> same provider as via Codex.
            let from_tsundoku = ExternalId::from_tsundoku(&format!("{tsundoku_token}:42")).unwrap();
            assert_eq!(
                from_tsundoku.provider, parsed.provider,
                "both directions must land on the same Provider for {tsundoku_token}"
            );

            // Token emission is verbatim (round-trip through the enum).
            assert_eq!(
                from_tsundoku.as_tsundoku().as_deref(),
                Some(format!("{tsundoku_token}:42").as_str())
            );
        }
    }

    #[test]
    fn provenance_prefixes_are_stripped_and_retained() {
        let plugin = ExternalId::from_codex("plugin:myanimelist:123").unwrap();
        assert_eq!(plugin.provider, Provider::Mal);
        assert_eq!(plugin.id, "123");
        assert_eq!(plugin.provenance, Some(Provenance::Plugin));

        let api = ExternalId::from_codex("api:myanimelist:123").unwrap();
        assert_eq!(api.provenance, Some(Provenance::Api));

        let bare = ExternalId::from_codex("myanimelist:123").unwrap();
        assert_eq!(bare.provider, Provider::Mal);
        assert_eq!(bare.provenance, None);

        // Provenance never participates in identity.
        assert_eq!(plugin.key(), api.key());
        assert_eq!(plugin.key(), bare.key());
    }

    #[test]
    fn unknown_source_parses_to_other_and_never_reaches_tsundoku() {
        let parsed = ExternalId::from_codex("plugin:fandom:abc").unwrap();
        assert_eq!(parsed.provider, Provider::Other("fandom".into()));
        assert_eq!(parsed.as_tsundoku(), None);

        let from_tsundoku = ExternalId::from_tsundoku("notaprovider:1").unwrap();
        assert_eq!(
            from_tsundoku.provider,
            Provider::Other("notaprovider".into())
        );
        assert_eq!(from_tsundoku.as_tsundoku(), None);
    }

    #[test]
    fn codex_two_field_form_parses_like_the_combined_form() {
        // Codex's API carries (source, externalId) as separate fields, with
        // the provenance prefix folded into `source`.
        let parts = ExternalId::from_codex_parts("plugin:myanimelist", "123").unwrap();
        let combined = ExternalId::from_codex("plugin:myanimelist:123").unwrap();
        assert_eq!(parts, combined);

        let bare = ExternalId::from_codex_parts("mangadex", "abc-def").unwrap();
        assert_eq!(bare.provider, Provider::Mangadex);
        assert_eq!(bare.provenance, None);

        // Non-provider sources (Codex also records comicinfo/epub origins).
        let other = ExternalId::from_codex_parts("comicinfo", "x").unwrap();
        assert_eq!(other.provider, Provider::Other("comicinfo".into()));
        assert_eq!(other.as_tsundoku(), None);

        assert!(ExternalId::from_codex_parts("", "1").is_err());
        assert!(ExternalId::from_codex_parts("plugin:", "1").is_err());
        assert!(ExternalId::from_codex_parts("mal", "").is_err());
    }

    #[test]
    fn ids_may_contain_colons() {
        // Only the provenance and source segments are structural; the id is
        // everything after them.
        let parsed = ExternalId::from_codex("plugin:mangadex:abc:def").unwrap();
        assert_eq!(parsed.provider, Provider::Mangadex);
        assert_eq!(parsed.id, "abc:def");
        assert_eq!(parsed.as_tsundoku().as_deref(), Some("mangadex:abc:def"));
    }

    #[test]
    fn codex_sources_are_case_insensitive() {
        let parsed = ExternalId::from_codex("plugin:MyAnimeList:123").unwrap();
        assert_eq!(parsed.provider, Provider::Mal);
    }

    #[test]
    fn malformed_inputs_are_rejected() {
        for input in [
            "",
            "mal",
            "plugin:",
            "plugin:mal",
            ":123",
            "mal:",
            "plugin::123",
        ] {
            assert!(
                ExternalId::from_codex(input).is_err(),
                "{input:?} should be rejected"
            );
        }
        for input in ["", "mal", ":123", "mal:"] {
            assert!(
                ExternalId::from_tsundoku(input).is_err(),
                "{input:?} should be rejected"
            );
        }
    }

    #[test]
    fn display_uses_tsundoku_tokens() {
        assert_eq!(Provider::AnimeNewsNetwork.to_string(), "anime_news_network");
        assert_eq!(
            Provider::Other("fandom".into()).to_string(),
            "other(fandom)"
        );
    }
}
