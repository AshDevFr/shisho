//! Shared test fixtures: a migrated in-memory database and an `AppState`. Auth
//! middleware tests, route handler tests, and the MCP crate's tests (via the
//! `test-util` feature) all build on these.

use std::sync::Arc;

use shisho_auth::providers::ProviderRegistry;
use shisho_auth::session::SessionKeys;
use shisho_config::AppConfig;

use crate::AppState;

pub use shisho_auth::test_util::{seed_key, test_db};

/// Fixed session secret for tests, so a test can reconstruct [`SessionKeys`]
/// and decode the cookies a handler emitted.
pub const TEST_SESSION_SECRET: &str = "test-session-secret";

/// Fixed setup token for tests: `test_state` enables the first-run wizard with
/// this token whenever the config is unconfigured (no api_key, no providers),
/// mirroring what `serve` does with a random token.
pub const TEST_SETUP_TOKEN: &str = "test-setup-token";

/// `AppState` over an in-memory database with no login providers (single-user /
/// API-key tests).
pub async fn test_state(cfg: AppConfig) -> Arc<AppState> {
    test_state_with_registry(cfg, ProviderRegistry::empty()).await
}

/// [`test_state`] with a caller-supplied provider registry (multi-user login
/// tests point it at a mock provider).
pub async fn test_state_with_registry(
    cfg: AppConfig,
    providers: ProviderRegistry,
) -> Arc<AppState> {
    // A test that sets `auth.session.secret` controls the full session policy
    // (TTL/refresh); otherwise use the fixed test secret with default policy so
    // tests can decode the cookies a handler emitted.
    let session = match cfg.auth.session.secret {
        Some(_) => SessionKeys::from_config(&cfg.auth.session),
        None => SessionKeys::for_test(TEST_SESSION_SECRET),
    };
    let db = test_db().await;
    let setup = Arc::new(if crate::setup::is_unconfigured(&cfg) {
        crate::setup::SetupState::enabled(TEST_SETUP_TOKEN)
    } else {
        crate::setup::SetupState::disabled()
    });
    Arc::new(AppState {
        db,
        cfg,
        providers: Arc::new(providers),
        session,
        setup,
        // Tests run against a plaintext in-memory DB; no DEK, so Codex minting
        // is skipped (the mint path is covered by the identity-broker tests).
        dek: None,
        codex_identity: None,
        services: None,
    })
}

/// An `AppState` with the domain surface fully wired against a Codex instance at
/// `codex_base_url` (a wiremock URI in tests): a DEK, the Codex identity
/// resolver, and the service registry. Multi-user mode, so per-user tools thread
/// a `user_id`. For end-to-end MCP / domain tests that need real `UserCtx`
/// resolution over a stored, encrypted Codex credential.
pub async fn test_state_with_codex(codex_base_url: &str) -> Arc<AppState> {
    let mut cfg = AppConfig::default();
    cfg.auth.mode = shisho_config::AuthMode::MultiUser;
    cfg.codex.base_url = codex_base_url.to_owned();
    // API-only MangaBaka (no dump, no fallback): resolves nothing, touches no
    // disk — the domain builds without a backbone dump in tests.
    cfg.mangabaka.api_fallback = false;
    cfg.mangabaka.offline_dump_url = None;

    let db = test_db().await;
    let dek = shisho_db::Dek::from_bytes([7u8; 32]);
    let services = crate::build_services(&cfg, db.clone())
        .await
        .expect("building test services");
    Arc::new(AppState {
        db,
        codex_identity: crate::build_codex_identity(&cfg),
        services,
        cfg,
        providers: Arc::new(ProviderRegistry::empty()),
        session: SessionKeys::for_test(TEST_SESSION_SECRET),
        setup: Arc::new(crate::setup::SetupState::disabled()),
        dek: Some(dek),
    })
}
