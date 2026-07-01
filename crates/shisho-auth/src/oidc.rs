//! OIDC authorization-code flow with PKCE (the `kind = "oidc"` path).
//!
//! Discovery-based: only the issuer is configured, and the authorization,
//! token, userinfo, and JWKS endpoints come from
//! `/.well-known/openid-configuration`. The `openidconnect` crate verifies the
//! ID token's signature, `iss`, `aud`, `exp`, and `nonce`, so any compliant
//! provider works without per-provider special-casing.
//!
//! The provider's discovered metadata is cached on the struct; the lightweight
//! `CoreClient` is rebuilt per request from that metadata (no network), which
//! sidesteps naming the crate's long type-state generics.

use anyhow::Context;
use base64::Engine;
use shisho_config::ProviderConfig;
use shisho_db::repos::users::OidcIdentity;
// `access_token()` comes from the base oauth2 trait; bring it into scope without
// shadowing openidconnect's `TokenResponse` (which provides `id_token()`).
use oauth2::TokenResponse as _;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreIdToken, CoreProviderMetadata};
use openidconnect::{
    AccessToken, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};

use crate::providers::{AuthRequest, HttpClient};

const DEFAULT_OIDC_SCOPES: [&str; 3] = ["openid", "email", "profile"];

pub struct OidcProvider {
    name: String,
    label: String,
    metadata: CoreProviderMetadata,
    client_id: ClientId,
    client_secret: Option<ClientSecret>,
    redirect: RedirectUrl,
    scopes: Vec<String>,
    /// Claim to read group membership from. `None` uses the `"groups"` default
    /// (the ID token only); an explicit value also enables the userinfo fallback.
    groups_claim: Option<String>,
}

/// Default claim name when `groups_claim` is unset.
const DEFAULT_GROUPS_CLAIM: &str = "groups";

