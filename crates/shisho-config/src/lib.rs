//! Layered configuration: struct defaults -> config file -> local overlay
//! -> environment.
//!
//! The config file may be TOML or YAML; the provider is chosen from the file
//! extension. A sibling `<stem>.local.<ext>` file (e.g. `shisho.docker.toml`
//! -> `shisho.docker.local.toml`) is auto-merged on top of the base file
//! when present, so operators can pin secrets and per-host overrides without
//! touching the committed config. Environment variables use the
//! `SHISHO_` prefix with `__` as the nesting separator, e.g.
//! `SHISHO_SERVER__PORT=9000`, and override both files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use base64::Engine;
use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml, Yaml};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub auth: AuthConfig,
    pub logging: LoggingConfig,
    pub api: ApiConfig,
    pub app: AppSection,
    pub mcp: McpConfig,
    pub encryption: EncryptionConfig,
}

/// MCP server policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Serve the MCP Streamable HTTP transport at `/mcp` on the HTTP server.
    pub enabled: bool,
    /// Allow unauthenticated MCP access over stdio (`shisho mcp stdio` running as
    /// a local trusted process). When false, stdio requires a verified API key
    /// in the `SHISHO_API_KEY` environment variable (or `auth.api_key`).
    pub stdio_no_auth: bool,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            stdio_no_auth: false,
        }
    }
}

/// Instance identity. `name` is the display name shown by the UI and
/// `/api/v1/info`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSection {
    pub name: String,
}

