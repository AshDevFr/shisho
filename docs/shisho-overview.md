---
title: Shisho Overview
generator: project-overview
generated: 2026-07-06
branch: main
commit: 1602453
commitFull: 1602453be91c18a330ca94b0ea1a42b46eb0b06b
tag: null
dirty: false
---

# Shisho - a self-hosted, library-aware manga recommendation service exposed as a remote MCP server

Shisho is a single self-hosted Rust binary that gives one person (or a small multi-user instance) proactive, taste-aware manga recommendations grounded in the library they already own. It is exposed primarily as a **remote MCP server**: an LLM client (Claude Code, or a scheduled `claude -p` run) drives the recommendation workflow through a small set of tools, while Shisho does all the deterministic work (ownership cross-checks, entity resolution, metadata joins, verdict bookkeeping) so the model never performs those joins in its context. The same binary serves a React web console and a REST API over the identical auth and data. The guiding principle is "fat tools, thin context": every mechanical join runs in Rust, and the model is left to reason about taste.

## What it plugs into (the three backends)

Shisho is defined by the three external systems it composes. It stores none of their data; it caches and cross-references them per request.

| Backend | Role | Tier | Credential |
| --- | --- | --- | --- |
| **Codex** | The user's manga **library** and ratings/read-state (the source of ownership truth). Also the metadata-match UI where unresolvable series are fixed. | Required | Per-user least-privilege **minted API key**, resolved per request against Codex `/api/v1/auth/me` |
| **Tsundoku** | The **acquirable** catalog and review queue (what is worth buying but not yet owned), plus a change feed. | Enhancement (Tier 0 deployments run without it) | Instance admin bearer |
| **MangaBaka** | The **metadata backbone**: a large offline dump (with full-text search) plus a live API fallback, used for series metadata and foreign-ID pivots. | Enhancement | None (public dump + API) |

Because ownership is authoritative only in Codex and is read with the caller's own minted key, ownership is never derived from the shared Tsundoku cache.

## Architecture

Shisho is a **pure Cargo workspace** (no root package) of one binary crate over thirteen `shisho-*` library crates plus a `migration` crate. The binary is a thin CLI shell; all behavior lives in the libraries, and the dependency direction is one-way: `shisho` -> `shisho-api` -> domain crates -> `shisho-db` / `shisho-config`.

### Crate layout

| Crate | Responsibility |
| --- | --- |
| `shisho` | Binary: clap-derive CLI (`serve`, `migrate`, `openapi`, `encryption`, `mcp`, `mangabaka`, `sweep`, `report`); builds `AppState`; registers background jobs on `serve`. |
| `shisho-api` | axum 0.8 router, REST handlers, auth middleware, OpenAPI doc (utoipa + Scalar), embedded SPA, `build_services` / `build_codex_identity` state assembly. |
| `shisho-auth` | API keys (argon2 + `resource:action` scopes), OIDC/OAuth2 login, JWT sessions, admin resolution, provider registry. |
| `shisho-oauth` | OAuth 2.1 authorization server (PKCE + dynamic client registration + token issuance) so MCP clients can obtain tokens. |
| `shisho-mcp` | The MCP server (rmcp 2): tool registry, `recommend`/`consolidate` prompts, taste-profile resource, Streamable HTTP transport + stdio, per-tool scope auth. |
| `shisho-services` | The domain brain: library snapshot, read-state derivation, candidate pools, four-leg lookup, verdict fold, reconciliation, sweeps, health probe, Codex identity resolver, Tsundoku cache. |
| `shisho-codex` | Typed Codex client (library list, ratings, `/me`, series search, key minting). |
| `shisho-tsundoku` | Typed Tsundoku client (acquirable list/detail, change feed, health). |
| `shisho-mangabaka` | MangaBaka provider: offline-dump download/ingest/promote, FTS, API fallback, backbone status. |
| `shisho-cache` | Stale-while-revalidate cache primitive shared by the per-user and global caches. |
| `shisho-crypto` | Envelope encryption primitives (AES-GCM + HKDF + zeroize), the wrapped-key keyfile. |
| `shisho-config` | Figment loader + `AppConfig` types (TOML/YAML + env). |
| `shisho-db` | sea-orm connect + SQLCipher keying, entities, repositories, migration runner, DEK bootstrap. |
| `shisho-ids` | `ExternalId` / provider normalization (`api:<source>:<id>` provenance parsing). |
| `migration` | sea-orm migrations. |

### Process shape

