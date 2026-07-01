use anyhow::Context;
use migration::{Migrator, MigratorTrait};
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use shisho_config::AppConfig;
use shisho_crypto::Dek;

/// Open the database described by `cfg.database.url`.
///
/// When `dek` is `Some`, the database is opened encrypted (SQLCipher): the data
/// key is applied as the page key before any other statement, and a non-empty
/// `cipher_version` is asserted so a non-encrypted build can never silently open
/// what should be ciphertext. `None` opens a plaintext database (the behaviour
/// when encryption is not configured). Resolve the key with
/// [`crate::keys::bootstrap_dek`] and thread the result through here.
///
/// Encryption is SQLite-only (SQLCipher); passing a `dek` with a non-sqlite URL
/// is rejected.
pub async fn connect(cfg: &AppConfig, dek: Option<&Dek>) -> anyhow::Result<DatabaseConnection> {
    // SQLite's mode=rwc creates a missing database file but not a missing
    // parent directory; pre-create it so a fresh checkout can `make run`
    // without a manual mkdir (the Docker images create /app/data themselves).
    if let Some(dir) = sqlite_parent_dir(&cfg.database.url) {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating database directory {}", dir.display()))?;
    }

    let is_sqlite = cfg.database.url.starts_with("sqlite");

    let db = match dek {
        Some(dek) => {
            anyhow::ensure!(
                is_sqlite,
                "at-rest encryption is only supported for sqlite (SQLCipher) databases, \
                 but database.url is {}",
                cfg.database.url
            );
            open_keyed(&cfg.database.url, dek).await?
        }
        None => {
            let mut opts = ConnectOptions::new(&cfg.database.url);
            opts.max_connections(cfg.database.max_connections)
                .sqlx_logging(false);
            Database::connect(opts)
                .await
                .with_context(|| format!("connecting to database {}", cfg.database.url))?
        }
    };

    // SQLite: WAL is a persistent file property (set once), foreign-key
    // enforcement is per-connection. For a single-binary service the pool is
    // small enough that running these on connect is sufficient; revisit with a
    // per-connection hook if you scale the pool.
    if is_sqlite {
        use sea_orm::{ConnectionTrait, Statement};
        let backend = db.get_database_backend();
        for pragma in ["PRAGMA journal_mode=WAL;", "PRAGMA foreign_keys=ON;"] {
            db.execute(Statement::from_string(backend, pragma)).await?;
        }

        // A keyed open must land on a real SQLCipher build with the key applied;
        // otherwise we would be writing plaintext where ciphertext is expected.
        if dek.is_some() {
            let cipher_version: String = db
                .query_one(Statement::from_string(backend, "PRAGMA cipher_version;"))
                .await?
                .context("PRAGMA cipher_version returned no row (SQLCipher not linked?)")?
                .try_get_by_index(0)?;
            anyhow::ensure!(
                !cipher_version.is_empty(),
                "expected an encrypted database but PRAGMA cipher_version is empty"
            );
        }
    }

    Ok(db)
}

/// Open an encrypted database: build a `sqlx` pool whose connect options carry
/// the SQLCipher `key` pragma, then wrap it as a sea-orm connection.
///
/// The key pragma must precede sqlx's default pragmas, because on an encrypted
/// file any statement issued before `PRAGMA key` fails; sqlx applies the `key`
/// pragma first, which sea-orm's URL-based `connect` cannot express. The raw-key
/// form is a quoted blob literal (`PRAGMA key = "x'<hex>'"`), and sqlx
/// interpolates the pragma value verbatim, so the quotes are part of the value.
async fn open_keyed(url: &str, dek: &Dek) -> anyhow::Result<DatabaseConnection> {
    use std::str::FromStr;

    use sea_orm::SqlxSqliteConnector;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    let hex = dek.to_sqlcipher_hex();
    let opts = SqliteConnectOptions::from_str(url)
        .with_context(|| format!("parsing sqlite url {url}"))?
        .pragma("key", format!("\"x'{}'\"", &*hex));
    // One connection: per-connection pragmas (foreign_keys, key) then hold for
    // every query, and a single writer is all SQLite supports anyway.
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("opening encrypted database {url}"))?;
    Ok(SqlxSqliteConnector::from_sqlx_sqlite_pool(pool))
}

pub async fn run_migrations(db: &DatabaseConnection) -> anyhow::Result<()> {
    Migrator::up(db, None).await?;
    Ok(())
}

