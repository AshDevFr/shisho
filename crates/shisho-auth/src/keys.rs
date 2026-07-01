//! API key material and scopes.
//!
//! Keys are `shisho_` + base64url of 32 random bytes. The prefix makes keys
//! greppable and lets secret scanners identify them. Only the argon2id hash
//! is stored; the plaintext is shown exactly once at creation.
//!
//! # Scopes
//!
//! A scope is a non-hierarchical `(resource, action)` pair serialized as the
//! string `"resource:action"`, e.g. `"example:write"`. Scopes persist as a
//! JSON string array, e.g. `["example:read","example:write"]`.
//!
//! This template ships a generic vocabulary: an `example` data resource (with
//! `read`/`write`/`delete`) demonstrating the pattern, plus a `keys` resource
//! (with the single `manage` action) that gates API-key administration. **When
//! you add a real domain, replace `example` with your own resources** by
//! editing [`Resource`], [`Scope`], [`VALID_SCOPES`], and the alias map.
//!
//! **Actions are independent within a resource**: holding `example:write`
//! grants neither `example:read` nor `example:delete`. There is no `>=`
//! ordering — [`allows`] is plain set membership.
//!
//! The `read`/`write`/`admin` names are creation-time aliases ([`expand_alias`])
//! that expand to fixed granular sets, never stored. They are a convenience for
//! key creation, not a stored representation.

use std::fmt;
use std::str::FromStr;

use anyhow::anyhow;
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRng;
use rand::rngs::SysRng;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A protected resource a key may be scoped to. `Example` is the template's
/// placeholder data resource; `Keys` gates API-key administration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resource {
    Example,
    Keys,
}

impl Resource {
    fn as_str(self) -> &'static str {
        match self {
            Resource::Example => "example",
            Resource::Keys => "keys",
        }
    }

    fn parse(s: &str) -> Option<Resource> {
        match s {
            "example" => Some(Resource::Example),
            "keys" => Some(Resource::Keys),
            _ => None,
        }
    }
}

/// An action a key may perform on a [`Resource`]. Actions are independent:
/// no action implies another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Read,
    Write,
    Delete,
    Manage,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Read => "read",
            Action::Write => "write",
            Action::Delete => "delete",
            Action::Manage => "manage",
        }
    }

    fn parse(s: &str) -> Option<Action> {
        match s {
            "read" => Some(Action::Read),
            "write" => Some(Action::Write),
            "delete" => Some(Action::Delete),
            "manage" => Some(Action::Manage),
            _ => None,
        }
    }
}

/// A permission a key carries: one `(resource, action)` pair. Serializes as
/// the string `"resource:action"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Scope {
    pub resource: Resource,
    pub action: Action,
}

/// Every valid scope string, in canonical order. The full set is also the
/// effective grant of the `admin` alias and the single-user config key.
pub const VALID_SCOPES: &[&str] = &[
    "example:read",
    "example:write",
    "example:delete",
    "keys:manage",
];

impl Scope {
    pub const EXAMPLE_READ: Scope = Scope {
        resource: Resource::Example,
        action: Action::Read,
    };
    pub const EXAMPLE_WRITE: Scope = Scope {
        resource: Resource::Example,
        action: Action::Write,
    };
    pub const EXAMPLE_DELETE: Scope = Scope {
        resource: Resource::Example,
        action: Action::Delete,
    };
    pub const KEYS_MANAGE: Scope = Scope {
        resource: Resource::Keys,
        action: Action::Manage,
    };

    /// Every valid scope. Used by the `admin` alias and the config-key
    /// superuser path.
    pub fn all() -> Vec<Scope> {
        vec![
            Scope::EXAMPLE_READ,
            Scope::EXAMPLE_WRITE,
            Scope::EXAMPLE_DELETE,
            Scope::KEYS_MANAGE,
        ]
    }