impl Default for AppSection {
    fn default() -> Self {
        Self {
            name: "Shisho".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// Public base URL the server is reachable at (scheme + host + optional
    /// port, no trailing slash), used to build OAuth redirect URIs. Defaults to
    /// `http://{host}:{port}` when unset; set it explicitly behind a reverse
    /// proxy or for a registered OAuth callback.
    pub public_url: Option<String>,
}

impl ServerConfig {
    /// The public base URL, falling back to `http://{host}:{port}`.
    pub fn public_base(&self) -> String {
        self.public_url
            .clone()
            .map(|u| u.trim_end_matches('/').to_string())
            .unwrap_or_else(|| format!("http://{}:{}", self.host, self.port))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: String,
    /// Emit JSON log lines instead of the human-readable format.
    pub json: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Mount the Scalar API docs UI at `/docs`.
    pub docs: bool,
}

/// Key material for at-rest database encryption (SQLCipher). `key_version`
/// selects the active KEK; older keys stay listed so an existing keyfile wrapped
/// by a previous version can still be rotated. Keys are named `key_v1`,
/// `key_v2`, ... and may be an inline base64 value or `env:VAR` indirection
/// (env: `SHISHO_ENCRYPTION__KEY_V1`, ...). Encryption is OFF when no key is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EncryptionConfig {
    pub key_version: u32,
    #[serde(flatten)]
    pub keys: BTreeMap<String, String>,
}

impl Default for EncryptionConfig {
    fn default() -> Self {
        Self {
            key_version: 1,
            keys: BTreeMap::new(),
        }
    }
}

impl EncryptionConfig {
    /// Whether at-rest encryption is configured. It is on when at least one
    /// `key_vN` is present; the empty default means "no encryption".
    pub fn is_enabled(&self) -> bool {
        !self.keys.is_empty()
    }

    /// Resolve the active KEK (`key_v{key_version}`) into 32 raw bytes, following
    /// `env:VAR` indirection and base64-decoding the value. Reads the process
    /// environment for `env:` references.
    pub fn resolve_kek(&self) -> anyhow::Result<[u8; 32]> {
        self.resolve_kek_version(self.key_version)
    }

    /// [`EncryptionConfig::resolve_kek`] with an injectable env lookup, so tests
    /// need not mutate the process environment.
    pub fn resolve_kek_with(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<[u8; 32]> {
        self.resolve_kek_version_with(self.key_version, lookup)
    }

    /// Resolve a specific KEK version, e.g. the older version that wrapped an
    /// existing keyfile during `encryption rotate-kek`.
    pub fn resolve_kek_version(&self, version: u32) -> anyhow::Result<[u8; 32]> {
        self.resolve_kek_version_with(version, |var| std::env::var(var).ok())
    }

    /// [`EncryptionConfig::resolve_kek_version`] with an injectable env lookup.
    pub fn resolve_kek_version_with(
        &self,
        version: u32,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<[u8; 32]> {
        let name = format!("key_v{version}");
        let raw = self
            .keys
            .get(&name)
            .with_context(|| format!("encryption.{name} is not set"))?;
        let value = match raw.strip_prefix("env:") {
            Some(var) => lookup(var)
                .with_context(|| format!("encryption.{name} references unset env var {var}"))?,
            None => raw.clone(),
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(value.trim())
            .with_context(|| format!("encryption.{name} is not valid base64"))?;
        let key: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            anyhow::anyhow!(
                "encryption.{name} must decode to 32 bytes, got {}",
                bytes.len()
            )
        })?;
        Ok(key)
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8080,
            public_url: None,
        }
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: "sqlite://data/shisho.db?mode=rwc".into(),
            max_connections: 16,
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            json: false,
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self { docs: true }
    }
}

/// Authentication policy. `single_user` is the zero-config default: one static
/// API key, no login UI. `multi_user` enables OIDC/OAuth2 login, sessions, and
/// per-user row scoping; it requires at least one `[[auth.providers]]` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub mode: AuthMode,
    /// Static API key for single-user mode. Also settable via `SHISHO_AUTH__API_KEY`.
    pub api_key: Option<String>,
    /// Identity providers offered on the login page (multi-user mode). Each
    /// `[[auth.providers]]` entry is one OIDC or OAuth2 backend.
    pub providers: Vec<ProviderConfig>,
    /// Session-cookie policy (JWT issuance, TTL, sliding refresh) and the HMAC
    /// secret that also signs the short-lived login-state cookie.
    pub session: SessionConfig,
    /// Admin email allowlist. When non-empty, exactly these users are admins.
    /// When empty (and `admin_groups` is empty), the first provisioned user is
    /// the admin.
    pub admin_emails: Vec<String>,
    /// Admin group allowlist. When non-empty, a user whose OIDC group claim
    /// (see `ProviderConfig.groups_claim`) intersects this list is an admin.
    /// Layered with `admin_emails`: a user is admin if either allowlist matches.
    pub admin_groups: Vec<String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            mode: AuthMode::SingleUser,
            api_key: None,
            providers: Vec::new(),
            session: SessionConfig::default(),
            admin_emails: Vec::new(),
            admin_groups: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    #[default]
    SingleUser,
    MultiUser,
}

impl AuthMode {
    /// The wire name of the mode (`single_user` / `multi_user`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SingleUser => "single_user",
            Self::MultiUser => "multi_user",
        }
    }
}

/// Session and login-cookie policy. The `secret` keys the HS256 JWTs for both
/// the session cookie and the short-lived OAuth login-state cookie. When unset
/// an ephemeral secret is generated at startup, so existing sessions reset on
/// restart: set it in any real deployment.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    pub secret: Option<String>,
    /// Session lifetime in hours (default 168 = 7 days).
    pub ttl_hours: u64,
    /// Re-issue the cookie when a request lands within this many hours of
    /// expiry (sliding refresh; default 24).
    pub refresh_threshold_hours: u64,
    /// Force the `Secure` cookie attribute. Leave `false` for plain-HTTP local
    /// use; set `true` when served behind TLS / a TLS-terminating proxy.
    pub secure_cookie: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            secret: None,
            ttl_hours: 168,
            refresh_threshold_hours: 24,
            secure_cookie: false,
        }
    }
}

