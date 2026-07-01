//! At-rest encryption operator commands.
//!
//! - `gen` mints a fresh 32-byte KEK and prints it as base64 for the operator to
//!   paste into `encryption.key_vN` (or a secret referenced by `env:VAR`). It is
//!   pure key generation: it reads no config and touches no data.
//! - `rotate-kek` re-wraps the existing data key under the current KEK version.
//!   Both the old and new `key_vN` must be present in config during the
//!   rotation; the database itself is never rewritten.
//! - `status` prints whether encryption is configured and which KEK version
//!   currently wraps the keyfile.

use std::path::PathBuf;

use anyhow::Context;
use base64::Engine;

/// `shisho encryption gen`: print a fresh base64 KEK. Stdout is just the key so it
/// can be piped into a secret store.
pub fn generate() -> anyhow::Result<()> {
    let key = shisho_crypto::generate_key();
    let b64 = base64::engine::general_purpose::STANDARD.encode(key);
    println!("{b64}");
    Ok(())
}

/// `shisho encryption rotate-kek --config <path>`: re-wrap the data key under the
/// current KEK version.
pub fn rotate_kek(config_path: PathBuf) -> anyhow::Result<()> {
    let cfg = shisho_config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    super::init_tracing(&cfg);
    shisho_db::keys::rotate_kek(&cfg)?;
    println!(
        "re-wrapped the data key under KEK v{}",
        cfg.encryption.key_version
    );
    Ok(())
}

/// `shisho encryption status --config <path>`: report the encryption state.
pub fn status(config_path: PathBuf) -> anyhow::Result<()> {
    let cfg = shisho_config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    let st = shisho_db::keys::status(&cfg);
    println!("enabled:        {}", st.enabled);
    println!(
        "kek_version:    {}",
        st.kek_version
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into())
    );
    println!(
        "last_rotated:   {}",
        st.last_rotated_at.as_deref().unwrap_or("-")
    );
    Ok(())
}
