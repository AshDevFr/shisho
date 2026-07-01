//! The on-disk keyfile holding the wrapped data key.
//!
//! Lives at `shisho.keys`, beside the database (the data volume is already
//! mounted and writable). It is not a secret on its own: it stores the DEK
//! *wrapped* by the KEK, so it is useless without the operator's KEK. The KEK
//! must live off the data disk (env/secret).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};

use crate::Result;

/// The wrapped data key plus the metadata needed to unwrap and rotate it.
///
/// JSON shape (snake_case, this is an on-disk artifact, not an API DTO):
/// ```json
/// { "dek_wrapped": "<base64>", "kek_version": 1, "created_at": "<rfc3339>" }
/// ```
/// `dek_wrapped` is the full sealed blob (`nonce || ciphertext || tag`) from
/// `wrap_dek`, so the nonce travels inside it; there is no separate nonce field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keyfile {
    /// Base64 of the DEK sealed under the KEK (`nonce || ciphertext || tag`).
    pub dek_wrapped: String,
    /// Which KEK version wrapped the DEK; matched against `encryption.key_version`.
    pub kek_version: u32,
    /// RFC 3339 timestamp recording when this keyfile was written.
    pub created_at: String,
}

impl Keyfile {
    /// Build a keyfile from freshly wrapped DEK bytes, stamping `created_at` now.
    pub fn new(wrapped: &[u8], kek_version: u32) -> Self {
        Self {
            dek_wrapped: B64.encode(wrapped),
            kek_version,
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Decode the wrapped DEK back to bytes for `unwrap_dek`.
    pub fn wrapped_bytes(&self) -> Result<Vec<u8>> {
        Ok(B64.decode(self.dek_wrapped.trim())?)
    }

    /// Read and parse a keyfile from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Write the keyfile atomically (temp sibling + rename) with `0600`
    /// permissions on Unix, so a crash never leaves a half-written keyfile and
    /// the wrapped key is not world-readable.
    pub fn store(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self)?;

        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("shisho.keys");
        let tmp = path.with_file_name(format!(".{file_name}.tmp"));

        {
            let mut f = open_private(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        // Rename is atomic on the same filesystem and carries the temp file's
        // restricted permissions to the destination.
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Open `path` write-only, creating/truncating it, with owner-only `0600`
/// permissions on Unix. On other platforms the default permissions apply.
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shisho.keys");

        let kf = Keyfile::new(b"\x00\x01\x02wrapped-bytes", 2);
        kf.store(&path).unwrap();

        let loaded = Keyfile::load(&path).unwrap();
        assert_eq!(loaded.kek_version, 2);
        assert_eq!(
            loaded.wrapped_bytes().unwrap(),
            b"\x00\x01\x02wrapped-bytes"
        );
        assert_eq!(loaded.created_at, kf.created_at);
    }

    #[test]
    fn store_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shisho.keys");
        Keyfile::new(b"x", 1).store(&path).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["shisho.keys".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn store_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shisho.keys");
        Keyfile::new(b"x", 1).store(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "keyfile must be 0600");
    }

    #[test]
    fn store_overwrites_an_existing_keyfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shisho.keys");
        Keyfile::new(b"old", 1).store(&path).unwrap();
        Keyfile::new(b"new", 2).store(&path).unwrap();

        let loaded = Keyfile::load(&path).unwrap();
        assert_eq!(loaded.kek_version, 2);
        assert_eq!(loaded.wrapped_bytes().unwrap(), b"new");
    }
}
