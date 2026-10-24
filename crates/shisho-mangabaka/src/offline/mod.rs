//! MangaBaka offline-dump subsystem.
//!
//! Pipeline:
//!
//! 1. [`fetch::download`] streams `series.sqlite.tar.gz` from MangaBaka,
//!    cross-checking the `.sha1` sidecar best-effort (see below).
//! 2. [`extract::extract_dump`] untars the archive into a temp `series.sqlite`.
//! 3. [`validate::validate_dump`] confirms the extract is a well-formed
//!    SQLite carrying a plausible `series` row count — the integrity guard,
//!    since upstream's sidecar can drift out of sync with the served tarball.
//! 4. [`setup::prepare`] adds source-id indexes + an FTS5 virtual table on
//!    the extracted file (a one-time cost per refresh, ~minutes on 585k rows).
//! 5. [`store::OfflineStore::open_ro`] opens the prepared file as a
//!    read-only sea-orm connection for the provider to query.
//!
//! The atomic-promote discipline is the load-bearing part: the temp copy is
//! validated and prepared fully before the rename; the live file is never
//! touched in place, so a failed refresh leaves the previous dump serving.

pub mod extract;
pub mod fetch;
pub mod setup;
pub mod store;
pub mod validate;

pub use store::{DiscoverQuery, OfflineStore, SeriesTerm};

use std::path::{Path, PathBuf};

use anyhow::Context;

/// Canonical filename for the extracted dump under `<storage dir>/mangabaka/`.
pub const DUMP_FILENAME: &str = "series.sqlite";

/// Subdirectory used for in-progress downloads and extractions. Kept inside
/// the dump dir so the temp lives on the same filesystem as the target
/// (cheap atomic rename).
pub const TMP_SUBDIR: &str = ".tmp";

/// Path the live OfflineStore opens from.
pub fn dump_path(dump_dir: impl AsRef<Path>) -> PathBuf {
    dump_dir.as_ref().join(DUMP_FILENAME)
}

/// Path used for in-progress work. Cleared on success.
pub fn tmp_dir(dump_dir: impl AsRef<Path>) -> PathBuf {
    dump_dir.as_ref().join(TMP_SUBDIR)
}

pub struct RefreshArtifacts {
    pub bytes_downloaded: u64,
    /// SHA-1 of the downloaded tarball (doubles as a dump version).
    pub sha1: String,
}

/// Run the pipeline end to end: download, extract, validate, prepare, and
/// atomically promote into `dump_path(dump_dir)`. The caller is responsible
/// for closing any open [`OfflineStore`] on the destination first —
/// extraction writes to a temp path, but the final rename will fail on
/// Windows (and confuse macOS) if the destination is open.
pub async fn refresh(
    http: &reqwest::Client,
    dump_url: &str,
    dump_dir: &Path,
) -> anyhow::Result<RefreshArtifacts> {
    tokio::fs::create_dir_all(dump_dir)
        .await
        .with_context(|| format!("creating {}", dump_dir.display()))?;
    // Wipe leftovers from a refresh killed mid-run (process restart,
    // crash): a stale `series.sqlite-journal` in here would make SQLite
    // "recover" — corrupt — the next freshly extracted file. Refreshes
    // are serialized by the provider, so a wholesale sweep is safe.
    let tmp = tmp_dir(dump_dir);
    if tmp.exists() {
        tokio::fs::remove_dir_all(&tmp)
            .await
            .with_context(|| format!("sweeping stale {}", tmp.display()))?;
    }
    tokio::fs::create_dir_all(&tmp)
        .await
        .with_context(|| format!("creating {}", tmp.display()))?;
    let archive_path = tmp.join("series.sqlite.tar.gz");
    let extracted_path = tmp.join(DUMP_FILENAME);
    let dest_path = dump_path(dump_dir);

    // Download. The `.sha1` sidecar is cross-checked best-effort only: in
    // practice upstream republishes the tarball without updating the
    // sidecar (or vice versa), so a mismatch is logged but does NOT abort —
    // it can't be distinguished from upstream's own drift. Integrity is
    // enforced instead by `validate::validate_dump` on the extracted file.
    tracing::info!(%dump_url, "downloading MangaBaka dump");
    let download = fetch::download(http, dump_url, &archive_path).await?;
    let sidecar_url = fetch::sha1_sidecar_url(dump_url);
    match fetch::fetch_expected_sha1(http, &sidecar_url).await {
        Ok(expected) if expected != download.sha1_hex => {
            tracing::warn!(
                expected = %expected,
                actual = %download.sha1_hex,
                %dump_url,
                "SHA-1 sidecar disagrees with downloaded tarball (upstream drift); \
                 relying on content validation instead"
            );
        }
        Ok(_) => tracing::info!("SHA-1 sidecar matched"),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), sidecar = %sidecar_url,
                "sha1 sidecar unavailable; proceeding without verification");
        }
    }

    // Extract.
    tracing::info!(archive = %archive_path.display(), "extracting dump");
    extract::extract_dump(&archive_path, &extracted_path).await?;

    // Validate the extracted DB before the multi-minute index build, so a
    // corrupt/truncated download fails fast rather than after `prepare`.
    validate::validate_dump(&extracted_path).await?;

    // Setup (indexes + FTS).
    setup::prepare(&extracted_path).await?;

    // Atomic swap into place. On most filesystems `rename` over an existing
    // file is atomic; we defensively remove first on Windows where it isn't.
    if dest_path.exists() {
        let _ = tokio::fs::remove_file(&dest_path).await;
    }
    tokio::fs::rename(&extracted_path, &dest_path)
        .await
        .with_context(|| {
            format!(
                "renaming {} -> {}",
                extracted_path.display(),
                dest_path.display()
            )
        })?;

    // Cleanup the tarball. Leave the tmp dir in place; it's tiny.
    let _ = tokio::fs::remove_file(&archive_path).await;

    Ok(RefreshArtifacts {
        bytes_downloaded: download.bytes_downloaded,
        sha1: download.sha1_hex,
    })
}