    /// Whether `(resource, action)` is a permitted pairing. `keys` pairs only
    /// with `manage`; `manage` pairs only with `keys`; data resources pair with
    /// `read`/`write`/`delete`.
    fn is_valid(resource: Resource, action: Action) -> bool {
        match (resource, action) {
            (Resource::Keys, Action::Manage) => true,
            (Resource::Keys, _) => false,
            (_, Action::Manage) => false,
            _ => true,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.resource.as_str(), self.action.as_str())
    }
}

/// Error parsing a `"resource:action"` scope string. Carries a message that
/// lists the valid scopes, so the API can surface a useful 400.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeParseError(pub String);

impl fmt::Display for ScopeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScopeParseError {}

impl FromStr for Scope {
    type Err = ScopeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || {
            ScopeParseError(format!(
                "unknown scope {s:?} (expected one of: {})",
                VALID_SCOPES.join(", ")
            ))
        };
        let (resource, action) = s.split_once(':').ok_or_else(err)?;
        let resource = Resource::parse(resource).ok_or_else(err)?;
        let action = Action::parse(action).ok_or_else(err)?;
        if !Scope::is_valid(resource, action) {
            return Err(err());
        }
        Ok(Scope { resource, action })
    }
}

impl Serialize for Scope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Scope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl utoipa::PartialSchema for Scope {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::String)
            .enum_values(Some(VALID_SCOPES.iter().copied()))
            .description(Some("API key permission as `resource:action`."))
            .into()
    }
}

impl utoipa::ToSchema for Scope {}

/// Whether a key holding `scopes` may perform `action` on `resource`. Plain
/// set membership: no action implies another.
pub fn allows(scopes: &[Scope], resource: Resource, action: Action) -> bool {
    scopes.contains(&Scope { resource, action })
}

/// Expand a convenience alias (`read` / `write` / `admin`) into a granular
/// scope set. Returns `None` for any other input (callers treat unknown names
/// as granular scope strings).
///
/// - `read`  → example read
/// - `write` → example read/write/delete
/// - `admin` → every scope (including `keys:manage`)
pub fn expand_alias(name: &str) -> Option<Vec<Scope>> {
    match name {
        "read" => Some(vec![Scope::EXAMPLE_READ]),
        "write" => Some(vec![
            Scope::EXAMPLE_READ,
            Scope::EXAMPLE_WRITE,
            Scope::EXAMPLE_DELETE,
        ]),
        "admin" => Some(Scope::all()),
        _ => None,
    }
}

/// Serialize scopes to the stored JSON array format
/// (`["example:read","example:write"]`).
pub fn scopes_to_json(scopes: &[Scope]) -> String {
    serde_json::to_string(scopes).expect("Scope serialization is infallible")
}

/// Parse the stored JSON array format back into scopes. Unknown scope
/// strings are an error.
pub fn scopes_from_json(json: &str) -> Result<Vec<Scope>, serde_json::Error> {
    serde_json::from_str(json)
}

