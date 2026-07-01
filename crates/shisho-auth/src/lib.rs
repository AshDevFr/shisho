//! Authentication domain: API key material and scopes, their persistence, the
//! first-run bootstrap key, and the multi-user login stack (OIDC/OAuth2
//! provider flows, session-cookie crypto). Deliberately axum-free; the
//! middleware and routes that wire this onto `/api/v1` live in `shisho-api`.

pub mod admin;
pub mod bootstrap;
pub mod keys;
// `oauth2` shadows the external crate name inside this module tree; refer to
// the dependency as `::oauth2` from here if ever needed.
pub mod oauth2;
pub mod oidc;
pub mod providers;
pub mod repo;
pub mod session;

#[cfg(any(test, feature = "test-util"))]
pub mod test_util;
