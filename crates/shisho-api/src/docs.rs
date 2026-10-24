use axum::http::StatusCode;
use utoipa::openapi::RefOr;
use utoipa::{Modify, OpenApi};

#[derive(OpenApi)]
#[openapi(
    modifiers(&DefaultResponseDescriptions),
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
        super::profile::get_profile,
        super::profile::get_profile_raw,
        super::profile::list_versions,
        super::profile::edit_profile,
        super::profile::approve_proposal,
        super::profile::reject_proposal,
        super::verdicts::list_verdicts,
        super::verdicts::record_outcome,
        super::status::get_status,
        super::status::trigger_dump_refresh,
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
        super::profile::ProfileResponse,
        super::profile::PendingProposalDto,
        super::profile::VersionDto,
        super::profile::EditProfileRequest,
        super::verdicts::VerdictDto,
        super::verdicts::SessionGroup,
        super::verdicts::RecordOutcomeRequest,
        super::status::StatusResponse,
        super::status::LibraryStatus,
        super::status::UnresolvableEntry,
        super::status::MintedKeyStatus,
        shisho_services::health::HealthResponse,
        shisho_services::health::DbHealth,
        shisho_services::health::BackendHealth,
        shisho_services::health::MangabakaHealth,
        shisho_services::health::CacheHealth,
        super::admin_routes::AdminUser,
        super::admin_routes::EncryptionStatus,
        shisho_auth::keys::Scope,
    )),
    tags(
        (name = "shisho", description = "Core endpoints: health, instance info, and the authenticated-identity echo."),
        (name = "keys", description = "API key management. Requires the keys:manage scope. Scopes are granular resource:action strings; the aliases read/write/admin are accepted on creation and expanded."),
        (name = "connections", description = "Connected-application (OAuth grant) management. Lists the OAuth clients holding a live grant on the caller's account and revokes one. Requires the keys:manage scope, so a browser session qualifies but a connector's own token does not."),
        (name = "profile", description = "Taste-profile management: read the current profile and version history, edit it directly, and approve or reject pending consolidation proposals. Reads require profile:read, writes profile:write."),
        (name = "verdicts", description = "Verdict-log browsing and outcome recording. Lists the append-only recommendation log filtered and grouped by session, and records a reading outcome (appended as a feedback row, never an in-place edit). Reads require verdicts:read, the outcome append verdicts:write."),
        (name = "status", description = "Operational status: backend reachability, cache freshness, MangaBaka dump age + refresh state, the caller's unresolvable-series list (with Codex deep links), and minted-key state. The dump-refresh action runs in the background. Reads require status:read, the refresh action status:write."),
        (name = "admin", description = "Admin-only operations. In single-user mode the sole operator is the admin; in multi-user mode admin is the admin_emails/admin_groups allowlists, or the first provisioned user when both are empty."),
        (name = "setup", description = "First-run setup wizard. Token-gated and available only on an unconfigured instance; every route 404s once a credential is configured.")
    )
)]
pub struct ApiDoc;

/// OpenAPI requires every response to carry a `description`, and utoipa omits
/// the field when an annotation gives none (which is most of ours). Fill the
/// gaps with the status code's reason phrase so the spec stays valid.
struct DefaultResponseDescriptions;

impl Modify for DefaultResponseDescriptions {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        for item in openapi.paths.paths.values_mut() {
            let operations = [
                &mut item.get,
                &mut item.put,
                &mut item.post,
                &mut item.delete,
                &mut item.options,
                &mut item.head,
                &mut item.patch,
                &mut item.trace,
            ];
            for op in operations.into_iter().flatten() {
                for (status, response) in op.responses.responses.iter_mut() {
                    if let RefOr::T(response) = response
                        && response.description.is_empty()
                    {
                        response.description = status
                            .parse::<u16>()
                            .ok()
                            .and_then(|code| StatusCode::from_u16(code).ok())
                            .and_then(|code| code.canonical_reason())
                            .unwrap_or("Response")
                            .to_string();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_response_has_a_description() {
        let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
        for (path, item) in spec["paths"].as_object().unwrap() {
            for (method, op) in item.as_object().unwrap() {
                for (status, response) in op["responses"].as_object().unwrap() {
                    let description = response["description"].as_str().unwrap_or_default();
                    assert!(
                        !description.is_empty(),
                        "{method} {path} {status} has no description"
                    );
                }
            }
        }
    }
}
