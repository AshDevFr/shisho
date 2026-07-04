//! Cookie crypto for multi-user auth.
//!
//! One HMAC secret (HS256, `auth.session.secret`) keys two short JWTs:
//!
//! - the **login-state cookie**, a stateless carrier for the OAuth CSRF state,
//!   OIDC nonce, and PKCE verifier between `/auth/login` and `/auth/callback`,
//!   and
//! - the **session cookie**, issued after a successful login.
//!
//! When `auth.session.secret` is unset an ephemeral secret is generated at
//! startup, so cookies reset on restart. That is fine for a quick local spin-up
//! but every real deployment must set the secret.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use rand::TryRng;
use rand::rngs::SysRng;
use serde::{Deserialize, Serialize};
use shisho_config::SessionConfig;

/// Login-state cookies live just long enough to complete the provider round
/// trip; 10 minutes covers a slow consent screen without lingering.
const LOGIN_STATE_TTL_SECS: i64 = 600;

/// HMAC key material plus the session-cookie policy, derived once at startup.
#[derive(Clone)]
pub struct SessionKeys {
    enc: EncodingKey,
    dec: DecodingKey,
    /// Session lifetime (hours).
    pub ttl_hours: i64,
    /// Re-issue the session cookie when a request lands within this many hours
    /// of expiry.
    pub refresh_threshold_hours: i64,
    /// Emit the `Secure` cookie attribute.
    pub secure_cookie: bool,
}

/// Session JWT claims. `sub` is the `users.id`; `iat`/`exp` are unix seconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClaims {
    pub sub: String,
    pub iat: i64,
    pub exp: i64,
}

/// The login-state JWT payload: everything `/auth/callback` needs to finish a
/// flow it did not start in the same process memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginState {
    pub provider: String,
    /// OAuth `state` (CSRF token) echoed by the provider.
    pub csrf: String,
    /// OIDC nonce; empty for plain OAuth2.
    #[serde(default)]
    pub nonce: String,
    pub pkce_verifier: String,
    /// Where to send the browser after a successful login.
    #[serde(default)]
    pub redirect_to: String,
    /// Force a fresh Codex-key mint on callback (the Status page's regenerate
    /// action), rather than the keep-if-valid behavior of a normal sign-in.
    #[serde(default)]
    pub regenerate: bool,
    pub exp: i64,
}

impl SessionKeys {
    /// Build key material from config, generating an ephemeral secret when none
    /// is configured.
    pub fn from_config(cfg: &SessionConfig) -> Self {
        let secret = cfg.secret.clone().unwrap_or_else(|| {
            let mut bytes = [0u8; 32];
            SysRng
                .try_fill_bytes(&mut bytes)
                .expect("OS RNG unavailable");
            tracing::warn!(
                "auth.session.secret is unset; generated an ephemeral secret. \
                 Sessions and pending logins reset on restart — set it in production."
            );
            URL_SAFE_NO_PAD.encode(bytes)
        });
        Self {
            enc: EncodingKey::from_secret(secret.as_bytes()),
            dec: DecodingKey::from_secret(secret.as_bytes()),
            ttl_hours: cfg.ttl_hours as i64,
            refresh_threshold_hours: cfg.refresh_threshold_hours as i64,
            secure_cookie: cfg.secure_cookie,
        }
    }

    /// Test-only constructor with a fixed secret and default policy.
    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test(secret: &str) -> Self {
        Self::from_config(&SessionConfig {
            secret: Some(secret.to_string()),
            ..Default::default()
        })
    }

    /// Issue a session JWT for `user_id`, expiring `ttl_hours` from now.
    pub fn issue_session(&self, user_id: &str) -> anyhow::Result<String> {
        let iat = now();
        let claims = SessionClaims {
            sub: user_id.to_string(),
            iat,
            exp: iat + self.ttl_hours * 3600,
        };
        Ok(encode(&Header::new(Algorithm::HS256), &claims, &self.enc)?)
    }

