//! Startup key bootstrap: turn the operator's KEK configuration into the data
//! key (DEK) that opens the encrypted database.
//!
//! Called once at startup, before the database is opened. The flow:
//!
//! - encryption not configured -> `None` (database opens as plaintext, the
//!   pre-encryption behaviour);
//! - configured + keyfile present -> unwrap the DEK with the configured KEK;
//! - configured + no keyfile + no database -> first init: generate a DEK, wrap
//!   it under the KEK, and write the keyfile;
//! - configured + no keyfile + a database already exists -> hard error (the data
//!   would be encrypted but the wrapped key is gone);
//! - not configured + a keyfile exists -> hard error (the data is encrypted but
//!   no key is configured to open it).
//!
//! KEK rotation (a keyfile wrapped by an older KEK version) is handled by
//! [`rotate_kek`]; a version mismatch at startup is a clear error pointing there.
//!
//! The keyfile lives beside the database file as `shisho.keys` (e.g.
//! `data/shisho.db` -> `data/shisho.keys`). Encryption is SQLite-only.

use std::path::PathBuf;

use anyhow::Context;
use shisho_config::AppConfig;
use shisho_crypto::{Dek, Keyfile};

use crate::conn::sqlite_file_path;

/// Location of the wrapped-DEK keyfile, beside the SQLite database file. `None`
/// when the database URL is not a sqlite file path (`:memory:` / non-sqlite).
pub fn keyfile_path(cfg: &AppConfig) -> Option<PathBuf> {
    Some(sqlite_file_path(&cfg.database.url)?.with_extension("keys"))
}

/// Resolve the data-encryption key for this instance, or `None` when encryption
/// is not configured. Creates the keyfile on first init. See the module docs for
/// the full decision table.
pub fn bootstrap_dek(cfg: &AppConfig) -> anyhow::Result<Option<Dek>> {
    let Some(keyfile) = keyfile_path(cfg) else {
        anyhow::ensure!(
            !cfg.encryption.is_enabled(),
            "at-rest encryption requires a sqlite database file URL, but database.url is {}",
            cfg.database.url
        );
        return Ok(None);
    };

    if !cfg.encryption.is_enabled() {
        anyhow::ensure!(
            !keyfile.exists(),
            "an encryption keyfile exists at {} but no encryption key is configured; \
             restore `encryption.key_v{}` (and `key_version`) or the data cannot be read",
            keyfile.display(),
            cfg.encryption.key_version
        );
        return Ok(None);
    }

    let kek = cfg.encryption.resolve_kek()?;

    if keyfile.exists() {
        let kf = Keyfile::load(&keyfile)
            .with_context(|| format!("reading keyfile {}", keyfile.display()))?;
        anyhow::ensure!(
            kf.kek_version == cfg.encryption.key_version,
            "keyfile was wrapped with KEK v{} but encryption.key_version = {}; \
             run `shisho encryption rotate-kek` to re-wrap it",
            kf.kek_version,
            cfg.encryption.key_version
        );
        let dek = shisho_crypto::unwrap_dek(&kek, &kf.wrapped_bytes()?)
            .context("unwrapping the data key with the configured KEK (wrong key?)")?;
        return Ok(Some(dek));
    }

    // No keyfile. If a database is already present it must have been encrypted
    // under a now-missing keyfile; refuse rather than fail cryptically later.
    if let Some(db_path) = sqlite_file_path(&cfg.database.url) {
        anyhow::ensure!(
            !db_path.exists(),
            "database {} exists but the keyfile {} is missing; \
             the wrapped data key is required to open it",
            db_path.display(),
            keyfile.display()
        );
    }

    // First init: mint a DEK, wrap it under the current KEK, persist the keyfile.
    let dek = shisho_crypto::generate_dek();
    let wrapped = shisho_crypto::wrap_dek(&kek, &dek);
    Keyfile::new(&wrapped, cfg.encryption.key_version)
        .store(&keyfile)
        .with_context(|| format!("writing keyfile {}", keyfile.display()))?;
    tracing::info!(path = %keyfile.display(), "initialized encryption keyfile");
    Ok(Some(dek))
}