/// A single login provider. OIDC providers are discovery-based (only `issuer`
/// is needed); plain OAuth2 providers either name a well-known backend
/// (`github`, `facebook`) or supply explicit endpoints.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Stable identifier used in URLs (`/auth/login/:name`) and stored on the
    /// `users` row as `provider`.
    pub name: String,
    pub kind: ProviderKind,
    pub client_id: String,
    /// Confidential-client secret. Omit for public PKCE clients. A value of the
    /// form `env:VAR_NAME` is resolved from the environment at load time.
    pub client_secret: Option<String>,
    /// OIDC issuer base URL; discovery runs at startup. Required for `oidc`.
    pub issuer: Option<String>,
    /// Requested scopes. OIDC defaults to `["openid","email","profile"]` when
    /// empty. To use group-based admin, request the provider's groups scope
    /// here (e.g. `"groups"`) so the claim is emitted.
    pub scopes: Vec<String>,
    /// ID-token/userinfo claim to read group membership from (group claim names
    /// are not standardized across OIDC providers). Defaults to `"groups"`.
    /// Only meaningful when `auth.admin_groups` is set.
    pub groups_claim: Option<String>,
    /// Login-button label. Defaults to a title-cased `name`.
    pub label: Option<String>,
    /// Plain-OAuth2 endpoint overrides (ignored for `oidc`). Well-known
    /// providers fill these in; explicit values win.
    pub auth_url: Option<String>,
    pub token_url: Option<String>,
    pub userinfo_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    Oidc,
    Oauth2,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oidc => "oidc",
            Self::Oauth2 => "oauth2",
        }
    }
}

/// Load configuration, applying defaults, then the base file (if present),
/// then the sibling `.local` overlay (if present), then env vars.
pub fn load(path: &Path) -> anyhow::Result<AppConfig> {
    let mut fig = Figment::from(Serialized::defaults(AppConfig::default()));

    if path.exists() {
        fig = merge_file(fig, path);
    }

    if let Some(local) = local_overlay_path(path)
        && local.exists()
    {
        fig = merge_file(fig, &local);
    }

    let mut cfg: AppConfig = fig
        .merge(Env::prefixed("SHISHO_").split("__"))
        .extract()
        .context("parsing configuration")?;

    resolve_provider_secrets(&mut cfg, |var| std::env::var(var).ok())?;
    Ok(cfg)
}

impl AppConfig {
    /// Validate cross-field invariants figment cannot express, called once at
    /// startup so misconfiguration fails loudly before serving (never at first
    /// login). Network-dependent checks (OIDC discovery, OAuth2 endpoint
    /// resolution) happen when the provider registry is built; this covers the
    /// static shape only.
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut seen = std::collections::HashSet::new();
        for p in &self.auth.providers {
            anyhow::ensure!(
                !p.name.trim().is_empty(),
                "an auth provider is missing its `name`"
            );
            anyhow::ensure!(
                seen.insert(p.name.as_str()),
                "duplicate auth provider name: {}",
                p.name
            );
            anyhow::ensure!(
                !p.client_id.trim().is_empty(),
                "auth provider `{}` is missing `client_id`",
                p.name
            );
            if p.kind == ProviderKind::Oidc {
                anyhow::ensure!(
                    p.issuer.as_deref().is_some_and(|s| !s.trim().is_empty()),
                    "oidc provider `{}` is missing `issuer`",
                    p.name
                );
            }
        }
        if self.auth.mode == AuthMode::MultiUser {
            anyhow::ensure!(
                !self.auth.providers.is_empty(),
                "auth.mode = \"multi_user\" requires at least one [[auth.providers]] entry"
            );
        }
        Ok(())
    }
}

/// Resolve `env:VAR_NAME` indirection in provider client secrets using
/// `lookup`. Kept lookup-injectable so it is testable without mutating the
/// process environment.
fn resolve_provider_secrets(
    cfg: &mut AppConfig,
    lookup: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<()> {
    for p in &mut cfg.auth.providers {
        if let Some(secret) = &p.client_secret
            && let Some(var) = secret.strip_prefix("env:")
        {
            let value = lookup(var).with_context(|| {
                format!(
                    "auth provider `{}` client_secret references unset env var {var}",
                    p.name
                )
            })?;
            p.client_secret = Some(value);
        }
    }
    Ok(())
}

fn merge_file(fig: Figment, path: &Path) -> Figment {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "yaml" | "yml" => fig.merge(Yaml::file(path)),
        _ => fig.merge(Toml::file(path)),
    }
}

