//! PKCE (RFC 7636) verification. Only the `S256` method is supported: the
//! authorization server advertises `code_challenge_methods_supported: ["S256"]`
//! and rejects everything else, so `plain` is never accepted.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// The only supported PKCE method.
pub const METHOD_S256: &str = "S256";

/// Whether `code_verifier` matches `code_challenge` under S256:
/// `BASE64URL-NOPAD(SHA256(code_verifier)) == code_challenge`.
pub fn verify_s256(code_verifier: &str, code_challenge: &str) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let computed = URL_SAFE_NO_PAD.encode(hasher.finalize());
    // The challenge is already base64url; a constant-time compare is
    // unnecessary (both sides are derived from the same single-use code).
    computed == code_challenge
}

#[cfg(test)]
mod tests {
    use super::*;

    // The canonical example from RFC 7636 Appendix B.
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    #[test]
    fn rfc_example_verifies() {
        assert!(verify_s256(VERIFIER, CHALLENGE));
    }

    #[test]
    fn wrong_verifier_rejected() {
        assert!(!verify_s256("not-the-verifier", CHALLENGE));
        assert!(!verify_s256(VERIFIER, "not-the-challenge"));
    }
}