A single `shisho serve` process binds one HTTP listener and mounts, on the same port: the REST API under `/api/v1`, the auth and OAuth routes, the MCP Streamable HTTP transport at `/mcp`, the OpenAPI JSON and Scalar docs, and the embedded React SPA as the fallback route. It also starts the background scheduler and the Tsundoku feed poller. There is no separate worker process; scaling is single-instance by design (SQLite, in-memory caches).

### Backend stack

- Rust edition 2024; tokio 1 (full); axum 0.8; sea-orm 1 (`sqlx-sqlite`, tokio-rustls); utoipa + `utoipa-axum` 0.2 + `utoipa-scalar`.
- `rmcp` 2 for the MCP server (Streamable HTTP + stdio); `schemars` for tool parameter schemas; `minijinja` + `rust-embed` for prompt templates.
- `openidconnect` / `oauth2` (login), `jsonwebtoken` (sessions), `argon2` (API keys).
- `aes-gcm` + `hkdf` + `zeroize` (envelope keys); SQLCipher linked via `libsqlite3-sys` in place of plain SQLite.
- Figment for layered config; clap derive for the CLI; `reqwest` for outbound HTTP; `tokio-cron-scheduler` for background jobs; `tracing` for structured logging.

### Frontend stack

React 19 + Vite + TypeScript + Mantine + Zustand + TanStack Query/Router, in `web/`. The API client is fully typed and generated from the OpenAPI spec via `openapi-typescript` + `openapi-fetch` (`web/src/types/api.generated.ts` is generated, never hand-edited). Built to `web/dist` and embedded in the binary through `rust-embed` behind the `embed-frontend` feature. Tests use vitest + Testing Library + MSW. Lint/format via Biome.

## CLI commands

| Command | Description |
| --- | --- |
| `serve` | Start the HTTP server: REST + MCP + SPA, and register the background jobs. |
| `migrate` | Run pending database migrations and exit. |
| `openapi` | Write the OpenAPI spec to a file (default `web/openapi.json`). |
| `encryption gen` | Generate a fresh base64 KEK to paste into `encryption.key_vN`. |
| `encryption rotate-kek` | Re-wrap the data key under the current KEK version. |
| `encryption status` | Report whether encryption is configured and the active KEK version. |
| `mcp stdio` | Serve the MCP protocol over stdin/stdout for a local client. |
| `mangabaka refresh` | Download and ingest the offline MangaBaka dump (full pipeline, atomic promote). |
| `sweep reconcile` | Run the outcome-reconciliation sweep once (resolve settled flags, flag started series). |
| `sweep unresolvable` | Run the unresolvable-series scan once over warm library snapshots. |
| `report prompt` | Render the weekly-report prompt to stdout (piped into `claude -p` by the runner). |

Every config-taking command accepts `-c/--config` (default `config/shisho.toml`).

## API layer

All DTOs are `#[serde(rename_all = "camelCase")]` and utoipa-annotated; the spec is served at `/openapi.json` and rendered by Scalar at `/docs`.

### REST (`/api/v1`)

Public (no auth): `GET /health`, `GET /info`, plus the first-run setup routes. Everything else sits behind `require_auth`.

| Group | Endpoints | Notes |
| --- | --- | --- |
| Identity | `GET /whoami` | The caller's resolved auth context. |
| Keys | `/keys` (list/create/revoke) | Guarded by `keys:manage`. |
| Connections | `/connections` (list/revoke) | OAuth-client connections for the user. |
| Taste profile | `GET /profile`, `GET /profile/raw`, `GET /profile/versions`, `PUT /profile`, `POST /profile/proposals/{version}/approve` and `/reject` | `profile:read` / `profile:write`; keyed on the caller's Codex identity. |
| Verdicts | `GET /verdicts` (filtered, session-grouped), `POST /verdicts/{id}/outcome` | Append-only log; the outcome POST appends a feedback row, it does not mutate. `verdicts:read` / `verdicts:write`. |
| Status | `GET /status`, `POST /status/dump-refresh` | Health + unresolvable list + minted-key state; the refresh spawns a background dump refresh (202). `status:read` / `status:write`. |
| Admin | `/admin/users` | Multi-user administration. |

### Auth routes

`GET /auth/providers`, `GET /auth/login/{provider}`, `GET /auth/callback/{provider}`, `POST /auth/logout`, `GET /auth/me`. The login flow can carry a `regenerate=true` intent so the Status page's "regenerate Codex key" action forces a fresh mint on callback.

### OAuth 2.1 server (for MCP clients)

