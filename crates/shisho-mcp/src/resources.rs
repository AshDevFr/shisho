//! MCP resources: the caller's taste profile, injectable into a session
//! without a tool round-trip.

use rmcp::ErrorData;
use rmcp::model::{
    ListResourcesResult, ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents,
};
use rmcp::service::{RequestContext, RoleServer};

use crate::McpService;
use shisho_db::repos::profiles;

pub const PROFILE_URI: &str = "shisho://profile";

/// The single advertised resource: the taste profile.
pub(crate) fn list() -> ListResourcesResult {
    let mut profile = Resource::new(PROFILE_URI, "taste-profile")
        .with_title("Taste profile")
        .with_description(
            "The caller's current taste profile (Markdown): loves, likes, dislikes, hard \
             allergens, soft red flags.",
        );
    profile.mime_type = Some("text/markdown".into());
    ListResourcesResult {
        resources: vec![profile],
        ..Default::default()
    }
}

/// Read the taste-profile resource for the caller. Resolves the caller's
/// `UserCtx` the same way the per-user tools do (their identity is not needed
/// to fetch the profile, but the profile is per-user, so the caller must be
/// resolvable), then renders the current version as Markdown.
pub(crate) async fn read(
    server: &McpService,
    request: &ReadResourceRequestParams,
    context: &RequestContext<RoleServer>,
) -> Result<ReadResourceResult, ErrorData> {
    if request.uri != PROFILE_URI {
        return Err(ErrorData::resource_not_found(
            format!("unknown resource {:?}", request.uri),
            None,
        ));
    }

    let auth = server.auth_context(context)?;
    let user = server.user_ctx(&auth).await?;

    let profile = profiles::get_or_default(&server.state.db, &user.user_sub)
        .await
        .map_err(|err| ErrorData::internal_error(format!("profile query failed: {err}"), None))?;

    let text = if profile.content.is_empty() {
        "No taste profile yet. Run the consolidate prompt (or propose_profile_update) to create \
         the first version."
            .to_owned()
    } else {
        format!(
            "<!-- taste profile v{}{} -->\n{}",
            profile.version,
            profile
                .updated_at
                .map(|t| format!(", updated {}", t.to_rfc3339()))
                .unwrap_or_default(),
            profile.content
        )
    };

    Ok(ReadResourceResult::new(vec![ResourceContents::text(
        text,
        PROFILE_URI,
    )]))
}
