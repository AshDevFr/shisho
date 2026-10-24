//! Post-extraction setup: add the indexes and FTS5 virtual table that
//! [`super::store::OfflineStore`] expects.
//!
//! The dump as published has no indexes on the source-id columns and no
//! FTS table. Adding them once per refresh costs a few minutes on 585k
//! rows but turns a sequential scan into an index lookup for every
//! foreign-id resolution. Run on the extracted file *before* renaming it
//! into the live `series.sqlite` slot.

use std::path::Path;

use anyhow::{Context, Result};
use sea_orm::{ConnectOptions, ConnectionTrait, Database, Statement};

const INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_series_anilist \
        ON series(source_anilist_id) WHERE source_anilist_id IS NOT NULL",
    "CREATE INDEX IF NOT EXISTS idx_series_mal \
        ON series(source_my_anime_list_id) WHERE source_my_anime_list_id IS NOT NULL",
    "CREATE INDEX IF NOT EXISTS idx_series_mangaupdates \
        ON series(source_manga_updates_id) WHERE source_manga_updates_id IS NOT NULL",
    "CREATE INDEX IF NOT EXISTS idx_series_kitsu \
        ON series(source_kitsu_id) WHERE source_kitsu_id IS NOT NULL",
    "CREATE INDEX IF NOT EXISTS idx_series_shikimori \
        ON series(source_shikimori_id) WHERE source_shikimori_id IS NOT NULL",
    "CREATE INDEX IF NOT EXISTS idx_series_anime_planet \
        ON series(source_anime_planet_id) WHERE source_anime_planet_id IS NOT NULL",
    "CREATE INDEX IF NOT EXISTS idx_series_ann \
        ON series(source_anime_news_network_id) WHERE source_anime_news_network_id IS NOT NULL",
    "CREATE INDEX IF NOT EXISTS idx_series_state ON series(state)",
];

/// The relationship columns read into `series_relations`, by kind.
const RELATION_KINDS: &[&str] = &["sequel", "prequel", "side_story", "spin_off", "main_story"];

async fn table_exists(db: &sea_orm::DatabaseConnection, name: &str) -> Result<bool> {
    let check = Statement::from_sql_and_values(
        db.get_database_backend(),
        "SELECT name FROM sqlite_master WHERE type='table' AND name=?1",
        [name.into()],
    );
    Ok(db.query_one_raw(check).await?.is_some())
}

async fn series_columns(db: &sea_orm::DatabaseConnection) -> Result<Vec<String>> {
    let rows = db
        .query_all_raw(Statement::from_string(
            db.get_database_backend(),
            "SELECT name FROM pragma_table_info('series')".to_string(),
        ))
        .await?;
    rows.iter()
        .map(|row| row.try_get::<String>("", "name").map_err(Into::into))
        .collect()
}

/// Indexed side tables over the dump's JSON columns, for taste-based
/// discovery and franchise logic: `series_terms` (genres and non-spoiler
/// structured tags of active series), `term_stats` (how many series carry
/// each term, for specificity weighting), and `series_relations` (sequel,
/// prequel, side story, spin-off, main story links). Columns an older dump
/// lacks are skipped, leaving the tables empty rather than failing.
async fn build_side_tables(db: &sea_orm::DatabaseConnection) -> Result<()> {
    if table_exists(db, "series_terms").await? {
        return Ok(());
    }
    let backend = db.get_database_backend();
    let columns = series_columns(db).await?;
    let has = |c: &str| columns.iter().any(|x| x == c);
    let active = "(s.state = 'active' OR s.state IS NULL)";

    let mut statements: Vec<String> = vec![
        "CREATE TABLE series_terms (
            series_id INTEGER NOT NULL,
            term TEXT NOT NULL COLLATE NOCASE,
            kind TEXT NOT NULL
        )"
        .into(),
    ];
    if has("genres") {
        statements.push(format!(
            "INSERT INTO series_terms (series_id, term, kind)
             SELECT s.id, j.value, 'genre' FROM series s, json_each(s.genres) j
             WHERE {active} AND json_valid(s.genres) AND j.type = 'text'"
        ));
    }
    if has("tags_v2") {
        statements.push(format!(
            "INSERT INTO series_terms (series_id, term, kind)
             SELECT s.id, json_extract(j.value, '$.name'), 'tag'
             FROM series s, json_each(s.tags_v2) j
             WHERE {active} AND json_valid(s.tags_v2)
               AND json_extract(j.value, '$.name') IS NOT NULL
               AND COALESCE(json_extract(j.value, '$.is_spoiler'), 0) = 0"
        ));
    } else if has("tags") {
        statements.push(format!(
            "INSERT INTO series_terms (series_id, term, kind)
             SELECT s.id, j.value, 'tag' FROM series s, json_each(s.tags) j
             WHERE {active} AND json_valid(s.tags) AND j.type = 'text'"
        ));
    }
    statements.extend([
        "CREATE INDEX idx_series_terms_term ON series_terms(term)".into(),
        "CREATE INDEX idx_series_terms_series ON series_terms(series_id)".into(),
        "CREATE TABLE term_stats AS
             SELECT term, kind, COUNT(DISTINCT series_id) AS series_count
             FROM series_terms GROUP BY term, kind"
            .into(),
        "CREATE INDEX idx_term_stats_term ON term_stats(term COLLATE NOCASE)".into(),
        "CREATE TABLE series_relations (
            series_id INTEGER NOT NULL,
            kind TEXT NOT NULL,
            related_id INTEGER NOT NULL
        )"
        .into(),
    ]);
    for kind in RELATION_KINDS {
        let column = format!("relationships_{kind}");
        if has(&column) {
            statements.push(format!(
                "INSERT INTO series_relations (series_id, kind, related_id)
                 SELECT s.id, '{kind}', j.value FROM series s, json_each(s.{column}) j
                 WHERE {active} AND json_valid(s.{column}) AND j.type = 'integer'"
            ));
        }
    }
    statements.extend([
        "CREATE INDEX idx_series_relations_series ON series_relations(series_id)".into(),
        "CREATE INDEX idx_series_relations_related ON series_relations(related_id)".into(),
    ]);

    tracing::info!("building dump side tables (terms, term stats, relations)");
    for stmt in statements {
        db.execute_raw(Statement::from_string(backend, stmt.clone()))
            .await
            .with_context(|| format!("building side tables via {stmt}"))?;
    }
    Ok(())
}