    /// Verify and decode a session JWT. Expired or tampered tokens error.
    pub fn verify_session(&self, token: &str) -> anyhow::Result<SessionClaims> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_aud = false;
        Ok(decode::<SessionClaims>(token, &self.dec, &validation)?.claims)
    }

    /// Whether a valid session is close enough to expiry to re-issue (sliding
    /// refresh): true once the remaining lifetime drops below
    /// `refresh_threshold_hours`.
    pub fn needs_refresh(&self, claims: &SessionClaims) -> bool {
        claims.exp - now() < self.refresh_threshold_hours * 3600
    }

    /// Sign a login-state cookie value (a JWT) with a fixed short TTL.
    pub fn encode_login_state(&self, mut state: LoginState) -> anyhow::Result<String> {
        state.exp = now() + LOGIN_STATE_TTL_SECS;
        Ok(encode(&Header::new(Algorithm::HS256), &state, &self.enc)?)
    }

    /// Verify and decode a login-state cookie value. Expired or tampered tokens
    /// error.
    pub fn decode_login_state(&self, token: &str) -> anyhow::Result<LoginState> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_aud = false;
        Ok(decode::<LoginState>(token, &self.dec, &validation)?.claims)
    }
}

/// Current unix time in seconds.
pub(crate) fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> LoginState {
        LoginState {
            provider: "google".into(),
            csrf: "csrf-abc".into(),
            nonce: "nonce-xyz".into(),
            pkce_verifier: "verifier-123".into(),
            redirect_to: "/".into(),
            regenerate: false,
            exp: 0,
        }
    }

    #[test]
    fn login_state_roundtrips() {
        let keys = SessionKeys::for_test("test-secret");
        let token = keys.encode_login_state(state()).unwrap();
        let decoded = keys.decode_login_state(&token).unwrap();
        assert_eq!(decoded.csrf, "csrf-abc");
        assert_eq!(decoded.nonce, "nonce-xyz");
        assert_eq!(decoded.pkce_verifier, "verifier-123");
        assert!(decoded.exp > now());
    }

    #[test]
    fn tampered_login_state_rejected() {
        let keys = SessionKeys::for_test("test-secret");
        let other = SessionKeys::for_test("different-secret");
        let token = keys.encode_login_state(state()).unwrap();
        assert!(other.decode_login_state(&token).is_err());
    }

    #[test]
    fn session_roundtrips() {
        let keys = SessionKeys::for_test("test-secret");
        let token = keys.issue_session("user-1").unwrap();
        let claims = keys.verify_session(&token).unwrap();
        assert_eq!(claims.sub, "user-1");
        assert!(claims.exp > now());
        assert!(
            !keys.needs_refresh(&claims),
            "a fresh 7-day session is not near expiry"
        );
    }

    #[test]
    fn expired_session_rejected() {
        let keys = SessionKeys::for_test("test-secret");
        // Encode directly with a past exp (the test module can reach `enc`).
        let claims = SessionClaims {
            sub: "user-1".into(),
            iat: now() - 7200,
            exp: now() - 3600,
        };
        let token = encode(&Header::new(Algorithm::HS256), &claims, &keys.enc).unwrap();
        assert!(keys.verify_session(&token).is_err());
    }

    #[test]
    fn tampered_session_rejected() {
        let keys = SessionKeys::for_test("test-secret");
        let other = SessionKeys::for_test("other-secret");
        let token = keys.issue_session("user-1").unwrap();
        assert!(other.verify_session(&token).is_err());
    }

    #[test]
    fn needs_refresh_near_expiry() {
        let keys = SessionKeys::for_test("test-secret");
        // Default refresh threshold is 24h; a session expiring in 1h is due.
        let claims = SessionClaims {
            sub: "user-1".into(),
            iat: now(),
            exp: now() + 3600,
        };
        assert!(keys.needs_refresh(&claims));
    }
}
