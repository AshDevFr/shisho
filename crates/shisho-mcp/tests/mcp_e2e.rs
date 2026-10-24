//! End-to-end MCP flow over the in-process transport: a real domain-configured
//! `AppState` (services + Codex identity resolver + DEK) against a wiremock
//! Codex, a stored encrypted credential, and the tools driven through a
//! connected rmcp client. Proves a recommend-shaped session resolves the
//! caller's `codex_user_sub` from `/api/v1/auth/me` and records verdict rows
//! keyed on it. No live backends.

use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use sea_orm::EntityTrait;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use shisho_api::AppState;
use shisho_api::middleware::AuthContext;
use shisho_api::test_util::test_state_with_codex;
use shisho_auth::keys::Scope;
use shisho_mcp::McpService;
use shisho_mcp::auth::McpIdentity;

/// The `/api/v1/auth/me` id every stored credential resolves to — the caller's
/// authoritative `codex_user_sub`.
const CODEX_SUB: &str = "550e8400-e29b-41d4-a716-446655440000";

fn fixture(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/../../tests/fixtures/codex/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

/// Mount the Codex endpoints a library-pool recommend flow touches: identity
/// (`/me`), the ratings-presence call, and the paginated series list.
async fn mount_codex(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("auth_me")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user/ratings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("user_ratings")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/full"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("snapshot_page1")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/series/full"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("snapshot_page2")))
        .mount(server)
        .await;
}

/// Build the domain-configured state, seed a user with a stored encrypted Codex
/// key, and serve `McpService` (as the stdio path would) with that user's
/// identity. Returns a connected client and the shared state.
async fn connect(server: &MockServer) -> (RunningService<RoleClient, ()>, Arc<AppState>) {
    let state = test_state_with_codex(&server.uri()).await;

    // Provision the user and seal a Codex key for them, so identity resolution
    // has a credential to validate against `/me`.
    shisho_db::repos::users::find_or_create(
        &state.db,
        &shisho_db::repos::users::OidcIdentity {
            provider: "mock".into(),
            subject: "sub-1".into(),
            ..Default::default()
        },
    )
    .await
    .expect("provision user");
    // `find_or_create` assigns a generated id; overwrite with a known one for
    // the identity/AuthContext to reference.
    let generated = shisho_db::repos::users::list_all(&state.db).await.unwrap()[0]
        .id
        .clone();
    let dek = state.dek.as_ref().unwrap();
    shisho_db::repos::users::set_codex_credential(
        &state.db,
        &generated,
        CODEX_SUB,
        "stored-codex-key",
        dek,
        1,
    )
    .await
    .expect("seal codex credential");

    let identity = McpIdentity::Local(AuthContext {
        key_id: None,
        user_id: Some(generated),
        scopes: Scope::all(),
    });

    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let service = McpService::new(state.clone(), identity);
    tokio::spawn(async move {
        if let Ok(running) = service.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    let client = ().serve(client_io).await.expect("client initialize");
    (client, state)
}

#[tokio::test]
async fn record_verdict_resolves_the_caller_and_persists_a_row() {
    let codex = MockServer::start().await;
    mount_codex(&codex).await;
    let (client, state) = connect(&codex).await;

    let result = client
        .call_tool(
            CallToolRequestParams::new("record_verdict").with_arguments(
                serde_json::json!({
                    "id": "mangabaka:112092",
                    "tier": "S",
                    "rationale": "fits the profile",
                    "verified": true
                })
                .as_object()
                .cloned()
                .unwrap(),
            ),
        )
        .await
        .expect("record_verdict call");
    assert_ne!(result.is_error, Some(true), "{result:?}");

    // The verdict landed keyed on the `/me` id (not the Shisho user id).
    let rows = shisho_db::entities::verdict::Entity::find()
        .all(&state.db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "one verdict row written");
    assert_eq!(rows[0].user_sub, CODEX_SUB, "keyed on codex_user_sub");
    assert_eq!(rows[0].tier, "S");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn get_candidates_library_pool_returns_the_untouched_backlog() {
    let codex = MockServer::start().await;
    mount_codex(&codex).await;
    let (client, _state) = connect(&codex).await;

    let result = client
        .call_tool(
            CallToolRequestParams::new("get_candidates").with_arguments(
                serde_json::json!({ "pool": "library" })
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .expect("get_candidates call");
    assert_ne!(result.is_error, Some(true), "{result:?}");

    // Tsundoku is unconfigured in this state, so every candidate degrades to
    // enrichment=none — but the pool resolves the caller's library snapshot
    // end to end (identity → ratings → paginated series).
    let structured = result.structured_content.expect("structured content");
    assert_eq!(structured["pool"], "library");
    assert!(
        structured["candidates"].as_array().is_some(),
        "a candidate set: {structured}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn search_library_flags_a_partial_page_and_pages_on_with_offset() {
    let codex = MockServer::start().await;
    mount_codex(&codex).await;
    let (client, _state) = connect(&codex).await;

    let search = |args: serde_json::Value| {
        let client = &client;
        async move {
            let result = client
                .call_tool(
                    CallToolRequestParams::new("search_library")
                        .with_arguments(args.as_object().cloned().unwrap()),
                )
                .await
                .expect("search_library call");
            assert_ne!(result.is_error, Some(true), "{result:?}");
            result.structured_content.expect("structured content")
        }
    };

    let first = search(serde_json::json!({ "limit": 1 })).await;
    let total = first["total"].as_u64().expect("total");
    assert!(
        total > 1,
        "the fixture library holds several series: {first}"
    );
    assert_eq!(first["series"].as_array().unwrap().len(), 1);
    assert_eq!(first["truncated"], true);

    let last = search(serde_json::json!({ "limit": 1, "offset": total - 1 })).await;
    assert_eq!(last["series"].as_array().unwrap().len(), 1);
    assert_eq!(last["truncated"], false);

    client.cancel().await.unwrap();
}
