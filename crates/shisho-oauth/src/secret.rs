//! Opaque secret material for the authorization server.
//!
//! Authorization codes and access/refresh tokens are `prefix` + base64url (no
//! padding) of 32 OS-random bytes. The prefix identifies the secret kind (so
//! [`crate`] callers can route a bearer token to the OAuth path) and keeps the
//! values greppable for secret scanners.
//!
//! Only the SHA-256 hex of a secret is ever stored. Unlike API keys (argon2),
//! a plain hash is correct here: these secrets carry 256 bits of entropy, so
//! there is nothing to brute-force, and a hash-as-primary-key gives the O(1)
//! lookup that `/mcp` needs on every request.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRng;
use rand::rngs::SysRng;
use sha2::{Digest, Sha256};

/// Prefix on access tokens. The bearer-auth path keys on this to try the OAuth
/// table before the API-key table.
pub const ACCESS_PREFIX: &str = "shisho_at_";
/// Prefix on refresh tokens.
pub const REFRESH_PREFIX: &str = "shisho_rt_";
/// Prefix on authorization codes.
pub const CODE_PREFIX: &str = "shisho_ac_";

/// Generate a fresh secret: `prefix` + base64url (no padding) of 32 random
/// bytes from the OS RNG.
fn generate(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("OS RNG unavailable");
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// A fresh authorization code.
pub fn new_code() -> String {
    generate(CODE_PREFIX)
}

/// A fresh access token.
pub fn new_access_token() -> String {
    generate(ACCESS_PREFIX)
}

/// A fresh refresh token.
pub fn new_refresh_token() -> String {
    generate(REFRESH_PREFIX)
}

/// A fresh public client identifier (dynamic client registration). Opaque
/// base64url; no prefix, since it is not a secret and is echoed in URLs.
pub fn new_client_id() -> String {
    generate("").trim_start_matches('_').to_string()
}

/// Whether `token` is shaped like an access token (used by the bearer-auth path
/// to skip the OAuth lookup for plain API keys).
pub fn is_access_token(token: &str) -> bool {
    token.starts_with(ACCESS_PREFIX)
}

/// SHA-256 hex digest of `secret`. The stored form of every code and token.
pub fn hash(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_carry_prefix_and_entropy() {
        let code = new_code();
        let secret = code.strip_prefix(CODE_PREFIX).expect("code prefix");
        let bytes = URL_SAFE_NO_PAD.decode(secret).expect("base64url");
        assert_eq!(bytes.len(), 32, "32 bytes of entropy");
        assert_ne!(new_code(), new_code(), "codes are unique");
        assert!(new_access_token().starts_with(ACCESS_PREFIX));
        assert!(new_refresh_token().starts_with(REFRESH_PREFIX));
    }

    #[test]
    fn is_access_token_discriminates() {
        assert!(is_access_token(&new_access_token()));
        assert!(!is_access_token(&new_refresh_token()));
        assert!(!is_access_token("shisho_plainapikey"));
    }

    #[test]
    fn hash_is_stable_and_distinct() {
        let token = new_access_token();
        assert_eq!(hash(&token), hash(&token), "deterministic");
        assert_eq!(hash(&token).len(), 64, "sha-256 hex is 64 chars");
        assert_ne!(hash(&token), hash(&new_access_token()), "distinct inputs");
    }
}
