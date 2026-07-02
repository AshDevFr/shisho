//! MCP server: one tool registry, two doors.
//!
//! [`McpService`] implements the rmcp server handler over the same
//! [`shisho_api::AppState`] the REST layer uses, so MCP tools and REST handlers
//! share one process state and one auth path — there is no second read/write
//! path to keep in sync.
//!
//! Transports:
//! - **stdio** — `shisho mcp stdio`, for local clients (Claude Code/Desktop).
//! - **Streamable HTTP** — `/mcp`, mounted by `shisho serve` when `mcp.enabled`
//!   is on, for remote and programmatic clients.
//!
//! The MCP spec's earlier HTTP+SSE transport was deprecated in favor of
//! Streamable HTTP and rmcp no longer ships a server for it, so it is
//! deliberately not implemented. Remote clients connect with the
//! streamable-HTTP client type (Claude Code: `"type": "http"`).
//!
//! Authentication: HTTP requests carry `Authorization: Bearer <api key>` and
//! are verified by the same code as the REST API before any MCP handling. The
//! stdio process verifies `SHISHO_API_KEY` at startup, unless
//! `mcp.stdio_no_auth = true` opts into local trust. Scopes are enforced per
//! tool against the granular `resource:action` model.
//!
//! The shipped tools (`ping`, `echo`) are a scaffold: replace them with your
//! own in [`tools`], each declaring its required scope in
//! [`service::required_scope`].

pub mod auth;
pub mod prompts;
pub mod service;
pub mod tools;
pub mod transport;

pub use service::McpService;