/// Re-wrap the data key under the current KEK (`encryption.key_version`),
/// unwrapping it with the KEK version that wrapped the existing keyfile. The
/// database is never touched: only the small keyfile is rewritten, so this is
/// O(1) regardless of data size. Both the old and new KEK versions must be
/// present in config during the rotation.
pub fn rotate_kek(cfg: &AppConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        cfg.encryption.is_enabled(),
        "encryption is not configured; nothing to rotate"
    );
    let keyfile =
        keyfile_path(cfg).context("at-rest encryption requires a sqlite database file URL")?;
    let kf = Keyfile::load(&keyfile)
        .with_context(|| format!("reading keyfile {}", keyfile.display()))?;

    let old_kek = cfg
        .encryption
        .resolve_kek_version(kf.kek_version)
        .with_context(|| {
            format!(
                "resolving the old KEK v{} that wrapped the keyfile \
                 (keep it in config until rotation completes)",
                kf.kek_version
            )
        })?;
    let new_kek = cfg.encryption.resolve_kek()?;

    let dek = shisho_crypto::unwrap_dek(&old_kek, &kf.wrapped_bytes()?)
        .context("unwrapping the data key with the old KEK")?;
    let wrapped = shisho_crypto::wrap_dek(&new_kek, &dek);
    Keyfile::new(&wrapped, cfg.encryption.key_version).store(&keyfile)?;
    tracing::info!(
        from = kf.kek_version,
        to = cfg.encryption.key_version,
        "re-wrapped the data key under the new KEK"
    );
    Ok(())
}

/// A read-only snapshot of the encryption state for operator visibility. It
/// never includes key material.
#[derive(Debug, Clone)]
pub struct KeyStatus {
    /// Whether at-rest encryption is configured.
    pub enabled: bool,
    /// The KEK version that wrapped the current keyfile, if one exists.
    pub kek_version: Option<u32>,
    /// When the keyfile was last written (first init or KEK rotation), RFC 3339.
    pub last_rotated_at: Option<String>,
}