/// Derive the sibling local-overlay path by inserting `.local` before the
/// final extension: `config/shisho.docker.toml` -> `config/shisho.docker.local.toml`.
/// Returns `None` when the input has no extension to anchor against.
fn local_overlay_path(path: &Path) -> Option<PathBuf> {
    let ext = path.extension()?.to_str()?;
    let stem = path.file_stem()?.to_str()?;
    let mut local = path.to_path_buf();
    local.set_file_name(format!("{stem}.local.{ext}"));
    Some(local)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn defaults_apply_when_no_file() {
        let cfg = load(&PathBuf::from("does-not-exist.toml")).unwrap();
        assert_eq!(cfg.server.port, 8080);
        assert_eq!(cfg.logging.level, "info");
    }

    #[test]
    fn local_overlay_overrides_base_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("shisho.docker.toml");
        let local = dir.path().join("shisho.docker.local.toml");

        std::fs::write(&base, "[server]\nport = 8080\n").unwrap();
        std::fs::write(&local, "[server]\nport = 9000\n").unwrap();

        let cfg = load(&base).unwrap();
        assert_eq!(cfg.server.port, 9000, "local overlay should win over base");
    }

    #[test]
    fn local_overlay_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("shisho.docker.toml");
        std::fs::write(&base, "[server]\nport = 8081\n").unwrap();

        // No sibling .local.toml exists: load must still succeed.
        let cfg = load(&base).unwrap();
        assert_eq!(cfg.server.port, 8081);
    }

    #[test]
    fn local_overlay_merges_dict_fields_recursively() {
        // Dicts merge field-by-field (only arrays are replaced wholesale), so
        // base + local can each contribute different keys to the same table.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("shisho.docker.toml");
        let local = dir.path().join("shisho.docker.local.toml");

        std::fs::write(
            &base,
            "[database]\nurl = \"sqlite://data/base.db?mode=rwc\"\nmax_connections = 32\n",
        )
        .unwrap();
        std::fs::write(&local, "[database]\nmax_connections = 64\n").unwrap();

        let cfg = load(&base).unwrap();
        // Base-only field survives the merge.
        assert_eq!(cfg.database.url, "sqlite://data/base.db?mode=rwc");
        // Local override wins for the field it set.
        assert_eq!(cfg.database.max_connections, 64);
    }

    #[test]
    fn encryption_is_disabled_by_default() {
        let cfg = AppConfig::default();
        assert!(!cfg.encryption.is_enabled());
        assert_eq!(cfg.encryption.key_version, 1);
    }

    #[test]
    fn resolve_kek_decodes_inline_base64() {
        let b64 = base64::engine::general_purpose::STANDARD.encode([0x11u8; 32]);
        let mut enc = EncryptionConfig::default();
        enc.keys.insert("key_v1".into(), b64);
        assert!(enc.is_enabled());
        assert_eq!(enc.resolve_kek().unwrap(), [0x11u8; 32]);
    }

    #[test]
    fn resolve_kek_follows_env_indirection() {
        let b64 = base64::engine::general_purpose::STANDARD.encode([0x22u8; 32]);
        let mut enc = EncryptionConfig::default();
        enc.keys.insert("key_v1".into(), "env:SOME_KEK_VAR".into());
        let got = enc
            .resolve_kek_with(|var| (var == "SOME_KEK_VAR").then(|| b64.clone()))
            .unwrap();
        assert_eq!(got, [0x22u8; 32]);
    }

    #[test]
    fn resolve_kek_rejects_wrong_length() {
        let b64 = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        let mut enc = EncryptionConfig::default();
        enc.keys.insert("key_v1".into(), b64);
        let err = enc.resolve_kek().unwrap_err().to_string();
        assert!(err.contains("32 bytes"), "unexpected error: {err}");
    }

    #[test]
    fn encryption_keys_flatten_under_the_section() {
        // `key_vN` entries live directly under [encryption] alongside key_version.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("shisho.toml");
        std::fs::write(
            &base,
            "[encryption]\nkey_version = 1\nkey_v1 = \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\"\n",
        )
        .unwrap();
        let cfg = load(&base).unwrap();
        assert!(cfg.encryption.is_enabled());
        assert_eq!(
            cfg.encryption.keys.get("key_v1").map(String::as_str),
            Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        );
    }

    #[test]
    fn auth_defaults_to_single_user() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.auth.mode, AuthMode::SingleUser);
        assert!(cfg.auth.providers.is_empty());
        cfg.validate().unwrap();
    }

    #[test]
    fn multi_user_requires_a_provider() {
        let mut cfg = AppConfig::default();
        cfg.auth.mode = AuthMode::MultiUser;
        assert!(
            cfg.validate().is_err(),
            "multi_user with no provider is invalid"
        );

        cfg.auth.providers.push(ProviderConfig {
            name: "google".into(),
            kind: ProviderKind::Oidc,
            client_id: "cid".into(),
            issuer: Some("https://accounts.google.com".into()),
            ..Default::default()
        });
        cfg.validate().unwrap();
    }

    #[test]
    fn validate_rejects_duplicate_and_incomplete_providers() {
        let mut cfg = AppConfig::default();
        cfg.auth.providers = vec![
            ProviderConfig {
                name: "p".into(),
                kind: ProviderKind::Oauth2,
                client_id: "a".into(),
                ..Default::default()
            },
            ProviderConfig {
                name: "p".into(),
                kind: ProviderKind::Oauth2,
                client_id: "b".into(),
                ..Default::default()
            },
        ];
        assert!(
            cfg.validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );

        // OIDC without an issuer is rejected.
        let mut cfg = AppConfig::default();
        cfg.auth.providers = vec![ProviderConfig {
            name: "o".into(),
            kind: ProviderKind::Oidc,
            client_id: "a".into(),
            ..Default::default()
        }];
        assert!(cfg.validate().unwrap_err().to_string().contains("issuer"));
    }

    #[test]
    fn provider_secret_env_indirection_resolves() {
        let mut cfg = AppConfig::default();
        cfg.auth.providers.push(ProviderConfig {
            name: "p".into(),
            client_secret: Some("env:MY_SECRET".into()),
            ..Default::default()
        });
        resolve_provider_secrets(&mut cfg, |v| (v == "MY_SECRET").then(|| "shh".to_string()))
            .unwrap();
        assert_eq!(cfg.auth.providers[0].client_secret.as_deref(), Some("shh"));
    }

    #[test]
    fn provider_secret_env_indirection_errors_when_unset() {
        let mut cfg = AppConfig::default();
        cfg.auth.providers.push(ProviderConfig {
            name: "p".into(),
            client_secret: Some("env:MISSING".into()),
            ..Default::default()
        });
        assert!(resolve_provider_secrets(&mut cfg, |_| None).is_err());
    }

    #[test]
    fn public_base_falls_back_to_host_port() {
        let mut cfg = AppConfig::default();
        assert_eq!(cfg.server.public_base(), "http://127.0.0.1:8080");
        cfg.server.public_url = Some("https://shisho.example.com/".into());
        assert_eq!(cfg.server.public_base(), "https://shisho.example.com");
    }

    #[test]
    fn local_overlay_path_derives_sibling_with_local_infix() {
        // Standard `<stem>.<ext>` cases.
        assert_eq!(
            local_overlay_path(Path::new("config/shisho.docker.toml")),
            Some(PathBuf::from("config/shisho.docker.local.toml"))
        );
        assert_eq!(
            local_overlay_path(Path::new("config/shisho.yaml")),
            Some(PathBuf::from("config/shisho.local.yaml"))
        );
        // file_stem() strips only the final extension, so multi-dot stems
        // round-trip cleanly.
        assert_eq!(
            local_overlay_path(Path::new("/etc/shisho/app.prod.yml")),
            Some(PathBuf::from("/etc/shisho/app.prod.local.yml"))
        );
        // Anchorless paths have no `.local.<ext>` to derive.
        assert_eq!(local_overlay_path(Path::new("shisho")), None);
    }
}
