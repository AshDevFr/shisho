//! Read-only sea-orm access to an extracted MangaBaka dump.
//!
//! The store opens the SQLite file with the read-only URI option, exposes
//! the three lookups the provider cares about (`find_by_id`,
//! `find_by_source_id`, `search_fts`), and maps each row onto
//! [`SeriesMetadata`].
//!
//! `find_by_source_id` relies on the indexes [`super::setup`] creates after
//! extraction (one per source column); `search_fts` relies on the
//! `series_title_fts` virtual table the same step builds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, FromQueryResult,
    Statement,
};
use serde_json::Value as Json;
use shisho_ids::{ExternalId, Provider};

use crate::mapping::{normalize_rating, parse_count, parse_kind, parse_status};
use crate::scoring::best_dice;
use crate::types::{ContentRating, MetadataOrigin, SearchHit, SeriesMetadata};

/// Maximum number of rows pulled from FTS before Dice re-ranking.
///
/// Has to be wide enough to survive BM25's length-normalization quirks:
/// MangaBaka's canonical "ONE PIECE" row ships ~14k chars of alternate
/// titles, which tanks its FTS rank down to ~position 500 of 600+ matches
/// for the query "one piece". The Dice rerank fixes the order, but only if
/// the row is in the pool. 2000 candidates cover the worst-case popular
/// queries and still re-rank in under 10 ms on local SQLite.
const FUZZY_CANDIDATE_POOL: u32 = 2000;

/// Read-only view over an extracted dump.
pub struct OfflineStore {
    db: DatabaseConnection,
    path: PathBuf,
}

impl OfflineStore {
    /// Open the file at `path` as a read-only sea-orm connection. The file
    /// must already have been [`super::setup::prepare`]d (indexes + FTS).
    /// The `?mode=ro` URI flag prevents accidental writes; SQLite will also
    /// not create a journal/WAL file alongside.
    pub async fn open_ro(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(anyhow!("dump file not found at {}", path.display()));
        }
        let url = format!("sqlite://{}?mode=ro", path.display());
        let mut opts = ConnectOptions::new(&url);
        opts.max_connections(1).sqlx_logging(false);
        let db = Database::connect(opts)
            .await
            .with_context(|| format!("opening dump {}", path.display()))?;
        Ok(Self { db, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Count the active series rows (the figure status surfaces report).
    pub async fn count_active_rows(&self) -> anyhow::Result<u64> {
        let row = self
            .db
            .query_one_raw(Statement::from_string(
                self.db.get_database_backend(),
                "SELECT COUNT(*) AS n FROM series WHERE state = 'active' OR state IS NULL"
                    .to_string(),
            ))
            .await?
            .ok_or_else(|| anyhow!("COUNT(*) returned no row"))?;
        let n: i64 = row.try_get("", "n")?;
        Ok(n.max(0) as u64)
    }

    /// Lookup by MangaBaka series id. Returns `Ok(None)` if no row matches
    /// or the row is marked merged/deleted (both are "not found" — merged
    /// rows live behind their canonical row).
    /// Genres and non-spoiler tags per series, from the `series_terms` side
    /// table (empty when the dump predates it).
    pub async fn terms_for(&self, ids: &[i64]) -> anyhow::Result<HashMap<i64, Vec<SeriesTerm>>> {
        let mut out: HashMap<i64, Vec<SeriesTerm>> = HashMap::new();
        if ids.is_empty() || !self.has_table("series_terms").await? {
            return Ok(out);
        }
        let placeholders = vec!["?"; ids.len()].join(", ");
        let rows = self
            .db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!(
                    "SELECT series_id, term, kind FROM series_terms \
                     WHERE series_id IN ({placeholders}) ORDER BY rowid"
                ),
                ids.iter().map(|&id| id.into()),
            ))
            .await?;
        for row in rows {
            let series_id: i64 = row.try_get("", "series_id")?;
            out.entry(series_id).or_default().push(SeriesTerm {
                term: row.try_get("", "term")?,
                kind: row.try_get("", "kind")?,
            });
        }
        Ok(out)
    }