/// Build indexes + FTS5 mirror on the extracted dump. Idempotent: a dump
/// that already went through `prepare` once is detected via the FTS5 table
/// presence and skipped.
pub async fn prepare(dump_path: impl AsRef<Path>) -> Result<()> {
    let path = dump_path.as_ref();
    if !path.exists() {
        anyhow::bail!("dump file not found at {}", path.display());
    }
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let mut opts = ConnectOptions::new(&url);
    opts.max_connections(1).sqlx_logging(false);
    let db = Database::connect(opts)
        .await
        .with_context(|| format!("opening dump for setup {}", path.display()))?;

    let backend = db.get_database_backend();

    // The side tables are their own idempotent stage, so a dump prepared
    // before they existed gains them on its next preparation.
    build_side_tables(&db).await?;

    // Skip re-running if FTS already exists.
    if table_exists(&db, "series_title_fts").await? {
        tracing::debug!("dump already prepared; skipping setup");
        return Ok(());
    }

    tracing::info!("preparing dump (indexes + FTS5); may take a few minutes");

    for stmt in INDEXES {
        db.execute_raw(Statement::from_string(backend, (*stmt).to_string()))
            .await
            .with_context(|| format!("creating index via {stmt}"))?;
    }

    // Contentless FTS5: the dump stays unchanged, FTS5 stores only the
    // tokens. tokenize='unicode61 remove_diacritics 2' folds Latin
    // diacritics so accented romanizations match the bare ASCII query.
    db.execute_raw(Statement::from_string(
        backend,
        "CREATE VIRTUAL TABLE series_title_fts USING fts5(
            title,
            native_title,
            romanized_title,
            alternate_titles,
            tokenize = 'unicode61 remove_diacritics 2'
        )"
        .to_string(),
    ))
    .await
    .context("creating series_title_fts")?;

    // Populate from active rows only. `titles` is a JSON array of
    // `{title, language, traits, is_primary, note}`. We extract every
    // contained title (excluding the canonical title to avoid duplicate
    // tokens) and stuff the lot into the `alternate_titles` FTS column.
    db.execute_raw(Statement::from_string(
        backend,
        "INSERT INTO series_title_fts(rowid, title, native_title, romanized_title, alternate_titles)
         SELECT
             s.id,
             COALESCE(s.title, ''),
             COALESCE(s.native_title, ''),
             COALESCE(s.romanized_title, ''),
             COALESCE(
                 (SELECT GROUP_CONCAT(t.value, ' ')
                  FROM json_each(s.titles) AS j,
                       (SELECT json_extract(j.value, '$.title') AS value FROM json_each(s.titles) AS j) AS t
                  WHERE t.value IS NOT NULL AND t.value <> COALESCE(s.title, '')),
                 ''
             )
         FROM series s
         WHERE s.state = 'active' OR s.state IS NULL"
            .to_string(),
    ))
    .await
    .context("populating series_title_fts from active rows")?;

    // Drop any reference to the connection so the file is closed before
    // OfflineStore opens it RO.
    drop(db);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture_store() -> (TempDir, crate::offline::OfflineStore) {
        let dir = TempDir::new().unwrap();
        crate::fixtures::write_dump_fixture(dir.path())
            .await
            .unwrap();
        let store = crate::offline::OfflineStore::open_ro(crate::offline::dump_path(dir.path()))
            .await
            .unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn side_tables_hold_genres_and_non_spoiler_tags() {
        let (_dir, store) = fixture_store().await;
        let terms = store.terms_for(&[112092]).await.unwrap();
        let frieren: Vec<&str> = terms[&112092].iter().map(|t| t.term.as_str()).collect();
        assert!(frieren.contains(&"Fantasy"), "{frieren:?}");
        assert!(frieren.contains(&"Elves"), "{frieren:?}");
        assert!(
            !frieren.contains(&"Hero Dies"),
            "spoiler tags stay out: {frieren:?}"
        );
    }

    #[tokio::test]
    async fn side_tables_count_series_per_term() {
        let (_dir, store) = fixture_store().await;
        // Frieren and its sequel both carry Elves.
        assert_eq!(store.term_series_count("elves").await.unwrap(), Some(2));
        assert_eq!(store.term_series_count("No Such Term").await.unwrap(), None);
        assert!(store.active_series_count().await.unwrap() >= 7);
    }

    #[tokio::test]
    async fn side_tables_discover_ranks_by_weighted_terms_and_filters() {
        let (_dir, store) = fixture_store().await;
        let weights = vec![("fantasy".to_owned(), 1.0), ("Elves".to_owned(), 0.5)];
        let query = crate::offline::DiscoverQuery {
            weights: &weights,
            kinds: &["manga"],
            allow_explicit: false,
            min_rating: 60.0,
            limit: 10,
        };
        let found = store.discover(&query).await.unwrap();
        let ids: Vec<i64> = found.iter().map(|m| m.mangabaka_id).collect();
        // Frieren (82) and its sequel (80) carry both terms; Explicit Pick is
        // Fantasy but pornographic.
        assert_eq!(&ids[..2], &[112092, 112093], "{ids:?}");
        assert!(!ids.contains(&77701), "{ids:?}");
        assert_eq!(
            found[0].tags,
            vec!["Elves".to_string()],
            "spoiler-free tags"
        );

        let explicit = store
            .discover(&crate::offline::DiscoverQuery {
                allow_explicit: true,
                ..query.clone()
            })
            .await
            .unwrap();
        assert!(explicit.iter().any(|m| m.mangabaka_id == 77701));
    }

    #[tokio::test]
    async fn side_tables_link_franchise_entries_both_ways() {
        let (_dir, store) = fixture_store().await;
        let links = store.relations_for(112092).await.unwrap();
        assert!(links.contains(&("sequel".to_owned(), 112093)), "{links:?}");
        let back = store.relations_for(112093).await.unwrap();
        assert!(back.contains(&("prequel".to_owned(), 112092)), "{back:?}");
    }
    use sea_orm::{ConnectionTrait, Database, Statement};
    use tempfile::TempDir;

    async fn make_dump(dir: &TempDir) -> std::path::PathBuf {
        let path = dir.path().join("series.sqlite");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let db = Database::connect(&url).await.unwrap();
        let backend = db.get_database_backend();
        db.execute_raw(Statement::from_string(
            backend,
            "CREATE TABLE series (
                id INTEGER PRIMARY KEY,
                title TEXT,
                native_title TEXT,
                romanized_title TEXT,
                titles TEXT,
                state TEXT,
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
        db.execute_raw(Statement::from_string(
            backend,
            "INSERT INTO series (id, title, native_title, titles, state, source_anilist_id, source_manga_updates_id)
             VALUES (1, 'Berserk', 'ベルセルク', '[{\"title\":\"Berserk: Black Swordsman\"}]', 'active', 33, 'berserk-1')"
                .to_string(),
        ))
        .await
        .unwrap();
        db.execute_raw(Statement::from_string(
            backend,
            "INSERT INTO series (id, title, state) VALUES (2, 'Merged Row', 'merged')".to_string(),
        ))
        .await
        .unwrap();
        drop(db);
        path
    }

    #[tokio::test]
    async fn prepare_builds_fts_and_populates_active_rows() {
        let dir = TempDir::new().unwrap();
        let path = make_dump(&dir).await;
        prepare(&path).await.unwrap();

        // Re-open RO and check FTS works.
        let db = Database::connect(format!("sqlite://{}?mode=ro", path.display()))
            .await
            .unwrap();
        let backend = db.get_database_backend();

        // FTS5 finds the canonical title.
        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                backend,
                "SELECT rowid FROM series_title_fts WHERE series_title_fts MATCH ?1",
                ["\"Berserk\"".into()],
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "FTS should match canonical title");

        // FTS5 finds an alternate title from the titles JSON array.
        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                backend,
                "SELECT rowid FROM series_title_fts WHERE series_title_fts MATCH ?1",
                ["\"Swordsman\"".into()],
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "FTS should match alternate from titles JSON");

        // Merged row excluded.
        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                backend,
                "SELECT rowid FROM series_title_fts WHERE series_title_fts MATCH ?1",
                ["\"Merged\"".into()],
            ))
            .await
            .unwrap();
        assert!(rows.is_empty(), "merged rows must not appear in FTS");
    }

    #[tokio::test]
    async fn prepare_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = make_dump(&dir).await;
        prepare(&path).await.unwrap();
        prepare(&path).await.unwrap(); // second call must be a no-op
    }
}