`GET /.well-known/oauth-protected-resource`, `GET /.well-known/oauth-authorization-server`, `POST /oauth/register` (dynamic client registration), `GET /oauth/authorize` + `POST /oauth/authorize/consent` (PKCE), `POST /oauth/token`.

### MCP surface (`/mcp` and `mcp stdio`)

One rmcp server behind a Streamable HTTP transport (DNS-rebinding host guard, sessions, bearer-auth layer with per-tool scopes) and a stdio subcommand. Tools authenticate with the same keys/scopes as REST.

**Tools (9):** `get_candidates` (library / acquirable pools), `lookup` (four-leg merge), `search_library`, `list_libraries`, `record_verdict`, `record_feedback`, `get_profile`, `propose_profile_update`, `status`.

**Prompts (2):** `recommend`, `consolidate` (rendered from embedded `.md.j2` templates).

**Resource (1):** the taste profile at `shisho://profile`.

## Authentication & authorization

Three coexisting mechanisms, all resolving to one `AuthContext { key_id, user_id, scopes }`:

- **API keys** - argon2-hashed, carrying granular `resource:action` scopes. The REST middleware derives the required action from the HTTP method (GET -> read, DELETE -> delete, else write) and checks it against the resource guard for the route.
- **Session cookies** - issued after OIDC/OAuth2 login, JWT-signed. A session carries the full scope set.
- **OAuth tokens** - issued by the built-in OAuth 2.1 server for MCP clients.

**Auth modes:** `single_user` (the first provisioned user owns everything; no login UI required) and `multi_user` (OIDC login, admin allowlist by email/group, per-user data). Set via `auth.mode`.

**Scope taxonomy** (resource -> actions):

| Resource | Actions | Used by |
| --- | --- | --- |
| `keys` | `manage` | Key management routes. |
| `candidates` | `read` | `get_candidates`. |
| `lookup` | `read` | `lookup`. |
| `library` | `read` | `search_library`, `list_libraries`, library snapshot. |
| `verdicts` | `read`, `write` | Verdicts page + `record_verdict` / `record_feedback`. |
| `profile` | `read`, `write` | Taste-profile page + `get_profile` / `propose_profile_update`. |
| `status` | `read`, `write` | Status page + `status` tool; `write` gates the dump refresh. |

(An `example:*` resource exists as scaffolding for the sample endpoints and is not part of the product surface.)

### The Codex minted-key lifecycle

At sign-in Shisho mints a dedicated **least-privilege Codex API key** for the user by forwarding the one-shot IdP token, encrypts it with the instance data key, and stores it on the user row (`codex_key_ciphertext` + `codex_key_version` + `codex_user_sub`). Per request, `codex_identity.resolve` decrypts the key and validates it against Codex `/me` (behind a short-TTL validation cache), yielding a `UserCtx { user_sub, credential }`. Per-user data is keyed on `codex_user_sub`, not the local user id. A revoked or expired key surfaces a distinct "re-authenticate at the Shisho UI" error rather than a silent failure; the Status page offers a regenerate action that re-runs the OIDC flow to force a fresh mint.

## Core domain subsystems

### Library snapshot and read-state (per-user)

Built with the caller's minted key from their paginated Codex series list, folded into a per-user snapshot: the normalized owned `(provider, id)` set, a read-state map (`unread == total` -> untouched, `unread == 0` -> completed, else in-progress), ratings presence, Codex UUIDs, and the list of unresolvable series (external IDs that map to no known provider). Cached stale-while-revalidate keyed on `user_sub` (roughly 15-minute revalidate). A credential-less `peek` path serves the web Status page without originating a Codex call.

### Candidate pools

`get_candidates` answers two pools, both anti-joined against the per-user snapshot so ownership is exact:

