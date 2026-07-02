//! Provider registry: the configured login backends, built once at startup.
//!
//! OIDC entries run discovery here (network I/O), so a bad issuer fails at
//! startup rather than at first login. OAuth2 entries resolve their endpoints
//! from config or well-known defaults. The registry preserves config order so
//! the login page lists providers as the operator wrote them, and it owns the
//! single shared HTTP client every flow uses.

use shisho_config::{AppConfig, ProviderKind};
use shisho_db::repos::users::OidcIdentity;

use crate::oauth2::OAuth2Provider;
use crate::oidc::OidcProvider;

/// The HTTP client type shared by the OIDC and OAuth2 flows. Both crates bundle
/// the same reqwest, so one client serves both.
pub type HttpClient = openidconnect::reqwest::Client;

/// The browser-redirect URL plus the secrets the login-state cookie must carry
/// to `/auth/callback`.
pub struct AuthRequest {
    pub url: String,
    pub csrf: String,
    /// OIDC nonce; empty for plain OAuth2.
    pub nonce: String,
    pub pkce_verifier: String,
}

/// One configured login backend. Both variants are boxed so the enum stays
/// pointer-sized despite the OIDC provider's large discovered metadata.
pub enum Provider {
    Oidc(Box<OidcProvider>),
    OAuth2(Box<OAuth2Provider>),
}

impl Provider {
    pub fn name(&self) -> &str {
        match self {
            Provider::Oidc(p) => p.name(),
            Provider::OAuth2(p) => p.name(),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Provider::Oidc(p) => p.label(),
            Provider::OAuth2(p) => p.label(),
        }
    }

    pub fn kind(&self) -> ProviderKind {
        match self {
            Provider::Oidc(_) => ProviderKind::Oidc,
            Provider::OAuth2(_) => ProviderKind::Oauth2,
        }
    }

    /// Build the authorization redirect plus the state to stash in the cookie.
    pub fn authorize(&self) -> anyhow::Result<AuthRequest> {
        match self {
            Provider::Oidc(p) => p.authorize(),
            Provider::OAuth2(p) => p.authorize(),
        }
    }

    /// Complete the flow: exchange the code, resolve the user's identity, and
    /// return the raw access token alongside it (for a one-shot IdP-authorized
    /// action such as Shisho's Codex key mint). The token is never persisted.
    pub async fn exchange(
        &self,
        http: &HttpClient,
        code: String,
        pkce_verifier: String,
        nonce: String,
    ) -> anyhow::Result<(OidcIdentity, Option<String>)> {
        match self {
            Provider::Oidc(p) => p.exchange(http, code, pkce_verifier, nonce).await,
            Provider::OAuth2(p) => p.exchange(http, code, pkce_verifier).await,
        }
    }
}

/// Summary of a provider for the login page (`GET /auth/providers`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub name: String,
    pub kind: String,
    pub label: String,
}

/// All configured providers plus the shared HTTP client.
pub struct ProviderRegistry {
    http: HttpClient,
    providers: Vec<Provider>,
}

impl ProviderRegistry {
    /// Build every configured provider, running OIDC discovery. Fails at
    /// startup on any unreachable/invalid issuer or malformed endpoint.
    pub async fn build(cfg: &AppConfig) -> anyhow::Result<Self> {
        let http = build_http_client()?;
        let base = cfg.server.public_base();
        let mut providers = Vec::with_capacity(cfg.auth.providers.len());
        for p in &cfg.auth.providers {
            let redirect = format!("{base}/auth/callback/{}", p.name);
            let provider = match p.kind {
                ProviderKind::Oidc => {
                    Provider::Oidc(Box::new(OidcProvider::discover(&http, p, redirect).await?))
                }
                ProviderKind::Oauth2 => {
                    Provider::OAuth2(Box::new(OAuth2Provider::from_config(p, redirect)?))
                }
            };
            providers.push(provider);
        }
        Ok(Self { http, providers })
    }

    /// An empty registry (single-user mode, or tests with no real providers).
    pub fn empty() -> Self {
        Self {
            http: build_http_client().expect("building HTTP client"),
            providers: Vec::new(),
        }
    }

    /// A registry over pre-built providers, for tests pointing at a mock server.
    pub fn from_providers(providers: Vec<Provider>) -> Self {
        Self {
            http: build_http_client().expect("building HTTP client"),
            providers,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    pub fn get(&self, name: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.name() == name)
    }

    pub fn list(&self) -> Vec<ProviderInfo> {
        self.providers
            .iter()
            .map(|p| ProviderInfo {
                name: p.name().to_string(),
                kind: p.kind().as_str().to_string(),
                label: p.label().to_string(),
            })
            .collect()
    }
}

/// One HTTP client for all flows. `redirect(none)` is a hard requirement: the
/// OAuth token endpoint must never be followed through a redirect (SSRF / token
/// leakage guard).
fn build_http_client() -> anyhow::Result<HttpClient> {
    Ok(openidconnect::reqwest::ClientBuilder::new()
        .redirect(openidconnect::reqwest::redirect::Policy::none())
        .build()?)
}
