//! `update_want_to_read`: add series to and remove them from the caller's
//! Codex want-to-read queue. Grading the queue is `get_candidates` with the
//! `wantToRead` pool; this tool only edits it, and never reorders it.

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::Deserialize;

use super::{CODEX_KEY_REVOKED, swr_codex_unauthorized, tool_error, tool_json};
use crate::McpService;
use shisho_api::middleware::AuthContext;
use shisho_services::want_to_read;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateWantToReadParams {
    /// Series to queue: external IDs in any known notation (e.g.
    /// "mangabaka:112092", as candidates carry them) or Codex series IDs.
    /// Only series in the caller's Codex library can be queued.
    #[serde(default)]
    pub add: Vec<String>,
    /// Series to unqueue, in the same notations. Clears every queue entry
    /// of the series, including entries for single books of it.
    #[serde(default)]
    pub remove: Vec<String>,
}

#[tool_router(router = want_to_read_router, vis = "pub(crate)")]
impl McpService {
    #[tool(
        name = "update_want_to_read",
        // Removal drops queue entries, so this is not additive-only.
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true),
        description = "Add series to or remove them from the user's Codex want-to-read queue, only when the user asks. Series are named by external ID (as candidates carry them) or Codex series ID, and must already be in the user's Codex library: anything else comes back 'notInLibrary' and is not sent. Each item reports added, alreadyQueued, removed, notQueued, notInLibrary or failed; 'gap' explains failures (relay it). Queue order is never changed. To grade the queue, use get_candidates with pool 'wantToRead'."
    )]
    async fn update_want_to_read(
        &self,
        Parameters(params): Parameters<UpdateWantToReadParams>,
        Extension(auth): Extension<AuthContext>,
    ) -> Result<CallToolResult, ErrorData> {
        if params.add.is_empty() && params.remove.is_empty() {
            return Ok(tool_error("give at least one series in 'add' or 'remove'"));
        }
        let user = self.user_ctx(&auth).await?;
        let services = self.services()?;
        let snapshot = match services
            .snapshots
            .get(&user.user_sub, &user.credential)
            .await
        {
            Ok(hit) => hit.value,
            Err(err) if swr_codex_unauthorized(&err) => return Ok(tool_error(CODEX_KEY_REVOKED)),
            Err(err) => {
                return Ok(tool_error(format!(
                    "reading the library from Codex failed: {err}"
                )));
            }
        };
        let report = want_to_read::update(
            services.snapshots.codex(),
            &user.credential,
            &snapshot,
            &params.add,
            &params.remove,
        )
        .await;
        tool_json(&report)
    }
}
