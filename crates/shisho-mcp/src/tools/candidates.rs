//! `get_candidates`: the two recommendation pools.

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::Deserialize;

use super::{CODEX_KEY_REVOKED, error_chain, swr_codex_unauthorized, tool_error, tool_json};
use crate::McpService;
use shisho_api::middleware::AuthContext;
use shisho_services::candidates::{self, CandidatesError, Filters, Pool};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetCandidatesParams {
    /// Which pool: "library" (the caller's untouched backlog, primary),
    /// "acquirable" (Tsundoku catalog series the caller does not own, with
    /// release coverage), or "discover" (MangaBaka series matching the
    /// caller's taste that they do not own; works without Tsundoku).
    pub pool: Pool,
    /// Any-of genre filter, case-insensitive.
    #[serde(default)]
    pub genres: Vec<String>,
    /// Any-of tag filter, case-insensitive.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Exact publication status (e.g. "ended", "releasing").
    #[serde(default)]
    pub status: Option<String>,
    /// Codex libraries to draw from, by name or ID (see list_libraries);
    /// empty = all. Library pool only. Ownership exclusion always spans
    /// every library regardless of this scope.
    #[serde(default)]
    pub libraries: Vec<String>,
    /// Over-provision target; default 40, capped at 100.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Ranked candidates to skip: pass the previous call's offset + count to
    /// see the next slice when 'truncated' is true.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Include series MangaBaka rates pornographic. Off by default; set it
    /// only when the user asks for explicit titles.
    #[serde(default)]
    pub allow_explicit: bool,
}

#[tool_router(router = candidates_router, vis = "pub(crate)")]
impl McpService {
    #[tool(
        name = "get_candidates",
        annotations(read_only_hint = true, open_world_hint = true),
        description = "Generate recommendation candidates from one of three pools: 'library' (series the caller owns but has never read - the primary pool), 'acquirable' (well-rated Tsundoku catalog series the caller does not own), or 'discover' (MangaBaka series matching the caller's taste that they do not own, manga/manhwa/manhua only). Candidates are pre-filtered: owned titles never appear in 'acquirable', and series with a non-retracted verdict are excluded. Library and discover candidates are ranked by affinity to the caller's own ratings and completed series: 'score' orders them and 'because' names the caller's series each one resembles, with the shared genres/tags (cite these; never invent ratings). Use 'offset' to page when 'truncated' is true. Pornographic-rated series are excluded unless 'allowExplicit' is set; 'contentRating' shows the rating of the rest. Over-provisions so a verification pass can reject candidates; check 'asOf' to phrase data freshness honestly. 'enrichment' names the metadata source per candidate: 'full' (Tsundoku catalog), 'mangabaka' (the metadata backbone, where Tsundoku lacks the series), or 'none' (Codex-only - verify via web research)."
    )]
    async fn get_candidates(
        &self,
        Parameters(params): Parameters<GetCandidatesParams>,
        Extension(auth): Extension<AuthContext>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = self.user_ctx(&auth).await?;
        let services = self.services()?;
        let filters = Filters {
            genres: params.genres,
            tags: params.tags,
            status: params.status,
            libraries: params.libraries,
            limit: params.limit,
            offset: params.offset,
            allow_explicit: params.allow_explicit,
        };
        match candidates::get_candidates(services, &user, params.pool, &filters).await {
            Ok(set) => tool_json(&set),
            Err(err) => Ok(map_error(&err)),
        }
    }
}

fn map_error(err: &CandidatesError) -> CallToolResult {
    match err {
        CandidatesError::Codex(swr) if swr_codex_unauthorized(swr) => tool_error(CODEX_KEY_REVOKED),
        other => tool_error(error_chain(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::GetCandidatesParams;

    #[test]
    fn pool_parameter_is_a_flat_inline_string_enum() {
        // The required `pool` enum must reach claude.ai as a flat, inline string
        // enum: its tool-input serializer drops `oneOf`/`const` (and mishandles
        // `$ref`) enum values to null, which 400s the call.
        let schema = serde_json::to_value(schemars::schema_for!(GetCandidatesParams)).unwrap();
        let rendered = schema.to_string();
        assert!(
            !rendered.contains("oneOf"),
            "params schema must not use oneOf: {rendered}"
        );
        assert!(
            !rendered.contains("$ref"),
            "the pool enum must be inlined, not referenced: {rendered}"
        );
        let pool = &schema["properties"]["pool"];
        assert_eq!(pool["type"], "string", "pool inline as string: {schema}");
        let values: Vec<&str> = pool["enum"]
            .as_array()
            .expect("flat enum array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(values, ["library", "acquirable", "discover"]);
    }
}
