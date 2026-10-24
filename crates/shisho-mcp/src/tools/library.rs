//! `search_library` + `list_libraries`: the caller's own Codex library.

use chrono::{DateTime, Utc};
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, tool, tool_router};
use serde::{Deserialize, Serialize};

use super::{CODEX_KEY_REVOKED, error_chain, swr_codex_unauthorized, tool_error, tool_json};
use crate::McpService;
use shisho_api::middleware::AuthContext;
use shisho_codex::CodexError;
use shisho_services::library::{self, LibraryError, LibraryQuery, LibrarySeries, LibrarySort};
use shisho_services::read_state::ReadState;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchLibraryParams {
    /// Title/author text, matched server-side by Codex.
    #[serde(default)]
    pub text: Option<String>,
    /// Case-insensitive genre membership.
    #[serde(default)]
    pub genre: Option<String>,
    /// Read-state filter: "untouched", "inProgress", or "completed".
    #[serde(default)]
    pub state: Option<ReadState>,
    /// Codex libraries to search, by name or ID (see list_libraries); empty = all.
    #[serde(default)]
    pub libraries: Vec<String>,
    /// Result order: "rating" (the caller's highest-rated first, unrated last)
    /// or "title". Default: library order.
    #[serde(default)]
    pub sort: Option<LibrarySort>,
    /// Default 50, capped at 200.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Matches to skip, for the next page when 'truncated' is true.
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LibrariesResult {
    libraries: Vec<LibraryInfo>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LibraryInfo {
    id: String,
    name: String,
    series_count: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LibrarySearchResult {
    series: Vec<LibrarySeries>,
    /// Every series matching the query, across all pages.
    total: usize,
    /// More series match beyond this page; page on with 'offset'.
    truncated: bool,
    /// Age of the library snapshot the results came from.
    as_of: DateTime<Utc>,
}

#[tool_router(router = library_router, vis = "pub(crate)")]
impl McpService {
    #[tool(
        name = "list_libraries",
        annotations(read_only_hint = true, open_world_hint = true),
        description = "The caller's Codex libraries (name, ID, series count). A Codex instance often shelves more than manga (comics, cookbooks, artbooks); call this first and, when several libraries exist, ask the user which to draw from (one, several, or all), then pass the chosen names via the 'libraries' parameter of get_candidates and search_library."
    )]
    async fn list_libraries(
        &self,
        Extension(auth): Extension<AuthContext>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = self.user_ctx(&auth).await?;
        let services = self.services()?;
        match services
            .snapshots
            .codex()
            .list_libraries(&user.credential)
            .await
        {
            Ok(libraries) => tool_json(&LibrariesResult {
                libraries: libraries
                    .into_iter()
                    .map(|l| LibraryInfo {
                        id: l.id,
                        name: l.name,
                        series_count: l.series_count,
                    })
                    .collect(),
            }),
            Err(CodexError::Unauthorized { .. }) => Ok(tool_error(CODEX_KEY_REVOKED)),
            Err(err) => Ok(tool_error(format!("could not list Codex libraries: {err}"))),
        }
    }

    #[tool(
        name = "search_library",
        annotations(read_only_hint = true, open_world_hint = true),
        description = "Search the caller's own Codex library by title/author text, genre, and read state. Answers questions like 'did I read anything by this author?' or 'what fantasy series am I mid-way through?'. Each series carries the caller's own Codex rating (1-100) and rating notes when set; sort='rating' with state='completed' returns the caller's top shelf, the ground truth for taste. Pure library view: results are the caller's own series only. 'total' counts every match; when 'truncated' is true, page on with 'offset'."
    )]
    async fn search_library(
        &self,
        Parameters(params): Parameters<SearchLibraryParams>,
        Extension(auth): Extension<AuthContext>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = self.user_ctx(&auth).await?;
        let services = self.services()?;
        let offset = params.offset.unwrap_or(0);
        let query = LibraryQuery {
            text: params.text,
            genre: params.genre,
            state: params.state,
            libraries: params.libraries,
            sort: params.sort,
            limit: params.limit,
            offset: Some(offset),
        };
        let page = match library::search_library(services, &user, &query).await {
            Ok(page) => page,
            Err(err) => return Ok(map_error(&err)),
        };

        tool_json(&LibrarySearchResult {
            truncated: offset + page.series.len() < page.total,
            total: page.total,
            series: page.series,
            as_of: page.as_of,
        })
    }
}

fn map_error(err: &LibraryError) -> CallToolResult {
    match err {
        LibraryError::Snapshot(swr) if swr_codex_unauthorized(swr) => tool_error(CODEX_KEY_REVOKED),
        LibraryError::Search(CodexError::Unauthorized { .. }) => tool_error(CODEX_KEY_REVOKED),
        other => tool_error(error_chain(other)),
    }
}