    /// How many active series carry `term` (case-insensitive), across kinds;
    /// `None` when no series does or the dump predates the side tables.
    pub async fn term_series_count(&self, term: &str) -> anyhow::Result<Option<i64>> {
        if !self.has_table("term_stats").await? {
            return Ok(None);
        }
        let row = self
            .db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT SUM(series_count) AS n FROM term_stats WHERE term = ?1 COLLATE NOCASE",
                [term.into()],
            ))
            .await?;
        Ok(row.and_then(|r| r.try_get::<Option<i64>>("", "n").ok().flatten()))
    }

    /// Active series in the dump: the corpus size for term specificity.
    pub async fn active_series_count(&self) -> anyhow::Result<u64> {
        self.count_active_rows().await
    }

    /// Franchise links of one series as `(kind, related_id)`, where `kind` is
    /// what the related series is to it (`sequel`, `prequel`, `side_story`,
    /// `spin_off`, `main_story`).
    pub async fn relations_for(&self, id: i64) -> anyhow::Result<Vec<(String, i64)>> {
        if !self.has_table("series_relations").await? {
            return Ok(Vec::new());
        }
        let rows = self
            .db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT kind, related_id FROM series_relations WHERE series_id = ?1",
                [id.into()],
            ))
            .await?;
        rows.iter()
            .map(|row| Ok((row.try_get("", "kind")?, row.try_get("", "related_id")?)))
            .collect()
    }

    /// Active series matching weighted terms, best summed weight first (then
    /// community rating), filtered by format, content rating and a rating
    /// floor. With no weights, the best-rated series that pass the filters.
    /// Tags on the results come from the spoiler-free side table.
    pub async fn discover(&self, query: &DiscoverQuery<'_>) -> anyhow::Result<Vec<SeriesMetadata>> {
        if !self.has_table("series_terms").await? {
            return Ok(Vec::new());
        }
        let mut values: Vec<sea_orm::Value> = Vec::new();
        let kinds = vec!["?"; query.kinds.len()].join(", ");
        values.extend(query.kinds.iter().map(|k| (*k).into()));
        let explicit = if query.allow_explicit {
            String::new()
        } else {
            "AND COALESCE(s.content_rating, '') <> 'pornographic'".to_owned()
        };
        let filters = format!(
            "(s.state = 'active' OR s.state IS NULL) AND s.type IN ({kinds}) \
             AND CAST(s.rating AS REAL) >= ? {explicit}"
        );
        values.push(query.min_rating.into());
        let cols = raw_row_columns("s.");
        let sql = if query.weights.is_empty() {
            values.push(i64::from(query.limit).into());
            format!(
                "SELECT {cols} FROM series s WHERE {filters} \
                 ORDER BY CAST(s.rating AS REAL) DESC LIMIT ?"
            )
        } else {
            let mut weights: Vec<sea_orm::Value> = Vec::new();
            let rows = vec!["(?, ?)"; query.weights.len()].join(", ");
            for (term, weight) in query.weights {
                weights.push(term.clone().into());
                weights.push((*weight).into());
            }
            weights.extend(values);
            values = weights;
            values.push(i64::from(query.limit).into());
            format!(
                "WITH w(term, weight) AS (VALUES {rows}) \
                 SELECT {cols} FROM w \
                 JOIN series_terms t ON t.term = w.term \
                 JOIN series s ON s.id = t.series_id \
                 WHERE {filters} \
                 GROUP BY s.id ORDER BY SUM(w.weight) DESC, CAST(s.rating AS REAL) DESC LIMIT ?"
            )
        };
        let rows = RawRow::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            values,
        ))
        .all(&self.db)
        .await?;
        let mut found: Vec<SeriesMetadata> = rows.into_iter().map(row_to_metadata).collect();

        let ids: Vec<i64> = found.iter().map(|m| m.mangabaka_id).collect();
        let terms = self.terms_for(&ids).await?;
        for meta in &mut found {
            if let Some(terms) = terms.get(&meta.mangabaka_id) {
                meta.tags = terms
                    .iter()
                    .filter(|t| t.kind == "tag")
                    .map(|t| t.term.clone())
                    .collect();
            }
        }
        Ok(found)
    }

    async fn has_table(&self, name: &str) -> anyhow::Result<bool> {
        Ok(self
            .db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT name FROM sqlite_master WHERE type='table' AND name=?1",
                [name.into()],
            ))
            .await?
            .is_some())
    }

    pub async fn find_by_id(&self, id: &str) -> anyhow::Result<Option<SeriesMetadata>> {
        let Ok(parsed) = id.parse::<i64>() else {
            return Ok(None);
        };
        let stmt = Statement::from_sql_and_values(
            self.db.get_database_backend(),
            select_by_id_sql(),
            [parsed.into()],
        );
        let row = RawRow::find_by_statement(stmt).one(&self.db).await?;
        Ok(row.and_then(active_row_to_metadata))
    }

    /// Lookup by a foreign source's id. `mb_source` is MangaBaka's column
    /// suffix (e.g. `"manga_updates"`, `"anilist"`), not our canonical
    /// provider token; the caller translates via
    /// [`crate::mapping::provider_to_mb_source`]. Returns `Ok(None)` for
    /// sources the dump has no column for (e.g. `mangadex`).
    pub async fn find_by_source_id(
        &self,
        mb_source: &str,
        external_id: &str,
    ) -> anyhow::Result<Option<SeriesMetadata>> {
        let Some(column) = source_column(mb_source) else {
            return Ok(None);
        };
        let stmt = Statement::from_sql_and_values(
            self.db.get_database_backend(),
            select_by_source_id_sql(column),
            [external_id.into()],
        );
        let row = RawRow::find_by_statement(stmt).one(&self.db).await?;
        Ok(row.and_then(active_row_to_metadata))
    }

    /// FTS5 full-text search over the title columns, then re-rank with the
    /// Sørensen–Dice coefficient.
    ///
    /// FTS5 retrieves the candidate pool (its BM25 rank is fine at picking
    /// "rows that contain these tokens"), but its ordering is unstable for
    /// the short-title, high-overlap domain we live in. So we over-fetch up
    /// to [`FUZZY_CANDIDATE_POOL`] rows and re-rank each by the maximum
    /// Dice score across canonical, native, and romanized titles, returning
    /// the top `limit` with their score.
    ///
    /// Empty query short-circuits to an empty result; we never let the
    /// caller's blank input become a wildcard FTS match.
    pub async fn search_fts(&self, query: &str, limit: u32) -> anyhow::Result<Vec<SearchHit>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.max(1);
        let candidate_pool = FUZZY_CANDIDATE_POOL.max(limit);

        let sanitized = sanitize_match(query);
        let stmt = Statement::from_sql_and_values(
            self.db.get_database_backend(),
            select_fts_sql(),
            [sanitized.into(), (candidate_pool as i64).into()],
        );
        let rows = RawRow::find_by_statement(stmt).all(&self.db).await?;

        let mut scored: Vec<(SearchHit, f32)> = rows
            .into_iter()
            .map(|r| {
                // Score against canonical + native + romanized only. The
                // JSON `titles` blob is intentionally skipped: scoring it
                // would mean parsing serde_json across every candidate
                // (2000 rows × ~400 chars average), and the triple already
                // handles English ↔ romaji ↔ native script.
                let mut titles: Vec<&str> = Vec::with_capacity(3);
                titles.push(r.title.as_str());
                if let Some(s) = r.native_title.as_deref() {
                    titles.push(s);
                }
                if let Some(s) = r.romanized_title.as_deref() {
                    titles.push(s);
                }
                let score = best_dice(query, titles);
                let kind = r.kind.as_deref().map(parse_kind);
                let hit = SearchHit {
                    mangabaka_id: r.id,
                    title: r.title.clone(),
                    year: r.year,
                    cover_url: pick_cover_from_row(&r),
                    kind,
                    score: Some(score),
                };
                (hit, score)
            })
            .collect();

        // Best score first; stable so equal-scoring rows preserve FTS rank.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored
            .into_iter()
            .take(limit as usize)
            .map(|(hit, _)| hit)
            .collect())
    }
}