/// Migrated in-memory SQLite for tests. The pool is pinned to one connection so
/// every query lands on the same in-memory database.
pub async fn connect_in_memory() -> DatabaseConnection {
    let mut opts = ConnectOptions::new("sqlite::memory:");
    opts.max_connections(1).sqlx_logging(false);
    let db = Database::connect(opts).await.expect("in-memory sqlite");
    Migrator::up(&db, None)
        .await
        .expect("migrating in-memory sqlite");
    db
}

/// The on-disk path of the SQLite database file from a `sqlite://` URL, with any
/// query string stripped. `None` for `:memory:` and non-sqlite backends.
pub fn sqlite_file_path(url: &str) -> Option<std::path::PathBuf> {
    let rest = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))?;
    if rest.is_empty() || rest.starts_with(":memory:") {
        return None;
    }
    let path = rest.split('?').next()?;
    if path.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(path))
}

fn sqlite_parent_dir(url: &str) -> Option<std::path::PathBuf> {
    let path = sqlite_file_path(url)?;
    let parent = path.parent()?;
    if parent.as_os_str().is_empty() {
        return None;
    }
    Some(parent.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn sqlite_file_path_extracts_path_without_query() {
        assert_eq!(
            sqlite_file_path("sqlite://data/app.db?mode=rwc"),
            Some(PathBuf::from("data/app.db"))
        );
        assert_eq!(
            sqlite_file_path("sqlite://app.db"),
            Some(PathBuf::from("app.db"))
        );
    }

    #[test]
    fn sqlite_file_path_ignores_memory_and_other_backends() {
        assert_eq!(sqlite_file_path("sqlite::memory:"), None);
        assert_eq!(sqlite_file_path("postgres://localhost/app"), None);
    }

    #[test]
    fn sqlite_parent_dir_extracts_relative_dir() {
        assert_eq!(
            sqlite_parent_dir("sqlite://data/app.db?mode=rwc"),
            Some(PathBuf::from("data"))
        );
    }

    #[test]
    fn sqlite_parent_dir_ignores_memory_and_other_backends() {
        assert_eq!(sqlite_parent_dir("sqlite::memory:"), None);
        assert_eq!(sqlite_parent_dir("postgres://localhost/app"), None);
    }

    #[test]
    fn sqlite_parent_dir_ignores_bare_filename() {
        assert_eq!(sqlite_parent_dir("sqlite://app.db"), None);
    }

    fn cfg_with_url(url: &str) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.database.url = url.to_string();
        cfg
    }

    /// De-risking spike, end to end: the bundled SQLite is SQLCipher, a keyed
    /// connection writes ciphertext to disk, and the wrong key cannot read it.
    #[tokio::test]
    async fn sqlcipher_keyed_roundtrip_and_wrong_key_fails() {
        use sea_orm::{ConnectionTrait, Statement};

        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("enc.db");
        let url = format!("sqlite://{}?mode=rwc", db_path.display());
        let dek = shisho_crypto::generate_dek();

        {
            let db = connect(&cfg_with_url(&url), Some(&dek)).await.unwrap();
            let backend = db.get_database_backend();
            db.execute(Statement::from_string(backend, "CREATE TABLE t(x TEXT);"))
                .await
                .unwrap();
            db.execute(Statement::from_string(
                backend,
                "INSERT INTO t(x) VALUES('secret');",
            ))
            .await
            .unwrap();
        }

        // On-disk header must be ciphertext, not a readable SQLite header.
        let header = std::fs::read(&db_path).unwrap();
        assert_ne!(
            &header[..16],
            b"SQLite format 3\0",
            "on-disk header is plaintext -> database is not encrypted"
        );

        // Right key reads it back.
        {
            let db = connect(&cfg_with_url(&url), Some(&dek)).await.unwrap();
            let backend = db.get_database_backend();
            let x: String = db
                .query_one(Statement::from_string(backend, "SELECT x FROM t;"))
                .await
                .unwrap()
                .unwrap()
                .try_get_by_index(0)
                .unwrap();
            assert_eq!(x, "secret");
        }

        // Wrong key cannot decrypt.
        let wrong = shisho_crypto::generate_dek();
        let res = connect(&cfg_with_url(&url), Some(&wrong)).await;
        assert!(
            res.is_err(),
            "wrong key must not open the encrypted database"
        );
    }

    #[tokio::test]
    async fn plaintext_connect_and_migrate_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            tmp.path().join("plain.db").display()
        );
        let db = connect(&cfg_with_url(&url), None).await.unwrap();
        run_migrations(&db).await.unwrap();
    }

    #[tokio::test]
    async fn encryption_rejected_for_non_sqlite_url() {
        let dek = shisho_crypto::generate_dek();
        let res = connect(&cfg_with_url("postgres://localhost/app"), Some(&dek)).await;
        assert!(res.is_err(), "encryption must be rejected for non-sqlite");
    }
}
