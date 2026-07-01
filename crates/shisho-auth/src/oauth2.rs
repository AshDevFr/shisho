//! Plain OAuth2 authorization-code flow with PKCE (the `kind = "oauth2"` path).
//!
//! Providers without OIDC discovery (GitHub, Facebook) need explicit endpoints
//! and a userinfo call, since there is no signed ID token to read identity
//! from. Well-known providers carry built-in endpoint defaults; anything else
//! supplies `auth_url`, `token_url`, and `userinfo_url` in config. After the
//! code exchange the access token fetches the userinfo document, which is
//! mapped to a common [`OidcIdentity`].

use anyhow::Context;
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use serde_json::Value;
use shisho_config::ProviderConfig;
use shisho_db::repos::users::OidcIdentity;

use crate::oidc::title_case;
use crate::providers::{AuthRequest, HttpClient};

/// Resolved OAuth2 endpoints plus identity-fetch details for one provider.
pub struct OAuth2Provider {
    name: String,
    label: String,
    client_id: String,
    client_secret: Option<String>,
    auth_url: String,
    token_url: String,
    userinfo_url: String,
    /// Secondary endpoint for providers that hide email behind a separate call
    /// (GitHub `/user/emails`).
    email_url: Option<String>,
    redirect: String,
    scopes: Vec<String>,
}

/// Built-in endpoints for a well-known plain-OAuth2 provider.
struct WellKnown {
    auth_url: &'static str,
    token_url: &'static str,
    userinfo_url: &'static str,
    email_url: Option<&'static str>,
    scopes: &'static [&'static str],
}

/// Built-in defaults for providers identifiable by config `name`.
fn well_known(name: &str) -> Option<WellKnown> {
    match name {
        "github" => Some(WellKnown {
            auth_url: "https://github.com/login/oauth/authorize",
            token_url: "https://github.com/login/oauth/access_token",
            userinfo_url: "https://api.github.com/user",
            email_url: Some("https://api.github.com/user/emails"),
            scopes: &["read:user", "user:email"],
        }),
        "facebook" => Some(WellKnown {
            auth_url: "https://www.facebook.com/v18.0/dialog/oauth",
            token_url: "https://graph.facebook.com/v18.0/oauth/access_token",
            userinfo_url: "https://graph.facebook.com/me?fields=id,name,email,picture",
            email_url: None,
            scopes: &["email", "public_profile"],
        }),
        _ => None,
    }
}