/// A taste-weighted discovery query over the dump.
#[derive(Debug, Clone)]
pub struct DiscoverQuery<'a> {
    /// Terms (genres or tags, case-insensitive) and their weights; only
    /// positive weights are useful here.
    pub weights: &'a [(String, f64)],
    /// MangaBaka `type` values to include (`manga`, `manhwa`, ...).
    pub kinds: &'a [&'a str],
    /// Include pornographic-rated series.
    pub allow_explicit: bool,
    /// Community rating floor on MangaBaka's 0-100 scale.
    pub min_rating: f64,
    pub limit: u32,
}

/// One genre or tag of a dump series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesTerm {
    pub term: String,
    /// `genre` or `tag`.
    pub kind: String,
}

// Single source of truth for the column list every dump SELECT pulls into
// `RawRow`. The three call sites (`find_by_id`, `find_by_source_id`,
// `search_fts`) all hit the same `series` table; only the surrounding
// WHERE/JOIN differs. Centralizing the columns keeps `RawRow` and the SQL
// in sync: adding a field to `RawRow` means adding it here once.
//
// `prefix` is empty for the unjoined queries and `"s."` for the FTS join.
// It attaches only to physical-column references; aliases (`AS kind`,
// `AS rating`, the `source_*_id` casts) stay unprefixed so `RawRow`
// decodes by stable output names regardless of which query produced the
// row.
//
// `source_*_id` columns are CAST to TEXT because the dump mixes INTEGER
// (anilist, mal, kitsu, shikimori, anime_news_network) and TEXT
// (manga_updates, anime_planet slugs); the cast lets sqlx decode each as
// `Option<String>` uniformly. `type` is aliased to `kind` because
// `FromQueryResult` does not honor `#[sea_orm(column_name = ...)]`.
// `rating` is CAST to REAL because the dump occasionally stores it as
// TEXT alongside the numeric form.
fn raw_row_columns(prefix: &str) -> String {
    let p = prefix;
    format!(
        "{p}id, {p}title, {p}native_title, {p}romanized_title, \
        {p}titles, {p}type AS kind, {p}status, {p}year, {p}final_volume, {p}total_chapters, \
        CAST({p}rating AS REAL) AS rating, \
        {p}description, {p}state, {p}genres, {p}tags, {p}content_rating, {p}authors, \
        {p}cover_x350_x2, {p}cover_x350_x1, {p}cover_x250_x2, {p}cover_x250_x1, {p}cover_raw_url, \
        CAST({p}source_anilist_id AS TEXT) AS source_anilist_id, \
        CAST({p}source_my_anime_list_id AS TEXT) AS source_my_anime_list_id, \
        CAST({p}source_manga_updates_id AS TEXT) AS source_manga_updates_id, \
        CAST({p}source_kitsu_id AS TEXT) AS source_kitsu_id, \
        CAST({p}source_shikimori_id AS TEXT) AS source_shikimori_id, \
        CAST({p}source_anime_planet_id AS TEXT) AS source_anime_planet_id, \
        CAST({p}source_anime_news_network_id AS TEXT) AS source_anime_news_network_id"
    )
}