- **library** - series the user owns but has not started (untouched backlog), enriched via Tsundoku/MangaBaka, with non-retracted verdicts subtracted.
- **acquirable** - series worth buying, excluding everything the caller already owns (anti-joined against the Codex-derived owned set, not Tsundoku's overlay).

### Four-leg lookup

`lookup` merges, in parallel, Tsundoku ∥ Codex ∥ MangaBaka ∥ Shisho's own verdict history into one response, with a MangaBaka foreign-ID pivot and a dump full-text-search title fallback. The backbone answers even when both live backends are cold.

### Verdicts (append-only)

Every recommendation, tier, rationale, retraction, and reading outcome is recorded in the append-only `verdict` log. A retraction is a follow-up row (`retracted: true`, reason `failedVerification` or `withdrawn`), never an update or delete. Reading outcomes are recorded by appending a `feedback`-session row (the mechanism `record_feedback` and the web outcome-correction both use); effective per-series state is computed by folding all rows for that series. Verified-and-rejected candidates are not re-litigated.

### Taste profile and consolidation

The taste profile is a per-user Markdown document (what the user loves, likes, dislikes, is allergic to). The `consolidate` prompt proposes updates as diffs into `profile_version` rows with `pending` status; the user approves or rejects them out-of-band (in the web console or via the tool), transitioning to `applied`/`rejected`. Direct edits create a new applied version.

### Reconciliation and sweeps

A verdict with no recorded outcome whose series now shows read progress in Codex raises a `reconciliation_flag` ("you started X after it was recommended, how was it?"). The reconcile sweep resolves settled flags (snapshot-independent) and, over warm snapshots, detects new ones. The unresolvable sweep counts and lists, per user, Codex series whose external IDs are unmappable, always with a deep link to Codex's metadata-match flow. Sweeps never patch mappings locally; fixes happen at the source (Codex, or Tsundoku's review queue).

### MangaBaka backbone

The metadata provider downloads a large offline dump, validates it, and atomically promotes it into place (so `serve` and `mangabaka refresh` never write the live file concurrently), exposing full-text title search and foreign-ID lookups. When the dump is missing it degrades to a live API fallback and reports `degraded` in health so a fresh deploy knows to ingest.

## Background tasks and scheduling

On `serve`, when the recommendation service is configured, `start_background_jobs` runs on `tokio-cron-scheduler`:

| Job | Schedule source | Behavior |
| --- | --- | --- |
| Reconcile sweep | `sweeps.reconcile_cron` | Resolve/flag reconciliation questions. |
| Unresolvable sweep | `sweeps.unresolvable_cron` | Scan snapshots for unmappable series. |
| MangaBaka refresh | `mangabaka.offline_refresh_cron` | Weekly dump refresh. |
| Tsundoku feed poller | `cache.tsundoku_ttl` interval | Drive cache invalidation from the change feed. |
| Boot-time dump refresh | on startup | Spawned when the dump is missing or older than `mangabaka.dump_max_age()`, so serving starts immediately while the dump lands. |

A job that fails is logged and retried on the next tick; a scheduler that fails to start is logged and skipped, never aborting `serve`.

### Weekly report (the proactive loop)

A scheduled, non-interactive `claude -p` session connects to `/mcp`, runs the recommendation workflow, and writes a Markdown report. The session prompt is an embedded Jinja template rendered at runtime by `shisho report prompt` (no prompt copy lives in the shell script). The runner (`scripts/weekly-report/run-weekly-report.sh`, plus systemd service/timer and a cron example) fails loud on a revoked key both at preflight (an MCP `initialize` 401) and mid-session, so a scheduler notification fires instead of a silently skipped week. The credential lives in the cron environment, never in Shisho.

## Configuration

Layered via Figment: a TOML or YAML file plus environment overrides with the `SHISHO_` prefix and `__` as the nesting separator (e.g. `SHISHO_SERVER__PORT=9000`). Example config in `config/shisho.example.toml`.

| Section | Notable settings |
| --- | --- |
| `server` | `host`, `port`. |
| `database` | SQLite path; MangaBaka dump directory. |
| `auth` | `mode` (`single_user` / `multi_user`), `api_key`, session policy, OIDC providers, admin allowlist. |
| `logging` | tracing level/format. |
| `api` | REST-surface toggles. |
| `app` | instance name/branding. |
| `mcp` | `enabled` (mounts `/mcp`), transport host guard. |
| `encryption` | `key_version`, `key_vN` KEKs (enables SQLCipher at-rest encryption). |
| `codex` | `base_url` (its presence gates the whole recommendation service), admin bearer. |
| `tsundoku` | `base_url`, admin bearer (empty = Tier 0, Codex-only). |
| `mangabaka` | `offline_dump_url`, `offline_refresh_cron`, `offline_max_age_days`, `api_fallback`. |
| `cache` | `tsundoku_ttl_secs` (also the feed-poll interval). |
| `sweeps` | `reconcile_cron`, `unresolvable_cron`. |

The `SHISHO_API_KEY` env var supplies the bootstrap admin key. When neither an api_key nor providers are configured, `serve` enables a first-run setup wizard with a printed setup token.

## Data model (SQLite only)

sea-orm entities over SQLite (WAL + foreign keys), with optional at-rest encryption via SQLCipher. Everything user-scoped keys on `user_sub`; caches are in-memory and cost only a warm-up on restart.

