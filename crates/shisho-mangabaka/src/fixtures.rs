//! Test-support dump builder (behind the `test-fixtures` feature; never
//! part of a production build). Writes a dump-shaped `series.sqlite` with
//! three rows that mirror the recorded Codex/Tsundoku fixtures under
//! `tests/fixtures/`, then runs the real [`crate::offline::setup::prepare`]
//! so FTS and the source-column indexes behave exactly like a promoted
//! dump:
//!
//! - `1677` Chainsaw Man (anilist 105778, mal 116778, mangaupdates ylx5wzn)
//! - `112092` Sousou no Frieren (mal 126287 — the Codex `lib-a` series)
//! - `424242` Obscure Title (anilist 99999 — the Codex `lib-b` series
//!   Tsundoku has never heard of)
//! - `333` Verdicted By MangaBaka Id (the Codex `lib-f` series, which has
//!   no summary — exercises description backfill from the dump)

use std::path::Path;

use anyhow::Context;
use sea_orm::{ConnectionTrait, Database, Statement};

use crate::offline::{self, setup};

/// Write the fixture dump to `<dump_dir>/series.sqlite` (the path a
/// provider over `dump_dir` opens) and prepare it (indexes + FTS).
pub async fn write_dump_fixture(dump_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dump_dir)
        .with_context(|| format!("creating {}", dump_dir.display()))?;
    let path = offline::dump_path(dump_dir);
    let db = Database::connect(format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .with_context(|| format!("creating fixture dump {}", path.display()))?;
    let backend = db.get_database_backend();

    let statements = [
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
            source_anime_news_network_id INTEGER
        )",
        "INSERT INTO series (
            id, title, native_title, titles, type, status, year, final_volume,
            total_chapters, rating, description, state, genres, tags,
            source_anilist_id, source_my_anime_list_id, source_manga_updates_id
        ) VALUES (
            1677, 'Chainsaw Man', 'チェンソーマン', '[{\"title\":\"CSM\"}]',
            'manga', 'releasing', 2018, '24', '232', 85.0,
            'Denji has a devil dog.', 'active',
            '[\"Action\", \"Horror\"]', '[\"Devils\"]',
            105778, 116778, 'ylx5wzn'
        )",
        "INSERT INTO series (
            id, title, type, status, year, rating, description, state,
            genres, tags, source_my_anime_list_id
        ) VALUES (
            112092, 'Sousou no Frieren', 'manga', 'completed', 2020, 82.0,
            'Dump description: the elf mage after the hero''s journey.',
            'active', '[\"Fantasy\", \"Drama\"]', '[]', 126287
        )",
        "INSERT INTO series (
            id, title, type, status, year, rating, description, state,
            genres, tags, source_anilist_id
        ) VALUES (
            424242, 'Obscure Title', 'manga', 'releasing', 2021, 76.0,
            'Dump description for Obscure Title.', 'active',
            '[\"Mystery\", \"Thriller\"]', '[]', 99999
        )",
        "INSERT INTO series (
            id, title, type, status, year, rating, description, state, genres, tags
        ) VALUES (
            333, 'Verdicted By MangaBaka Id', 'manga', 'releasing', 2019, 70.0,
            'Dump description for the 333 row.', 'active', '[\"Action\"]', '[]'
        )",
    ];
    for stmt in statements {
        db.execute_raw(Statement::from_string(backend, stmt.to_string()))
            .await
            .context("seeding fixture dump")?;
    }
    drop(db);

    setup::prepare(&path)
        .await
        .context("preparing fixture dump")
}