fn select_by_id_sql() -> String {
    format!(
        "SELECT {cols} FROM series WHERE id = ?1 LIMIT 1",
        cols = raw_row_columns("")
    )
}

/// The CAST in the WHERE clause is what makes the indexes built by
/// `setup::prepare` usable for both TEXT slugs and INTEGER ids.
fn select_by_source_id_sql(column: &str) -> String {
    format!(
        "SELECT {cols} FROM series WHERE CAST({column} AS TEXT) = ?1 LIMIT 1",
        cols = raw_row_columns("")
    )
}

fn select_fts_sql() -> String {
    format!(
        "SELECT {cols} FROM series_title_fts f \
        JOIN series s ON s.id = f.rowid \
        WHERE series_title_fts MATCH ?1 AND (s.state = 'active' OR s.state IS NULL) \
        ORDER BY rank LIMIT ?2",
        cols = raw_row_columns("s.")
    )
}

/// Map MangaBaka's `source` component onto its physical SQLite column.
/// `None` means "the dump has no column for this source" (mangadex); the
/// caller treats it as a miss and may still try the API.
fn source_column(mb_source: &str) -> Option<&'static str> {
    match mb_source {
        "anilist" => Some("source_anilist_id"),
        "my_anime_list" => Some("source_my_anime_list_id"),
        "manga_updates" => Some("source_manga_updates_id"),
        "kitsu" => Some("source_kitsu_id"),
        "shikimori" => Some("source_shikimori_id"),
        "anime_planet" => Some("source_anime_planet_id"),
        "anime_news_network" => Some("source_anime_news_network_id"),
        _ => None,
    }
}

