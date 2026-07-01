//! Application-level encryption primitives for shisho.
//!
//! This crate is the envelope-encryption toolbox the rest of the system keys
//! itself with. It holds no policy and no I/O beyond the keyfile: callers pass
//! raw key bytes in and get sealed bytes out. The hierarchy it supports:
//!
//! ```text
//! KEK (operator master key, 32 bytes)  --wraps-->  DEK (random data key, 32 bytes)
//!                                                    |- SQLCipher page key (see `to_sqlcipher_hex`)
//!                                                    |- HKDF -> per-file AEAD key (`file_key`)
//! ```
//!
//! - [`seal`]/[`open`] are the AEAD primitive (AES-256-GCM, random nonce
//!   prepended): `seal` returns `nonce || ciphertext || tag`, `open` reverses it.
//! - [`wrap_dek`]/[`unwrap_dek`] are `seal`/`open` specialised to a [`Dek`].
//! - [`file_key`] derives the file-encryption key from the DEK via HKDF-SHA256.
//! - [`Keyfile`] is the on-disk `{data_dir}/shisho.keys` holding the wrapped DEK.
//!
//! Key material ([`Dek`], unwrapped plaintext, the SQLCipher hex) is zeroized on
//! drop. There are no axum/db/network dependencies here by design.

mod keyfile;

pub use keyfile::Keyfile;

use std::fmt;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hkdf::Hkdf;
use rand::Rng;
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// Length of every symmetric key (KEK, DEK, derived file key): 256 bits.
pub const KEY_LEN: usize = 32;
/// AES-GCM nonce length, prepended to every sealed blob.
const NONCE_LEN: usize = 12;
/// AES-GCM authentication tag length, appended by the AEAD.
const TAG_LEN: usize = 16;

/// HKDF `info` label for the file-encryption subkey. Versioned so a future
/// derivation change can coexist with existing files.
const FILE_KEY_INFO: &[u8] = b"shisho:files:v1";

/// Errors from sealing, opening, key wrapping, and keyfile I/O.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// AEAD verification failed: wrong key or the ciphertext was modified.
    #[error("AEAD authentication failed: wrong key or corrupted ciphertext")]
    Decrypt,
    /// A sealed blob is shorter than a nonce + tag, so it cannot be valid.
    #[error("sealed blob too short: {got} bytes, need at least {min}")]
    TooShort { got: usize, min: usize },
    /// An unwrapped data key did not decode to exactly [`KEY_LEN`] bytes.
    #[error("unwrapped data key has the wrong length: {0} bytes, expected {KEY_LEN}")]
    DekLength(usize),
    /// Reading or writing the keyfile failed.
    #[error("keyfile I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The keyfile is not valid JSON.
    #[error("keyfile is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A base64 field in the keyfile could not be decoded.
    #[error("invalid base64 in keyfile: {0}")]
    Base64(#[from] base64::DecodeError),
}

/// Result alias for crypto operations.
pub type Result<T> = std::result::Result<T, CryptoError>;

/// A 256-bit data-encryption key. The bytes are wiped from memory on drop, and
/// the `Debug` impl never prints them.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Dek([u8; KEY_LEN]);

impl Dek {
    /// Adopt 32 raw bytes as a data key (e.g. just-unwrapped material).
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// The raw key bytes, for callers that must hand them to a lower-level
    /// primitive (the SQLCipher pragma, HKDF). Prefer the typed helpers below.
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// The key as a `Zeroizing` lowercase-hex string for SQLCipher's raw-key
    /// pragma, used as `PRAGMA key = "x'<hex>'"`. The string wipes on drop.
    pub fn to_sqlcipher_hex(&self) -> Zeroizing<String> {
        use std::fmt::Write;
        let mut s = String::with_capacity(KEY_LEN * 2);
        for b in self.0 {
            // Infallible: writing to a String never errors.
            let _ = write!(s, "{b:02x}");
        }
        Zeroizing::new(s)
    }
}

impl fmt::Debug for Dek {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never leak key material into logs or panics.
        f.write_str("Dek(***)")
    }
}

/// Generate a fresh random 32-byte key from the OS CSPRNG, e.g. a KEK for an
/// operator to paste into config via `shisho encryption gen`.
pub fn generate_key() -> [u8; KEY_LEN] {
    let mut bytes = [0u8; KEY_LEN];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

/// Generate a fresh random data key from the OS CSPRNG.
pub fn generate_dek() -> Dek {
    Dek::from_bytes(generate_key())
}

/// Seal `plaintext` under `key` with AES-256-GCM and a fresh random nonce.
/// Output layout is `nonce (12) || ciphertext || tag (16)`, self-contained for
/// [`open`].
pub fn seal(key: &[u8; KEY_LEN], plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from(nonce_bytes);
    // GCM encryption of an in-memory buffer cannot fail.
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .expect("AES-256-GCM encryption of an in-memory buffer cannot fail");
    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    out
}

/// Open a blob produced by [`seal`]. Returns [`CryptoError::Decrypt`] if the
/// key is wrong or the data was tampered with.
pub fn open(key: &[u8; KEY_LEN], blob: &[u8]) -> Result<Vec<u8>> {
    let min = NONCE_LEN + TAG_LEN;
    if blob.len() < min {
        return Err(CryptoError::TooShort {
            got: blob.len(),
            min,
        });
    }
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key));
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let nonce = Nonce::try_from(nonce_bytes).map_err(|_| CryptoError::Decrypt)?;
    cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| CryptoError::Decrypt)
}