impl OidcProvider {
    /// Discover the issuer's metadata and assemble a provider. Network I/O: a
    /// bad or unreachable issuer fails here (at startup), never at first login.
    pub async fn discover(
        http: &HttpClient,
        cfg: &ProviderConfig,
        redirect: String,
    ) -> anyhow::Result<Self> {
        let issuer = cfg.issuer.clone().context("oidc provider missing issuer")?;
        let metadata = CoreProviderMetadata::discover_async(
            IssuerUrl::new(issuer.clone())
                .with_context(|| format!("invalid issuer URL: {issuer}"))?,
            http,
        )
        .await
        .with_context(|| format!("OIDC discovery failed for provider `{}`", cfg.name))?;

        let scopes = if cfg.scopes.is_empty() {
            DEFAULT_OIDC_SCOPES.iter().map(|s| s.to_string()).collect()
        } else {
            cfg.scopes.clone()
        };

        Ok(Self {
            name: cfg.name.clone(),
            label: cfg.label.clone().unwrap_or_else(|| title_case(&cfg.name)),
            metadata,
            client_id: ClientId::new(cfg.client_id.clone()),
            client_secret: cfg.client_secret.clone().map(ClientSecret::new),
            redirect: RedirectUrl::new(redirect.clone())
                .with_context(|| format!("invalid redirect URL: {redirect}"))?,
            scopes,
            groups_claim: cfg.groups_claim.clone(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Build the provider redirect URL plus the CSRF state, nonce, and PKCE
    /// verifier the caller must stash in the login-state cookie.
    pub fn authorize(&self) -> anyhow::Result<AuthRequest> {
        // Built inline (not via a helper) so the crate's long client type-state
        // generics never have to be named.
        let client = CoreClient::from_provider_metadata(
            self.metadata.clone(),
            self.client_id.clone(),
            self.client_secret.clone(),
        )
        .set_redirect_uri(self.redirect.clone());
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut builder = client.authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        );
        for scope in &self.scopes {
            builder = builder.add_scope(Scope::new(scope.clone()));
        }
        let (url, csrf, nonce) = builder.set_pkce_challenge(challenge).url();
        Ok(AuthRequest {
            url: url.to_string(),
            csrf: csrf.secret().clone(),
            nonce: nonce.secret().clone(),
            pkce_verifier: verifier.secret().clone(),
        })
    }

    /// Exchange the code, verify the ID token (signature/iss/aud/exp/nonce),
    /// and map the verified claims to an identity.
    pub async fn exchange(
        &self,
        http: &HttpClient,
        code: String,
        pkce_verifier: String,
        nonce: String,
    ) -> anyhow::Result<OidcIdentity> {
        let client = CoreClient::from_provider_metadata(
            self.metadata.clone(),
            self.client_id.clone(),
            self.client_secret.clone(),
        )
        .set_redirect_uri(self.redirect.clone());
        let token_response = client
            .exchange_code(AuthorizationCode::new(code))?
            .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier))
            .request_async(http)
            .await
            .context("OIDC token exchange failed")?;

        let id_token = token_response
            .id_token()
            .context("provider returned no ID token")?;
        let verifier = client.id_token_verifier();
        let claims = id_token
            .claims(&verifier, &Nonce::new(nonce))
            .context("ID token verification failed")?;

        let subject = claims.subject().as_str().to_string();
        anyhow::ensure!(!subject.is_empty(), "ID token has an empty subject");

        let email = claims.email().map(|e| e.as_str().to_string());
        let display_name = claims
            .name()
            .and_then(|n| n.get(None))
            .map(|n| n.as_str().to_string());
        let avatar_url = claims
            .picture()
            .and_then(|p| p.get(None))
            .map(|p| p.as_str().to_string());

        let groups = self
            .resolve_groups(http, id_token, token_response.access_token())
            .await;

        Ok(OidcIdentity {
            provider: self.name.clone(),
            subject,
            email,
            display_name,
            avatar_url,
            groups,
        })
    }

    /// Resolve group memberships for admin mapping. Reads the group claim from
    /// the verified ID token; if absent and a `groups_claim` was explicitly
    /// configured, falls back to the userinfo endpoint. Best-effort: any failure
    /// yields no groups (login never breaks over a missing/odd group claim).
    async fn resolve_groups(
        &self,
        http: &HttpClient,
        id_token: &CoreIdToken,
        access_token: &AccessToken,
    ) -> Vec<String> {
        let claim = self.groups_claim.as_deref().unwrap_or(DEFAULT_GROUPS_CLAIM);

        let groups = groups_from_id_token(id_token, claim);
        if !groups.is_empty() {
            return groups;
        }
        // Only pay for a userinfo round-trip when groups were explicitly opted
        // into (otherwise every login on a no-groups instance would call it).
        if self.groups_claim.is_some() {
            return self.userinfo_groups(http, access_token, claim).await;
        }
        Vec::new()
    }

    /// Fetch the userinfo document and read the group claim from it.
    async fn userinfo_groups(
        &self,
        http: &HttpClient,
        access_token: &AccessToken,
        claim: &str,
    ) -> Vec<String> {
        let Some(endpoint) = self.metadata.userinfo_endpoint() else {
            return Vec::new();
        };
        let response = http
            .get(endpoint.to_string())
            .bearer_auth(access_token.secret())
            .send()
            .await;
        // The bundled reqwest has no `json` feature, so read the body as text
        // and parse it ourselves.
        let body = match response {
            Ok(resp) => resp.text().await,
            Err(_) => return Vec::new(),
        };
        match body {
            Ok(body) => serde_json::from_str::<serde_json::Value>(&body)
                .map(|json| groups_from_claims(&json, claim))
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }
}

/// Read a group-list claim from a verified ID token.
///
/// The `openidconnect` Core claims type doesn't surface non-standard claims, and
/// parameterizing the whole client over a custom additional-claims type means
/// naming its long type-state generics (which this module deliberately avoids).
/// The crate has already verified the token's signature and standard claims, so
/// re-reading one extra field from the now-trusted payload is sound.
fn groups_from_id_token(id_token: &CoreIdToken, claim: &str) -> Vec<String> {
    let Ok(serde_json::Value::String(jwt)) = serde_json::to_value(id_token) else {
        return Vec::new();
    };
    groups_from_jwt_payload(&jwt, claim)
}

/// Decode a compact JWT's payload (`header.payload.signature`) and read `claim`.
fn groups_from_jwt_payload(jwt: &str, claim: &str) -> Vec<String> {
    let Some(payload) = jwt.split('.').nth(1) else {
        return Vec::new();
    };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    groups_from_claims(&json, claim)
}

/// Extract `claim` as an array of strings; any other shape yields an empty list.
fn groups_from_claims(claims: &serde_json::Value, claim: &str) -> Vec<String> {
    claims
        .get(claim)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Title-case a provider name for the default login-button label
/// (`google` -> `Google`).
pub(crate) fn title_case(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a compact JWT (`header.payload.sig`) whose payload is `claims`.
    fn jwt_with(claims: serde_json::Value) -> String {
        let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let header = b64(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = b64(serde_json::to_vec(&claims).unwrap().as_slice());
        format!("{header}.{payload}.signature")
    }

    #[test]
    fn groups_from_claims_reads_a_string_array() {
        let claims = json!({ "groups": ["admins", "staff"], "sub": "x" });
        assert_eq!(
            groups_from_claims(&claims, "groups"),
            vec!["admins", "staff"]
        );
    }

    #[test]
    fn groups_from_claims_honors_a_custom_claim_name() {
        let claims = json!({ "roles": ["a"], "groups": ["b"] });
        assert_eq!(groups_from_claims(&claims, "roles"), vec!["a"]);
    }

    #[test]
    fn groups_from_claims_tolerates_missing_or_wrong_shape() {
        assert!(groups_from_claims(&json!({ "sub": "x" }), "groups").is_empty());
        // A scalar (not an array) yields nothing rather than erroring.
        assert!(groups_from_claims(&json!({ "groups": "admins" }), "groups").is_empty());
        // Non-string array members are skipped.
        assert_eq!(
            groups_from_claims(&json!({ "groups": ["ok", 1, null] }), "groups"),
            vec!["ok"]
        );
    }

    #[test]
    fn groups_from_jwt_payload_decodes_the_middle_segment() {
        let jwt = jwt_with(json!({ "groups": ["admins"], "sub": "s" }));
        assert_eq!(groups_from_jwt_payload(&jwt, "groups"), vec!["admins"]);
    }

    #[test]
    fn groups_from_jwt_payload_is_empty_on_garbage() {
        assert!(groups_from_jwt_payload("not-a-jwt", "groups").is_empty());
        assert!(groups_from_jwt_payload("only.two", "groups").is_empty());
        assert!(groups_from_jwt_payload("h.@@@notb64@@@.s", "groups").is_empty());
    }
}