/// Wrap each user-supplied term in double quotes so FTS5 treats it as a
/// phrase literal and the contained punctuation cannot inject syntax.
/// Sequences are joined with implicit AND (FTS5's default operator).
fn sanitize_match(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| {
            // Escape embedded double quotes by doubling them per FTS5 spec.
            let escaped = t.replace('"', "\"\"");
            format!("\"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Mirrors the columns selected by `raw_row_columns`. Source-id columns are
/// read as strings because the schema mixes INTEGER and TEXT (manga_updates
/// and anime_planet use ASCII slugs); the CAST coerces both forms to TEXT.
#[derive(Debug, FromQueryResult)]
struct RawRow {
    id: i64,
    title: String,
    native_title: Option<String>,
    romanized_title: Option<String>,
    titles: Option<String>,
    kind: Option<String>,
    status: Option<String>,
    year: Option<i32>,
    /// Last/final volume number, stored as TEXT in the dump (nullable).
    final_volume: Option<String>,
    /// Total chapter count, stored as TEXT in the dump (nullable).
    total_chapters: Option<String>,
    /// Average user rating on the dump's native 0-100 scale (nullable).
    rating: Option<f64>,
    description: Option<String>,
    state: Option<String>,
    /// JSON array of genre names (e.g. `["Action", "Comedy"]`). The HTTP
    /// API uses the same shape; parsing it back lands the offline and live
    /// paths on the same canonical metadata.
    genres: Option<String>,
    tags: Option<String>,
    content_rating: Option<String>,
    /// JSON array of author names.
    authors: Option<String>,
    cover_x350_x2: Option<String>,
    cover_x350_x1: Option<String>,
    cover_x250_x2: Option<String>,
    cover_x250_x1: Option<String>,
    cover_raw_url: Option<String>,
    source_anilist_id: Option<String>,
    source_my_anime_list_id: Option<String>,
    source_manga_updates_id: Option<String>,
    source_kitsu_id: Option<String>,
    source_shikimori_id: Option<String>,
    source_anime_planet_id: Option<String>,
    source_anime_news_network_id: Option<String>,
}

fn active_row_to_metadata(row: RawRow) -> Option<SeriesMetadata> {
    // Skip merged / deleted rows; resolution should follow the canonical
    // row instead. The dump publishes both sides — we filter out the
    // non-active side here.
    if matches!(row.state.as_deref(), Some("merged") | Some("deleted")) {
        return None;
    }
    Some(row_to_metadata(row))
}

fn row_to_metadata(row: RawRow) -> SeriesMetadata {
    let alternate_titles = collect_alternates(&row);
    let foreign_ids = row_to_foreign_ids(&row);
    let cover_url = pick_cover_from_row(&row);
    let kind = row.kind.as_deref().map(parse_kind);
    let status = row.status.as_deref().map(parse_status);
    let genres = parse_string_array(row.genres.as_deref());
    let tags = parse_string_array(row.tags.as_deref());

    SeriesMetadata {
        mangabaka_id: row.id,
        title: row.title,
        alternate_titles,
        kind,
        status,
        year: row.year,
        cover_url,
        total_volumes: parse_count(row.final_volume.as_deref()),
        total_chapters: parse_count(row.total_chapters.as_deref()),
        rating: normalize_rating(row.rating),
        description: row.description.filter(|s| !s.is_empty()),
        genres,
        tags,
        content_rating: row.content_rating.as_deref().and_then(ContentRating::parse),
        authors: parse_string_array(row.authors.as_deref()),
        foreign_ids,
        origin: MetadataOrigin::Dump,
    }
}

/// Decode a SQLite TEXT cell holding a JSON array of strings. Malformed or
/// missing payloads degrade to an empty list rather than failing the
/// lookup: the row is still useful without genres/tags.
fn parse_string_array(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

fn collect_alternates(row: &RawRow) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: Option<&str>| {
        if let Some(v) = s
            && !v.is_empty()
            && v != row.title
            && !out.iter().any(|e| e == v)
        {
            out.push(v.to_string());
        }
    };
    push(row.native_title.as_deref());
    push(row.romanized_title.as_deref());

    // `titles` is a JSON array of `{title, language, traits, is_primary, note}`.
    if let Some(raw) = row.titles.as_deref()
        && let Ok(Json::Array(arr)) = serde_json::from_str::<Json>(raw)
    {
        for entry in arr {
            if let Some(t) = entry.get("title").and_then(|v| v.as_str()) {
                push(Some(t));
            }
        }
    }
    out
}

fn row_to_foreign_ids(row: &RawRow) -> Vec<ExternalId> {
    let mut out = Vec::new();
    let mut push = |provider: Provider, id: &Option<String>| {
        if let Some(id) = id.as_deref().filter(|s| !s.is_empty()) {
            out.push(ExternalId {
                provider,
                id: id.to_string(),
                provenance: None,
            });
        }
    };
    push(Provider::Anilist, &row.source_anilist_id);
    push(Provider::Mal, &row.source_my_anime_list_id);
    push(Provider::Mangaupdates, &row.source_manga_updates_id);
    push(Provider::Kitsu, &row.source_kitsu_id);
    push(Provider::Shikimori, &row.source_shikimori_id);
    push(Provider::AnimePlanet, &row.source_anime_planet_id);
    push(
        Provider::AnimeNewsNetwork,
        &row.source_anime_news_network_id,
    );
    out
}

fn pick_cover_from_row(row: &RawRow) -> Option<String> {
    row.cover_x350_x2
        .clone()
        .or_else(|| row.cover_x350_x1.clone())
        .or_else(|| row.cover_x250_x2.clone())
        .or_else(|| row.cover_x250_x1.clone())
        .or_else(|| row.cover_raw_url.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SeriesKind, SeriesStatus};
    use sea_orm::{ConnectionTrait, Database, Statement};

    /// Build an in-memory SQLite mimicking the dump schema (subset of
    /// columns) so the store can be exercised without downloading 3 GB.
    async fn fixture_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let backend = db.get_database_backend();
        db.execute_raw(Statement::from_string(
            backend,
            "CREATE TABLE series (
                id INTEGER PRIMARY KEY,
                title TEXT,
                native_title TEXT,
                romanized_title TEXT,
                titles TEXT,
                type TEXT,
                status TEXT,
                year INTEGER,
                final_volume TEXT,
                total_chapters TEXT,
                rating REAL,
                description TEXT,
                state TEXT,
                genres TEXT,
                tags TEXT,
                cover_x350_x2 TEXT,
                cover_x350_x1 TEXT,
                cover_x250_x2 TEXT,
                cover_x250_x1 TEXT,
                cover_raw_url TEXT,
                source_anilist_id INTEGER,
                source_my_anime_list_id INTEGER,
                source_manga_updates_id TEXT,
                source_kitsu_id INTEGER,
                source_shikimori_id INTEGER,
                source_anime_planet_id TEXT,
                source_anime_news_network_id INTEGER,
                content_rating TEXT,
                authors TEXT
            )"
            .to_string(),
        ))
        .await
        .unwrap();
        // Chainsaw Man (active) and a merged duplicate to exercise filtering.
        db.execute_raw(Statement::from_string(
            backend,
            "INSERT INTO series (
                id, title, native_title, romanized_title, titles, type, status, year,
                final_volume, total_chapters, rating, description, state,
                genres, tags, content_rating, authors,
                cover_x350_x2, source_anilist_id, source_my_anime_list_id,
                source_manga_updates_id, source_kitsu_id, source_anime_planet_id
            ) VALUES (
                1677, 'Chainsaw Man', 'チェンソーマン', 'Chainsaw Man',
                '[{\"title\":\"Chainsaw-Man\",\"language\":\"en\"},{\"title\":\"CSM\",\"language\":\"en\"}]',
                'manga', 'releasing', 2018, '24', '232', 85.0, 'Denji has a devil dog.', 'active',
                '[\"Action\", \"Horror\"]', '[\"Chainsaws\", \"Devils\"]',
                'suggestive', '[\"Tatsuki Fujimoto\"]',
                'https://mb/350@2x.jpg', 105778, 116778,
                'ylx5wzn', 54139, 'chainsaw-man'
            )"
            .to_string(),
        ))
        .await
        .unwrap();
        db.execute_raw(Statement::from_string(
            backend,
            "INSERT INTO series (
                id, title, type, state, source_anilist_id
            ) VALUES (
                9999, 'Chainsaw Man (merged)', 'manga', 'merged', 105778
            )"
            .to_string(),
        ))
        .await
        .unwrap();
        // FTS5 virtual table mirroring what setup::prepare builds.
        db.execute_raw(Statement::from_string(
            backend,
            "CREATE VIRTUAL TABLE series_title_fts USING fts5(
                title, native_title, romanized_title, alternate_titles, tokenize = 'unicode61'
            )"
            .to_string(),
        ))
        .await
        .unwrap();
        db.execute_raw(Statement::from_string(
            backend,
            "INSERT INTO series_title_fts(rowid, title, native_title, romanized_title, alternate_titles)
                VALUES (1677, 'Chainsaw Man', 'チェンソーマン', 'Chainsaw Man', 'Chainsaw-Man CSM')"
                .to_string(),
        ))
        .await
        .unwrap();
        db
    }

    fn store_from(db: DatabaseConnection) -> OfflineStore {
        OfflineStore {
            db,
            path: PathBuf::from(":memory:"),
        }
    }

    #[tokio::test]
    async fn find_by_id_returns_canonical_metadata() {
        let store = store_from(fixture_db().await);
        let m = store.find_by_id("1677").await.unwrap().unwrap();
        assert_eq!(m.mangabaka_id, 1677);
        assert_eq!(m.title, "Chainsaw Man");
        assert_eq!(m.kind, Some(SeriesKind::Manga));
        assert_eq!(m.status, Some(SeriesStatus::Ongoing));
        assert_eq!(m.year, Some(2018));
        assert_eq!(m.cover_url.as_deref(), Some("https://mb/350@2x.jpg"));
        assert_eq!(m.description.as_deref(), Some("Denji has a devil dog."));
        assert!(
            m.alternate_titles.contains(&"チェンソーマン".to_string()),
            "expected native title in alternates, got {:?}",
            m.alternate_titles
        );
        assert!(
            m.alternate_titles.contains(&"CSM".to_string()),
            "expected JSON-array alternates to be merged in"
        );
        assert_eq!(m.genres, vec!["Action".to_string(), "Horror".to_string()]);
        assert_eq!(m.tags, vec!["Chainsaws".to_string(), "Devils".to_string()]);
        assert_eq!(m.content_rating, Some(ContentRating::Suggestive));
        assert_eq!(m.authors, vec!["Tatsuki Fujimoto".to_string()]);
        assert_eq!(m.total_volumes, Some(24));
        assert_eq!(m.total_chapters, Some(232));
        // Rating is normalized from MangaBaka's 0-100 scale to 0-10.
        assert_eq!(m.rating, Some(8.5));
        assert_eq!(m.origin, MetadataOrigin::Dump);
        // Foreign IDs are typed canonical pairs.
        assert!(
            m.foreign_ids
                .iter()
                .any(|f| f.provider == Provider::Mal && f.id == "116778")
        );
        assert!(
            m.foreign_ids
                .iter()
                .any(|f| f.provider == Provider::Mangaupdates && f.id == "ylx5wzn")
        );
    }

    #[test]
    fn parse_string_array_handles_malformed_payloads() {
        assert!(parse_string_array(None).is_empty());
        assert!(parse_string_array(Some("")).is_empty());
        assert!(parse_string_array(Some("   ")).is_empty());
        assert!(parse_string_array(Some("not-json")).is_empty());
        assert_eq!(
            parse_string_array(Some(r#"["A","B"]"#)),
            vec!["A".to_string(), "B".to_string()],
        );
    }

    #[tokio::test]
    async fn find_by_id_skips_merged_rows() {
        let store = store_from(fixture_db().await);
        assert!(store.find_by_id("9999").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn find_by_id_returns_none_for_non_numeric_input() {
        let store = store_from(fixture_db().await);
        assert!(store.find_by_id("not-a-number").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn count_active_rows_excludes_merged() {
        let store = store_from(fixture_db().await);
        assert_eq!(store.count_active_rows().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn find_by_source_id_resolves_text_slug_and_integer_ids() {
        let store = store_from(fixture_db().await);
        // Slug form (manga_updates uses ASCII slugs).
        let m = store
            .find_by_source_id("manga_updates", "ylx5wzn")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.mangabaka_id, 1677);
        // The full denormalized payload must come back, not just the id.
        assert_eq!(m.title, "Chainsaw Man");
        assert_eq!(m.total_volumes, Some(24));
        assert_eq!(m.total_chapters, Some(232));
        assert_eq!(m.rating, Some(8.5));
        // Integer form (anilist).
        let m = store
            .find_by_source_id("anilist", "105778")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.mangabaka_id, 1677);
        // Sources without a dump column return Ok(None) without error.
        assert!(
            store
                .find_by_source_id("mangadex", "abc")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .find_by_source_id("not_a_source", "1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn search_fts_matches_canonical_and_alternate_titles() {
        let store = store_from(fixture_db().await);
        let hits = store.search_fts("Chainsaw", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].mangabaka_id, 1677);
        assert!(
            hits[0].score.is_some(),
            "Dice rescoring should populate score on every hit"
        );

        let hits = store.search_fts("CSM", 10).await.unwrap();
        assert_eq!(hits.len(), 1);

        let hits = store.search_fts("nonexistent", 10).await.unwrap();
        assert!(hits.is_empty());

        let hits = store.search_fts("   ", 10).await.unwrap();
        assert!(hits.is_empty(), "blank query should short-circuit");
    }

    /// Two rows that both pass the FTS predicate but differ in how close
    /// their canonical title is to the query: the Dice rerank must place
    /// the closer match first regardless of insertion / FTS rank order.
    #[tokio::test]
    async fn search_fts_reranks_candidates_by_dice() {
        let db = fixture_db().await;
        let backend = db.get_database_backend();
        db.execute_raw(Statement::from_string(
            backend,
            "INSERT INTO series (
                id, title, type, state, source_anilist_id
            ) VALUES (
                42, 'Chainsaw Man: Buddy Stories', 'manga', 'active', 999
            )"
            .to_string(),
        ))
        .await
        .unwrap();
        db.execute_raw(Statement::from_string(
            backend,
            "INSERT INTO series_title_fts(rowid, title, native_title, romanized_title, alternate_titles)
                VALUES (42, 'Chainsaw Man: Buddy Stories', '', '', '')"
                .to_string(),
        ))
        .await
        .unwrap();
        let store = store_from(db);

        let hits = store.search_fts("Chainsaw Man", 10).await.unwrap();
        assert_eq!(hits.len(), 2, "both candidates should appear");
        assert_eq!(
            hits[0].mangabaka_id, 1677,
            "the closer canonical-title match should rank first by Dice"
        );
        let s0 = hits[0].score.unwrap();
        let s1 = hits[1].score.unwrap();
        assert!(s0 >= s1, "scores must be descending, got {s0} then {s1}");
    }

    #[test]
    fn source_column_translates_known_sources() {
        assert_eq!(
            source_column("manga_updates"),
            Some("source_manga_updates_id")
        );
        assert_eq!(source_column("anilist"), Some("source_anilist_id"));
        assert_eq!(source_column("mangadex"), None); // not in dump
    }

    #[test]
    fn sanitize_match_wraps_terms_in_phrase_quotes() {
        assert_eq!(sanitize_match("chainsaw man"), "\"chainsaw\" \"man\"");
        assert_eq!(
            sanitize_match("OR AND foo"),
            "\"OR\" \"AND\" \"foo\"",
            "operator words must be neutralized as phrase literals"
        );
        // Embedded quotes are escaped per FTS5 syntax (doubled inside the
        // phrase literal): input token `"world"` → `"""world"""`.
        assert_eq!(sanitize_match(r#"hello "world""#), r#""hello" """world""""#,);
    }
}