/// Report the encryption status from config plus the keyfile metadata. Reads no
/// key material and never fails: a missing/unreadable keyfile reports `None`.
pub fn status(cfg: &AppConfig) -> KeyStatus {
    let (kek_version, last_rotated_at) =
        match keyfile_path(cfg).and_then(|p| Keyfile::load(&p).ok()) {
            Some(kf) => (Some(kf.kek_version), Some(kf.created_at)),
            None => (None, None),
        };
    KeyStatus {
        enabled: cfg.encryption.is_enabled(),
        kek_version,
        last_rotated_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use std::path::Path;

    fn cfg_with_encryption(data_dir: &Path, key_version: u32, keys: &[(&str, &str)]) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.database.url = format!("sqlite://{}?mode=rwc", data_dir.join("shisho.db").display());
        cfg.encryption.key_version = key_version;
        cfg.encryption.keys = keys
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        cfg
    }

    fn b64_key(byte: u8) -> String {
        base64::engine::general_purpose::STANDARD.encode([byte; 32])
    }

    #[test]
    fn disabled_encryption_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = AppConfig::default();
        cfg.database.url = format!(
            "sqlite://{}?mode=rwc",
            tmp.path().join("shisho.db").display()
        );
        assert!(bootstrap_dek(&cfg).unwrap().is_none());
    }

    #[test]
    fn first_init_creates_keyfile_and_returns_dek() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_encryption(tmp.path(), 1, &[("key_v1", &b64_key(0x11))]);

        let dek = bootstrap_dek(&cfg).unwrap().expect("dek");
        assert!(keyfile_path(&cfg).unwrap().exists(), "keyfile written");

        // A second bootstrap loads the same DEK from the keyfile.
        let again = bootstrap_dek(&cfg).unwrap().expect("dek");
        assert_eq!(
            dek.as_bytes(),
            again.as_bytes(),
            "stable DEK across restarts"
        );
    }

    #[test]
    fn wrong_kek_fails_to_unwrap() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_encryption(tmp.path(), 1, &[("key_v1", &b64_key(0x11))]);
        bootstrap_dek(&cfg).unwrap().expect("init");

        // Same version, different key bytes -> unwrap fails.
        let cfg2 = cfg_with_encryption(tmp.path(), 1, &[("key_v1", &b64_key(0x22))]);
        let err = bootstrap_dek(&cfg2).unwrap_err().to_string();
        assert!(err.contains("unwrapping"), "unexpected error: {err}");
    }

    #[test]
    fn missing_keyfile_with_existing_db_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        // Simulate an existing database with no keyfile.
        std::fs::write(tmp.path().join("shisho.db"), b"SQLite format 3\0").unwrap();
        let cfg = cfg_with_encryption(tmp.path(), 1, &[("key_v1", &b64_key(0x11))]);

        let err = bootstrap_dek(&cfg).unwrap_err().to_string();
        assert!(err.contains("keyfile"), "unexpected error: {err}");
    }

    #[test]
    fn keyfile_present_but_encryption_disabled_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let enabled = cfg_with_encryption(tmp.path(), 1, &[("key_v1", &b64_key(0x11))]);
        bootstrap_dek(&enabled).unwrap();

        let mut disabled = AppConfig::default();
        disabled.database.url = enabled.database.url.clone();
        let err = bootstrap_dek(&disabled).unwrap_err().to_string();
        assert!(
            err.contains("no encryption key is configured"),
            "got: {err}"
        );
    }

    #[test]
    fn kek_version_mismatch_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_encryption(tmp.path(), 1, &[("key_v1", &b64_key(0x11))]);
        bootstrap_dek(&cfg).unwrap();

        // Operator bumped key_version to 2 without rotating the keyfile.
        let bumped = cfg_with_encryption(
            tmp.path(),
            2,
            &[("key_v1", &b64_key(0x11)), ("key_v2", &b64_key(0x22))],
        );
        let err = bootstrap_dek(&bumped).unwrap_err().to_string();
        assert!(err.contains("rotate-kek"), "unexpected error: {err}");
    }

    #[test]
    fn rotate_kek_rewraps_without_changing_the_dek() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_encryption(tmp.path(), 1, &[("key_v1", &b64_key(0x11))]);
        let dek_before = bootstrap_dek(&cfg).unwrap().unwrap();

        let rotated = cfg_with_encryption(
            tmp.path(),
            2,
            &[("key_v1", &b64_key(0x11)), ("key_v2", &b64_key(0x22))],
        );
        rotate_kek(&rotated).unwrap();

        let st = status(&rotated);
        assert_eq!(st.kek_version, Some(2));
        let dek_after = bootstrap_dek(&rotated).unwrap().unwrap();
        assert_eq!(
            dek_before.as_bytes(),
            dek_after.as_bytes(),
            "the data key is preserved across KEK rotation"
        );

        // The old key_version alone now mismatches the rotated keyfile.
        let only_v1 = cfg_with_encryption(tmp.path(), 1, &[("key_v1", &b64_key(0x11))]);
        assert!(bootstrap_dek(&only_v1).is_err());
    }

    #[test]
    fn rotate_kek_requires_the_old_key_in_config() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_encryption(tmp.path(), 1, &[("key_v1", &b64_key(0x11))]);
        bootstrap_dek(&cfg).unwrap();

        // New key present, the v1 that wrapped the keyfile is gone -> error.
        let bad = cfg_with_encryption(tmp.path(), 2, &[("key_v2", &b64_key(0x22))]);
        let err = format!("{:#}", rotate_kek(&bad).unwrap_err());
        assert!(err.contains("key_v1"), "unexpected error: {err}");
    }

    #[test]
    fn status_reflects_configuration_and_keyfile() {
        let tmp = tempfile::tempdir().unwrap();

        let mut disabled = AppConfig::default();
        disabled.database.url = format!(
            "sqlite://{}?mode=rwc",
            tmp.path().join("shisho.db").display()
        );
        let st = status(&disabled);
        assert!(!st.enabled);
        assert_eq!(st.kek_version, None);

        let cfg = cfg_with_encryption(tmp.path(), 1, &[("key_v1", &b64_key(0x11))]);
        bootstrap_dek(&cfg).unwrap();
        let st = status(&cfg);
        assert!(st.enabled);
        assert_eq!(st.kek_version, Some(1));
        assert!(st.last_rotated_at.is_some());
    }
}
