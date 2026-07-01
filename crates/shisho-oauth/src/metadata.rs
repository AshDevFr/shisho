//! OAuth discovery documents an MCP connector fetches before it can start a
//! flow: protected-resource metadata (RFC 9728) and authorization-server
//! metadata (RFC 8414). Field names are the wire names from those RFCs, so the
//! structs serialize directly.

use serde::Serialize;

/// RFC 9728 protected-resource metadata, served at
/// `/.well-known/oauth-protected-resource`.
#[derive(Debug, Serialize)]
pub struct ProtectedResourceMetadata {
    /// Canonical identifier of the protected resource (this server instance).
    pub resource: String,
    /// Authorization servers that issue tokens for the resource.
    pub authorization_servers: Vec<String>,
    /// How a bearer token may be presented. This server only reads the header.
    pub bearer_methods_supported: Vec<String>,
    /// Scopes a client may request.
    pub scopes_supported: Vec<String>,
}

/// RFC 8414 authorization-server metadata, served at
/// `/.well-known/oauth-authorization-server`.
#[derive(Debug, Serialize)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: String,
    pub response_types_supported: Vec<String>,
    pub grant_types_supported: Vec<String>,
    pub code_challenge_methods_supported: Vec<String>,
    pub token_endpoint_auth_methods_supported: Vec<String>,
    pub scopes_supported: Vec<String>,
}

/// Build the protected-resource document for a deployment reachable at
/// `public_base` (scheme + host, no trailing slash), advertising `scopes`.
pub fn protected_resource(public_base: &str, scopes: Vec<String>) -> ProtectedResourceMetadata {
    ProtectedResourceMetadata {
        resource: public_base.to_string(),
        authorization_servers: vec![public_base.to_string()],
        bearer_methods_supported: vec!["header".to_string()],
        scopes_supported: scopes,
    }
}

/// Build the authorization-server document for `public_base`, advertising
/// `scopes`. Endpoints are absolute URLs under `public_base`.
pub fn authorization_server(public_base: &str, scopes: Vec<String>) -> AuthorizationServerMetadata {
    AuthorizationServerMetadata {
        issuer: public_base.to_string(),
        authorization_endpoint: format!("{public_base}/oauth/authorize"),
        token_endpoint: format!("{public_base}/oauth/token"),
        registration_endpoint: format!("{public_base}/oauth/register"),
        response_types_supported: vec!["code".to_string()],
        grant_types_supported: vec![
            "authorization_code".to_string(),
            "refresh_token".to_string(),
        ],
        code_challenge_methods_supported: vec![super::pkce::METHOD_S256.to_string()],
        token_endpoint_auth_methods_supported: vec!["none".to_string()],
        scopes_supported: scopes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_server_endpoints_are_under_public_base() {
        let md = authorization_server("https://shisho.example.com", vec!["example:read".into()]);
        assert_eq!(md.issuer, "https://shisho.example.com");
        assert_eq!(
            md.authorization_endpoint,
            "https://shisho.example.com/oauth/authorize"
        );
        assert_eq!(md.token_endpoint, "https://shisho.example.com/oauth/token");
        assert_eq!(md.code_challenge_methods_supported, vec!["S256"]);
        assert_eq!(md.token_endpoint_auth_methods_supported, vec!["none"]);
    }

    #[test]
    fn protected_resource_points_at_itself() {
        let md = protected_resource("https://shisho.example.com", vec![]);
        assert_eq!(md.resource, "https://shisho.example.com");
        assert_eq!(md.authorization_servers, vec!["https://shisho.example.com"]);
        assert_eq!(md.bearer_methods_supported, vec!["header"]);
    }
}