| Entity / table | Purpose |
| --- | --- |
| `users` | Provisioned users; holds the encrypted minted Codex credential (`codex_key_ciphertext`, `codex_key_version`, `codex_user_sub`). |
| `api_keys` | Hashed API keys with scopes. |
| `oauth_clients` | Dynamically registered OAuth 2.1 clients. |
| `oauth_auth_codes` | PKCE authorization codes. |
| `oauth_tokens` | Issued access/refresh tokens. |
| `verdict` | Append-only recommendation log (tier, rationale, verified, retracted + reason, outcome, session id), keyed on `user_sub` + series identity. |
| `profile` | Current taste profile per user (Markdown, versioned). |
| `profile_version` | Full history of profile changes with diffs and status (pending -> approved/rejected). |
| `reconciliation_flag` | A user's open "did you finish X?" questions. |
| `feed_cursor` | The Tsundoku change-feed cursor (the sole persisted cross-system state). |

Migrations (`migration/src/`): users + api_keys, OAuth tables, then verdicts, profiles, profile_versions, reconciliation_flags, feed_cursor, and the users-codex-credential column.

## Deployment

- **Binary**: one `shisho` executable; embed the built SPA with the `embed-frontend` feature (`make frontend-build` first).
- **Docker**: `Dockerfile`, `Dockerfile.cross` (multi-arch amd64/arm64, used by CI), `Dockerfile.dev`; `docker-compose.yml` and `compose.watch.yml`.
- **CI** (Gitea Actions): three jobs on push to `main`: **Test** (`cargo test`), **Lint** (`clippy` + Biome), and **Docker** (multi-arch build + push, which runs the production frontend build `tsc -b && vite build` inside `Dockerfile.cross`). The frontend production build is stricter than a bare `tsc --noEmit`, so run `npm run build` (or `make check`) locally before pushing.
- **Scaling**: single instance. SQLite and the in-memory SWR caches make horizontal scale-out a non-goal; a restart costs a cache warm-up, nothing more.

## Tech stack summary

- **Language/runtime**: Rust edition 2024, tokio.
- **HTTP**: axum 0.8, utoipa + Scalar for OpenAPI, tower middleware.
- **MCP**: rmcp 2 (Streamable HTTP + stdio), schemars, minijinja + rust-embed prompts.
- **DB**: sea-orm 1 over SQLite (WAL, FKs), SQLCipher via libsqlite3-sys; migrations via sea-orm-migration.
- **Auth**: argon2 API keys, openidconnect/oauth2 login, jsonwebtoken sessions, a built-in OAuth 2.1 server.
- **Crypto**: aes-gcm + hkdf + zeroize envelope keys.
- **Config**: Figment (TOML/YAML + `SHISHO_` env), clap CLI.
- **Scheduling**: tokio-cron-scheduler.
- **Frontend**: React 19 + Vite + TypeScript + Mantine + Zustand + TanStack Query/Router; openapi-fetch/typescript typed client; vitest + MSW; Biome.
- **Testing**: Rust unit + in-memory-SQLite DB tests + HTTP-replay fixtures under `tests/fixtures/`; an end-to-end MCP recommend-flow test against a wiremock Codex.

## Current status

The implementation plan (a private spec repo nested at `.shisho/`, gitignored by this repo) tracked nine phases. All nine are complete as of this commit.

| Phase | Area | Status |
| --- | --- | --- |
| 1 | Foundations (shell, config, DB, crypto) | Complete |
| 2 | Backend clients (Codex, Tsundoku) | Complete |
| 3 | MangaBaka backbone (dump ingest, FTS, API fallback) | Complete |
| 4 | Domain DB (verdicts, profiles, reconciliation, feed cursor) | Complete |
| 5 | Codex key mint and per-request identity | Complete |
| 6 | Services (snapshot, pools, lookup, verdict fold, sweeps, health) | Complete |
| 7 | MCP surface (9 tools, 2 prompts, profile resource, minijinja) | Complete |
| 8 | Web UI (taste profile, verdicts, status pages) | Complete |
| 9 | Sweeps/CLI/report (scheduler, `sweep` CLI, weekly report) | Complete |

**Known deliberate deferrals** (documented as such in the code and specs, not shipped): the `record_feedback` Codex rating push-back stays disabled; "recent auth failures" is not surfaced on Status (no counter source in the current shell); live-backend `recommend` validation is a manual step, since CI runs on fixtures only. A possible future embedding recall step (`model2vec-rs` over `fastembed-rs`) is recorded as an enhancement, gated behind actual need, and not built.

## Changelog

- 2026-07-06: Initial overview generated from `main` @ `1602453`.