/// Wrap a data key under a key-encryption key (the keyfile's `dek_wrapped`).
pub fn wrap_dek(kek: &[u8; KEY_LEN], dek: &Dek) -> Vec<u8> {
    seal(kek, dek.as_bytes())
}

/// Unwrap a data key previously produced by [`wrap_dek`]. Returns
/// [`CryptoError::Decrypt`] for the wrong KEK.
pub fn unwrap_dek(kek: &[u8; KEY_LEN], wrapped: &[u8]) -> Result<Dek> {
    let plain = Zeroizing::new(open(kek, wrapped)?);
    let bytes: [u8; KEY_LEN] = plain
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::DekLength(plain.len()))?;
    Ok(Dek::from_bytes(bytes))
}

/// Derive the per-file AEAD key from the DEK via HKDF-SHA256. Stable for a given
/// DEK, distinct from the DEK itself, so file encryption survives KEK rotation.
pub fn file_key(dek: &Dek) -> [u8; KEY_LEN] {
    let hk = Hkdf::<Sha256>::new(None, dek.as_bytes());
    let mut okm = [0u8; KEY_LEN];
    hk.expand(FILE_KEY_INFO, &mut okm)
        .expect("32-byte HKDF-SHA256 output is within the allowed length");
    okm
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> [u8; KEY_LEN] {
        [b; KEY_LEN]
    }

    #[test]
    fn seal_open_round_trip() {
        let k = key(7);
        let blob = seal(&k, b"hello world");
        assert_eq!(open(&k, &blob).unwrap(), b"hello world");
    }

    #[test]
    fn seal_uses_a_fresh_nonce_each_call() {
        let k = key(7);
        // Same plaintext, different sealed bytes -> nonce is not reused.
        assert_ne!(seal(&k, b"same"), seal(&k, b"same"));
    }

    #[test]
    fn wrong_key_fails_to_open() {
        let blob = seal(&key(1), b"secret");
        assert!(matches!(open(&key(2), &blob), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn tampered_ciphertext_fails_authentication() {
        let k = key(3);
        let mut blob = seal(&k, b"secret");
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert!(matches!(open(&k, &blob), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn too_short_blob_is_rejected() {
        assert!(matches!(
            open(&key(1), &[0u8; 4]),
            Err(CryptoError::TooShort { got: 4, .. })
        ));
    }

    #[test]
    fn wrap_unwrap_round_trips_the_dek() {
        let kek = key(9);
        let dek = generate_dek();
        let wrapped = wrap_dek(&kek, &dek);
        let restored = unwrap_dek(&kek, &wrapped).unwrap();
        assert_eq!(dek.as_bytes(), restored.as_bytes());
    }

    #[test]
    fn unwrap_with_wrong_kek_fails() {
        let dek = generate_dek();
        let wrapped = wrap_dek(&key(1), &dek);
        assert!(matches!(
            unwrap_dek(&key(2), &wrapped),
            Err(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn generate_dek_is_random() {
        assert_ne!(generate_dek().as_bytes(), generate_dek().as_bytes());
    }

    #[test]
    fn file_key_is_deterministic_and_distinct_from_the_dek() {
        let dek = generate_dek();
        let fk1 = file_key(&dek);
        let fk2 = file_key(&dek);
        assert_eq!(fk1, fk2, "HKDF is deterministic for a given DEK");
        assert_ne!(&fk1, dek.as_bytes(), "file key must differ from the DEK");
        // A different DEK yields a different file key.
        assert_ne!(fk1, file_key(&generate_dek()));
    }

    #[test]
    fn sqlcipher_hex_is_64_lowercase_hex_chars() {
        let dek = Dek::from_bytes([0xAB; KEY_LEN]);
        let hex = dek.to_sqlcipher_hex();
        assert_eq!(&*hex, &"ab".repeat(KEY_LEN));
    }

    #[test]
    fn debug_does_not_leak_key_bytes() {
        let dek = Dek::from_bytes([0x42; KEY_LEN]);
        assert_eq!(format!("{dek:?}"), "Dek(***)");
    }
}