/// Generate a fresh API key: `shisho_` + base64url (no padding) of 32 bytes
/// from the OS RNG.
pub fn generate_key() -> String {
    let mut bytes = [0u8; 32];
    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("OS RNG unavailable");
    format!("shisho_{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// Hash a plaintext key with argon2id (default params), returning a PHC
/// string (`$argon2id$...`).
pub fn hash_key(key: &str) -> anyhow::Result<String> {
    let mut salt_bytes = [0u8; 16];
    SysRng
        .try_fill_bytes(&mut salt_bytes)
        .map_err(|e| anyhow!("OS RNG unavailable: {e}"))?;
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|e| anyhow!("encoding salt: {e}"))?;
    Ok(Argon2::default()
        .hash_password(key.as_bytes(), &salt)
        .map_err(|e| anyhow!("hashing key: {e}"))?
        .to_string())
}

/// Verify a plaintext key against a stored PHC hash. Malformed hashes
/// verify as false rather than erroring: a corrupt row must not 500.
pub fn verify_key(key: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(key.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[test]
    fn generated_key_has_prefix_and_entropy() {
        let key = generate_key();
        let secret = key.strip_prefix("shisho_").expect("missing shisho_ prefix");
        let bytes = URL_SAFE_NO_PAD.decode(secret).expect("not base64url");
        assert_eq!(bytes.len(), 32, "expected 32 bytes of entropy");
        assert_ne!(generate_key(), key, "keys must be unique across calls");
    }

    #[test]
    fn hash_then_verify_roundtrip() {
        let key = generate_key();
        let hash = hash_key(&key).unwrap();
        assert!(hash.starts_with("$argon2id$"), "not a PHC argon2id: {hash}");
        assert!(verify_key(&key, &hash));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let key = generate_key();
        let hash = hash_key(&key).unwrap();

        // Flip the final character to a different one.
        let mut mutated = key.clone();
        let last = mutated.pop().unwrap();
        mutated.push(if last == 'A' { 'B' } else { 'A' });

        assert!(!verify_key(&mutated, &hash));
        assert!(!verify_key(&key, "not-a-phc-string"));
    }

    #[test]
    fn scope_string_roundtrip() {
        for s in VALID_SCOPES {
            let scope: Scope = s.parse().expect("valid scope parses");
            assert_eq!(scope.to_string(), *s, "round-trips through Display");
        }
        assert_eq!(
            "example:write".parse::<Scope>().unwrap(),
            Scope::EXAMPLE_WRITE
        );
    }

    #[test]
    fn rejects_unknown_resource_action() {
        assert!("example:bogus".parse::<Scope>().is_err());
        assert!("bogus:read".parse::<Scope>().is_err());
        assert!("examplewrite".parse::<Scope>().is_err(), "missing colon");
        // The error message lists the valid set so the API 400 is useful.
        let err = "example:bogus".parse::<Scope>().unwrap_err();
        assert!(err.to_string().contains("example:read"), "got: {err}");
    }

    #[test]
    fn rejects_invalid_pairing() {
        assert!("keys:write".parse::<Scope>().is_err(), "keys only manage");
        assert!("keys:read".parse::<Scope>().is_err());
        assert!(
            "example:manage".parse::<Scope>().is_err(),
            "manage is keys-only"
        );
    }

    #[test]
    fn write_does_not_imply_read_or_delete() {
        let scopes = [Scope::EXAMPLE_WRITE];
        assert!(allows(&scopes, Resource::Example, Action::Write));
        assert!(!allows(&scopes, Resource::Example, Action::Read));
        assert!(!allows(&scopes, Resource::Example, Action::Delete));
        assert!(!allows(&scopes, Resource::Keys, Action::Manage));
    }

    #[test]
    fn alias_read_expansion() {
        assert_eq!(expand_alias("read").unwrap(), vec![Scope::EXAMPLE_READ]);
    }

    #[test]
    fn alias_write_includes_delete() {
        let write = expand_alias("write").unwrap();
        for scope in [
            Scope::EXAMPLE_READ,
            Scope::EXAMPLE_WRITE,
            Scope::EXAMPLE_DELETE,
        ] {
            assert!(write.contains(&scope), "write must include {scope}");
        }
        assert!(!write.contains(&Scope::KEYS_MANAGE), "write is not admin");
    }

    #[test]
    fn alias_admin_includes_keys_manage() {
        let admin = expand_alias("admin").unwrap();
        assert!(admin.contains(&Scope::KEYS_MANAGE));
        for scope in expand_alias("write").unwrap() {
            assert!(admin.contains(&scope), "admin must include {scope}");
        }
        assert_eq!(admin, Scope::all());
    }

    #[test]
    fn unknown_alias_is_none() {
        assert!(expand_alias("example:write").is_none());
        assert!(expand_alias("superuser").is_none());
    }

    #[test]
    fn json_array_roundtrip() {
        let scopes = vec![Scope::EXAMPLE_WRITE, Scope::EXAMPLE_READ];
        let json = scopes_to_json(&scopes);
        assert_eq!(json, r#"["example:write","example:read"]"#);
        assert_eq!(scopes_from_json(&json).unwrap(), scopes);
        assert!(scopes_from_json(r#"["entries:bogus"]"#).is_err());
    }
}