impl OAuth2Provider {
    /// Assemble from config, filling endpoints from well-known defaults when the
    /// name matches and explicit values are absent.
    pub fn from_config(cfg: &ProviderConfig, redirect: String) -> anyhow::Result<Self> {
        let known = well_known(&cfg.name);
        let auth_url = pick(cfg.auth_url.as_deref(), known.as_ref().map(|k| k.auth_url))
            .with_context(|| format!("oauth2 provider `{}` missing auth_url", cfg.name))?;
        let token_url = pick(
            cfg.token_url.as_deref(),
            known.as_ref().map(|k| k.token_url),
        )
        .with_context(|| format!("oauth2 provider `{}` missing token_url", cfg.name))?;
        let userinfo_url = pick(
            cfg.userinfo_url.as_deref(),
            known.as_ref().map(|k| k.userinfo_url),
        )
        .with_context(|| format!("oauth2 provider `{}` missing userinfo_url", cfg.name))?;
        let email_url = known.as_ref().and_then(|k| k.email_url).map(str::to_string);
        let scopes = if !cfg.scopes.is_empty() {
            cfg.scopes.clone()
        } else {
            known
                .map(|k| k.scopes.iter().map(|s| s.to_string()).collect())
                .unwrap_or_default()
        };

        Ok(Self {
            name: cfg.name.clone(),
            label: cfg.label.clone().unwrap_or_else(|| title_case(&cfg.name)),
            client_id: cfg.client_id.clone(),
            client_secret: cfg.client_secret.clone(),
            auth_url,
            token_url,
            userinfo_url,
            email_url,
            redirect,
            scopes,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn authorize(&self) -> anyhow::Result<AuthRequest> {
        // Built inline (not via a helper) so the crate's endpoint type-state
        // generics never have to be named.
        let mut client = BasicClient::new(ClientId::new(self.client_id.clone()))
            .set_auth_uri(AuthUrl::new(self.auth_url.clone())?)
            .set_token_uri(TokenUrl::new(self.token_url.clone())?)
            .set_redirect_uri(RedirectUrl::new(self.redirect.clone())?);
        if let Some(secret) = &self.client_secret {
            client = client.set_client_secret(ClientSecret::new(secret.clone()));
        }
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut builder = client.authorize_url(CsrfToken::new_random);
        for scope in &self.scopes {
            builder = builder.add_scope(Scope::new(scope.clone()));
        }
        let (url, csrf) = builder.set_pkce_challenge(challenge).url();
        Ok(AuthRequest {
            url: url.to_string(),
            csrf: csrf.secret().clone(),
            nonce: String::new(),
            pkce_verifier: verifier.secret().clone(),
        })
    }

    pub async fn exchange(
        &self,
        http: &HttpClient,
        code: String,
        pkce_verifier: String,
    ) -> anyhow::Result<OidcIdentity> {
        let mut client = BasicClient::new(ClientId::new(self.client_id.clone()))
            .set_auth_uri(AuthUrl::new(self.auth_url.clone())?)
            .set_token_uri(TokenUrl::new(self.token_url.clone())?)
            .set_redirect_uri(RedirectUrl::new(self.redirect.clone())?);
        if let Some(secret) = &self.client_secret {
            client = client.set_client_secret(ClientSecret::new(secret.clone()));
        }
        let token_response = client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier))
            .request_async(http)
            .await
            .context("OAuth2 token exchange failed")?;
        let access_token = token_response.access_token().secret();

        let userinfo: Value = self.fetch(http, &self.userinfo_url, access_token).await?;
        let mut identity = self.map_identity(&userinfo);

        // GitHub-style providers omit a private email from the profile; pull it
        // from the dedicated emails endpoint and keep the primary, verified one.
        if identity.email.is_none()
            && let Some(email_url) = &self.email_url
        {
            let emails: Value = self.fetch(http, email_url, access_token).await?;
            identity.email = primary_email(&emails);
        }

        anyhow::ensure!(
            !identity.subject.is_empty(),
            "userinfo for provider `{}` had no subject/id",
            self.name
        );
        Ok(identity)
    }

    async fn fetch(&self, http: &HttpClient, url: &str, token: &str) -> anyhow::Result<Value> {
        let resp = http
            .get(url)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .header("User-Agent", "shisho")
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?
            .error_for_status()
            .with_context(|| format!("non-success status from {url}"))?;
        // The OAuth crates' bundled reqwest has no `json` feature, so decode
        // the body text ourselves.
        let body = resp
            .text()
            .await
            .with_context(|| format!("reading body from {url}"))?;
        serde_json::from_str(&body).with_context(|| format!("parsing JSON from {url}"))
    }

    /// Map a userinfo document to an identity. Reads OIDC-standard fields with
    /// GitHub/Facebook fallbacks (`id`, `login`, nested `picture`).
    fn map_identity(&self, json: &Value) -> OidcIdentity {
        let subject = json
            .get("sub")
            .or_else(|| json.get("id"))
            .map(stringify)
            .unwrap_or_default();
        let email = json
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string);
        let display_name = json
            .get("name")
            .or_else(|| json.get("login"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let avatar_url = json
            .get("avatar_url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                json.pointer("/picture/data/url")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .or_else(|| {
                json.get("picture")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });

        OidcIdentity {
            provider: self.name.clone(),
            subject,
            email,
            display_name,
            avatar_url,
            // Plain OAuth2 has no standard group claim; group-based admin is
            // OIDC-only. OAuth2 users get group memberships only via the email
            // allowlist or first-user fallback.
            groups: Vec::new(),
        }
    }
}

fn pick(explicit: Option<&str>, default: Option<&str>) -> Option<String> {
    explicit
        .filter(|s| !s.trim().is_empty())
        .or(default)
        .map(str::to_string)
}

/// Render a JSON scalar (string or number) as a string for subject ids.
fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Pick the primary verified email from a GitHub `/user/emails` array, falling
/// back to the first verified, then the first entry.
fn primary_email(emails: &Value) -> Option<String> {
    let arr = emails.as_array()?;
    let by = |pred: &dyn Fn(&Value) -> bool| {
        arr.iter()
            .find(|e| pred(e))
            .and_then(|e| e.get("email"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    by(&|e| e.get("primary").and_then(Value::as_bool).unwrap_or(false))
        .or_else(|| by(&|e| e.get("verified").and_then(Value::as_bool).unwrap_or(false)))
        .or_else(|| by(&|_| true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> OAuth2Provider {
        OAuth2Provider {
            name: "mock".into(),
            label: "Mock".into(),
            client_id: "id".into(),
            client_secret: Some("secret".into()),
            auth_url: "https://p/auth".into(),
            token_url: "https://p/token".into(),
            userinfo_url: "https://p/userinfo".into(),
            email_url: None,
            redirect: "http://localhost/auth/callback/mock".into(),
            scopes: vec![],
        }
    }

    #[test]
    fn maps_generic_oidc_style_userinfo() {
        let id = provider().map_identity(&serde_json::json!({
            "sub": "u-1",
            "email": "a@example.com",
            "name": "Ada",
            "picture": "https://img/a.png",
        }));
        assert_eq!(id.subject, "u-1");
        assert_eq!(id.email.as_deref(), Some("a@example.com"));
        assert_eq!(id.display_name.as_deref(), Some("Ada"));
        assert_eq!(id.avatar_url.as_deref(), Some("https://img/a.png"));
    }

    #[test]
    fn maps_github_style_userinfo() {
        let id = provider().map_identity(&serde_json::json!({
            "id": 4242,
            "login": "ada",
            "avatar_url": "https://gh/a.png",
            "email": null,
        }));
        assert_eq!(id.subject, "4242", "numeric id stringified");
        assert_eq!(
            id.display_name.as_deref(),
            Some("ada"),
            "login as name fallback"
        );
        assert_eq!(id.avatar_url.as_deref(), Some("https://gh/a.png"));
        assert!(id.email.is_none());
    }

    #[test]
    fn primary_email_prefers_primary_verified() {
        let emails = serde_json::json!([
            {"email": "secondary@example.com", "primary": false, "verified": true},
            {"email": "primary@example.com", "primary": true, "verified": true},
        ]);
        assert_eq!(
            primary_email(&emails).as_deref(),
            Some("primary@example.com")
        );
    }

    #[test]
    fn well_known_defaults_fill_github() {
        let cfg = ProviderConfig {
            name: "github".into(),
            kind: shisho_config::ProviderKind::Oauth2,
            client_id: "id".into(),
            ..Default::default()
        };
        let p = OAuth2Provider::from_config(&cfg, "http://localhost/cb".into()).unwrap();
        assert_eq!(p.userinfo_url, "https://api.github.com/user");
        assert!(p.email_url.is_some());
        assert_eq!(p.scopes, vec!["read:user", "user:email"]);
    }
}
