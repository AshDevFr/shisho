use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    info(title = "shisho API", version = env!("CARGO_PKG_VERSION")),
    paths(
        super::health::health,
        super::info::info,
        super::whoami::whoami,
        super::keys::list_keys,
        super::keys::create_key,
        super::keys::delete_key,
        super::connections::list_connections,
        super::connections::revoke_connection,
        super::admin_routes::list_users,
        super::admin_routes::deactivate_user,
        super::admin_routes::activate_user,
        super::admin_routes::encryption_status,
        super::setup::setup_state,
        super::setup::generate_key,
    ),
    components(schemas(
        super::health::Health,
        super::info::InfoResponse,
        super::whoami::WhoAmI,
        super::setup::SetupStateResponse,
        super::setup::SetupKeyResponse,
        super::error::ErrorBody,
        super::error::ErrorDetail,
        super::keys::ApiKeyResponse,
        super::keys::CreateKeyRequest,
        super::keys::CreatedKeyResponse,
        super::connections::ConnectionResponse,
        super::admin_routes::AdminUser,
        super::admin_routes::EncryptionStatus,
        shisho_auth::keys::Scope,
    )),
    tags(
        (name = "shisho", description = "Core endpoints: health, instance info, and the authenticated-identity echo."),
        (name = "keys", description = "API key management. Requires the keys:manage scope. Scopes are granular resource:action strings; the aliases read/write/admin are accepted on creation and expanded."),
        (name = "connections", description = "Connected-application (OAuth grant) management. Lists the OAuth clients holding a live grant on the caller's account and revokes one. Requires the keys:manage scope, so a browser session qualifies but a connector's own token does not."),
        (name = "admin", description = "Admin-only operations. In single-user mode the sole operator is the admin; in multi-user mode admin is the admin_emails/admin_groups allowlists, or the first provisioned user when both are empty."),
        (name = "setup", description = "First-run setup wizard. Token-gated and available only on an unconfigured instance; every route 404s once a credential is configured.")
    )
)]
pub struct ApiDoc;
