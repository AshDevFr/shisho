//! Full refresh-pipeline tests against a locally built dump tarball served
//! by wiremock: download → extract → validate → prepare → atomic promote,
//! plus the failure path (a corrupt download leaves the previous dump
//! serving and no partial file at the live path).
//!
//! The validation floor is real (100k `series` rows), so the fixture dump
//! is padded with generated rows via a recursive CTE — fast enough to run
//! on every test pass.

use sea_orm::{ConnectionTrait, Database, Statement};
use shisho_config::MangabakaConfig;
use shisho_mangabaka::{MangabakaProvider, RefreshOutcome};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FILLER_ROWS: i64 = 100_000;

/// Build a dump-shaped SQLite at `path`: one real row (Chainsaw Man, id
/// 1677) plus enough filler to clear the validation floor.
async fn build_dump_sqlite(path: &std::path::Path) {
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
    db.execute_raw(Statement::from_string(
        backend,
        "INSERT INTO series (
            id, title, native_title, type, status, year, final_volume, total_chapters,
            rating, description, state, genres, source_anilist_id, source_manga_updates_id
        ) VALUES (
            1677, 'Chainsaw Man', 'チェンソーマン', 'manga', 'releasing', 2018, '24', '232',
            85.0, 'Denji has a devil dog.', 'active', '[\"Action\"]', 105778, 'ylx5wzn'
        )"
        .to_string(),
    ))
    .await
    .unwrap();
    db.execute_raw(Statement::from_string(
        backend,
        format!(
            "INSERT INTO series (id, title, state)
             WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < {FILLER_ROWS})
             SELECT 2000000 + x, 'filler ' || x, 'active' FROM cnt"
        ),
    ))
    .await
    .unwrap();
    drop(db);
}

/// Pack `file` into a single-entry `.tar.gz`, returning its bytes.
fn tar_gz_bytes(file: &std::path::Path) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    let gz = GzEncoder::new(Vec::new(), Compression::fast());
    let mut tar = tar::Builder::new(gz);
    let mut src = std::fs::File::open(file).unwrap();
    tar.append_file("series.sqlite", &mut src).unwrap();
    tar.into_inner().unwrap().finish().unwrap()
}

async fn dump_tarball(dir: &tempfile::TempDir) -> Vec<u8> {
    let src = dir.path().join("upstream.sqlite");
    build_dump_sqlite(&src).await;
    tar_gz_bytes(&src)
}

fn provider_cfg(server: &MockServer) -> MangabakaConfig {
    MangabakaConfig {
        api_fallback: false,
        offline_dump_url: Some(format!("{}/v1/database/series.sqlite.tar.gz", server.uri())),
        ..Default::default()
    }
}

