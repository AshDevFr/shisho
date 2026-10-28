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

/// The scope set granted to an MCP connector at consent: exactly what the MCP
/// tools need to run the recommend / consolidate workflow, and nothing more.
///
/// This is the union of every tool's required scope (see `required_scope` in
/// `shisho-mcp`; the `oauth_connector_grant_covers_every_tool` test there keeps
/// the two in lockstep). It deliberately excludes:
/// - `keys:manage` — a connector has no business minting API keys;
/// - `status:write` — triggering a MangaBaka dump refresh is a web-console
///   operator action, not a tool.
///
/// The OAuth `scope` parameter could narrow this later; today consent grants
/// the full connector set.
pub fn granted_scopes() -> Vec<Scope> {
    use shisho_auth::keys::Scope;
    vec![
        Scope::CANDIDATES_READ, // get_candidates
        Scope::LOOKUP_READ,     // lookup
        Scope::LIBRARY_READ,    // search_library, list_libraries
        Scope::LIBRARY_WRITE,   // update_want_to_read
        Scope::VERDICTS_WRITE,  // record_verdict, record_feedback
        Scope::PROFILE_READ,    // get_profile, the taste-profile resource
        Scope::PROFILE_WRITE,   // propose_profile_update
        Scope::STATUS_READ,     // status
    ]
}

/// The advertised `scopes_supported`, as wire strings, for the discovery
/// documents.
pub fn scopes_supported() -> Vec<String> {
    granted_scopes().iter().map(|s| s.to_string()).collect()
}
