//! OAuth 2.0 authorization server for the MCP transport.
//!
//! Some MCP clients (e.g. the claude.ai web connector) speak OAuth 2.1
//! (authorization-code + PKCE with discovery and dynamic client registration)
//! and cannot use a static API key. This crate is the authorization-server
//! *domain* layer: opaque secret generation + hashing ([`secret`]), PKCE
//! verification ([`pkce`]), the client/code/token repos ([`clients`], [`codes`],
//! [`tokens`]), and the discovery documents ([`metadata`]). The axum endpoints
//! that drive these live in `shisho-api`; the human-authentication step at
//! `/authorize` reuses the multi-user session login.
//!
//! Tokens issued here are accepted by the same bearer-auth path the REST API
//! and `/mcp` already use, so all downstream per-user tenancy and scope
//! enforcement is unchanged.

pub mod clients;
pub mod codes;
pub mod metadata;
pub mod pkce;
pub mod secret;
pub mod tokens;

use shisho_auth::keys::Scope;

/// Authorization-code lifetime. Short by design: a code is exchanged for a
/// token within one redirect round-trip.
pub const CODE_TTL_SECS: i64 = 60;
/// Access-token lifetime (1 hour). The client refreshes with its refresh token.
pub const ACCESS_TTL_SECS: i64 = 3600;
/// Refresh-token lifetime (30 days). Rotated on each use.
pub const REFRESH_TTL_SECS: i64 = 30 * 24 * 3600;

/// The scope set granted to a connector at consent: the `write` alias set
/// (read + write over the example resource). Deliberately excludes
/// `keys:manage` — an MCP connector has no business minting API keys. The OAuth
/// `scope` parameter could narrow this later; today consent grants the full set.
pub fn granted_scopes() -> Vec<Scope> {
    shisho_auth::keys::expand_alias("write").expect("`write` is a known alias")
}

/// The advertised `scopes_supported`, as wire strings, for the discovery
/// documents.
pub fn scopes_supported() -> Vec<String> {
    granted_scopes().iter().map(|s| s.to_string()).collect()
}
