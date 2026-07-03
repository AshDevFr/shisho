//! `lookup`: the full picture of one series in one call.

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::Deserialize;

use super::{tool_error, tool_json};
use crate::McpService;
use shisho_api::middleware::AuthContext;
use shisho_services::lookup;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LookupParams {
    /// A series identity: canonical `provider:id` (e.g. "mangabaka:112092",
    /// "mal:126287"), Codex notation ("plugin:mangabaka:112092"), or a title
    /// to resolve via provider search.
    pub id_or_title: String,
}

#[tool_router(router = lookup_router, vis = "pub(crate)")]
impl McpService {
    #[tool(
        name = "lookup",
        description = "The full picture of one series: canonical metadata from the MangaBaka backbone (title, description, genres, rating — with its source annotated), catalog coverage (Tsundoku), the caller's own ownership/read-state/rating (Codex), and the caller's verdict history, merged into one view. Accepts an external ID in any known notation, or a title. Partial views name what is missing in 'gaps'; use this to verify candidates before recommending."
    )]
    async fn lookup(
        &self,
        Parameters(params): Parameters<LookupParams>,
        Extension(auth): Extension<AuthContext>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = self.user_ctx(&auth).await?;
        let services = self.services()?;
        match lookup::lookup(services, &user, &params.id_or_title).await {
            Ok(view) => tool_json(&view),
            // LookupError Display strings are self-contained and already carry
            // the per-source gaps where relevant.
            Err(err) => Ok(tool_error(err.to_string())),
        }
    }
}