#[tokio::test]
async fn full_refresh_downloads_promotes_and_is_idempotent() {
    let scratch = tempfile::TempDir::new().unwrap();
    let tarball = dump_tarball(&scratch).await;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/database/series.sqlite.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball))
        .mount(&server)
        .await;
    // No sidecar published: the pipeline logs and proceeds.
    Mock::given(method("GET"))
        .and(path("/v1/database/series.sqlite.tar.gz.sha1"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let dump_dir = tempfile::TempDir::new().unwrap();
    let provider =
        MangabakaProvider::new(&provider_cfg(&server), dump_dir.path().to_path_buf()).unwrap();
    assert!(!provider.offline_loaded().await, "fresh deployment");

    let outcome = provider.refresh_cache().await.expect("refresh succeeds");
    let RefreshOutcome::Refreshed(summary) = outcome else {
        panic!("expected Refreshed, got {outcome:?}");
    };
    assert_eq!(summary.series_rows, FILLER_ROWS as u64 + 1);
    assert!(summary.bytes_downloaded > 0);
    assert_eq!(summary.sha1.len(), 40);

    // The promoted dump serves lookups and ranked FTS search.
    assert!(provider.offline_loaded().await);
    let m = provider.get("1677").await.unwrap().expect("dump hit");
    assert_eq!(m.title, "Chainsaw Man");
    assert_eq!(m.origin.as_str(), "mangabaka-dump");
    let hits = provider.search("Chainsaw Man", 5).await.unwrap();
    assert!(!hits.is_empty(), "FTS search returns matches");
    assert_eq!(hits[0].mangabaka_id, 1677);
    assert!(hits[0].score.is_some(), "hits carry a Dice score");

    // Status reflects the loaded dump.
    let status = provider.status().await;
    assert!(status.dump_loaded);
    assert_eq!(status.series_rows, Some(FILLER_ROWS as u64 + 1));
    assert!(status.dump_modified_at.is_some());
    assert!(status.last_refresh.as_ref().unwrap().outcome.is_ok());

    // A second run repeats the whole pipeline cleanly (wholesale replace).
    let outcome = provider.refresh_cache().await.expect("second refresh");
    assert!(matches!(outcome, RefreshOutcome::Refreshed(_)));
    assert!(provider.get("1677").await.unwrap().is_some());
}

#[tokio::test]
async fn corrupt_download_fails_loudly_and_previous_dump_keeps_serving() {
    let scratch = tempfile::TempDir::new().unwrap();
    let tarball = dump_tarball(&scratch).await;

    // First fire serves the good tarball, every later one garbage.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/database/series.sqlite.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/database/series.sqlite.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"not a tarball".to_vec()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/database/series.sqlite.tar.gz.sha1"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let dump_dir = tempfile::TempDir::new().unwrap();
    let provider =
        MangabakaProvider::new(&provider_cfg(&server), dump_dir.path().to_path_buf()).unwrap();

    let outcome = provider.refresh_cache().await.expect("first refresh");
    assert!(matches!(outcome, RefreshOutcome::Refreshed(_)));

    // Second refresh downloads garbage: it must fail loudly...
    let err = provider.refresh_cache().await.expect_err("corrupt refresh");
    assert!(
        err.to_string().contains("offline store"),
        "failure is a store error: {err}"
    );

    // ...while the previous dump keeps serving...
    assert!(provider.offline_loaded().await, "previous dump re-opened");
    let m = provider.get("1677").await.unwrap().expect("still resolves");
    assert_eq!(m.title, "Chainsaw Man");

    // ...the failure is visible on status...
    let status = provider.status().await;
    assert!(status.last_refresh.as_ref().unwrap().outcome.is_err());

    // ...and the live path holds the previous, valid dump (no partial
    // file): a fresh provider can still open it.
    let reopened =
        MangabakaProvider::new(&provider_cfg(&server), dump_dir.path().to_path_buf()).unwrap();
    assert!(reopened.load_existing_dump().await, "live file is intact");
    assert!(reopened.offline_loaded().await);
}

/// A refresh killed mid-run (process restart under a dev watcher, a crash)
/// leaves partial work in `.tmp` — most damagingly a hot
/// `series.sqlite-journal` that SQLite would use to "recover" (i.e.
/// corrupt) the next freshly extracted file. The pipeline must start from
/// a clean slate every time.
#[tokio::test]
async fn stale_tmp_leftovers_are_swept_before_the_next_refresh() {
    let scratch = tempfile::TempDir::new().unwrap();
    let tarball = dump_tarball(&scratch).await;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/database/series.sqlite.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/database/series.sqlite.tar.gz.sha1"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    // Simulate the interrupted run's debris.
    let dump_dir = tempfile::TempDir::new().unwrap();
    let tmp = dump_dir.path().join(".tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("series.sqlite"), b"partial extract").unwrap();
    std::fs::write(tmp.join("series.sqlite-journal"), b"stale hot journal").unwrap();

    let provider =
        MangabakaProvider::new(&provider_cfg(&server), dump_dir.path().to_path_buf()).unwrap();
    let outcome = provider.refresh_cache().await.expect("refresh succeeds");
    assert!(matches!(outcome, RefreshOutcome::Refreshed(_)));
    assert!(
        !tmp.join("series.sqlite-journal").exists(),
        "stale journal swept; it must never pair with a fresh extract"
    );
    assert!(provider.get("1677").await.unwrap().is_some());
}

/// An undersized (but well-formed) dump is refused by the validation gate.
#[tokio::test]
async fn undersized_dump_is_refused_before_promotion() {
    let scratch = tempfile::TempDir::new().unwrap();
    let small = scratch.path().join("small.sqlite");
    let db = Database::connect(format!("sqlite://{}?mode=rwc", small.display()))
        .await
        .unwrap();
    db.execute_raw(Statement::from_string(
        db.get_database_backend(),
        "CREATE TABLE series (id INTEGER PRIMARY KEY, title TEXT, state TEXT)".to_string(),
    ))
    .await
    .unwrap();
    db.execute_raw(Statement::from_string(
        db.get_database_backend(),
        "INSERT INTO series (id, title, state) VALUES (1, 'Lonely Row', 'active')".to_string(),
    ))
    .await
    .unwrap();
    drop(db);
    let tarball = tar_gz_bytes(&small);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/database/series.sqlite.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/database/series.sqlite.tar.gz.sha1"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let dump_dir = tempfile::TempDir::new().unwrap();
    let provider =
        MangabakaProvider::new(&provider_cfg(&server), dump_dir.path().to_path_buf()).unwrap();

    let err = provider.refresh_cache().await.expect_err("refused");
    let chain = format!("{err:?}");
    assert!(
        chain.contains("series rows"),
        "validation gate names the row floor: {chain}"
    );
    // Nothing was promoted: no dump at the live path, API-only mode persists.
    assert!(!provider.offline_loaded().await);
    assert!(!dump_dir.path().join("series.sqlite").exists());
}
