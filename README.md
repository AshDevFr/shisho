# shisho

A self-hosted library-aware manga recommendation service, exposed as a single remote MCP server

An MCP server shell: a REST API + web UI with API-key / OIDC-OAuth2 login and
admin groups, an OAuth 2.1 authorization server so MCP clients can connect, an
[MCP](https://modelcontextprotocol.io) server over Streamable HTTP and stdio,
and SQLCipher at-rest encryption — everything but the domain. The shipped
`example` resource and `ping`/`echo` MCP tools are scaffolds to replace.

## Quick start

```bash
# Backend (REST at /api/v1, Scalar docs at /docs, MCP at /mcp). On first run it
# prints a one-time admin API key and a /setup?token=... link.
cargo run -- serve

# Frontend dev server (proxies /api to the backend, HMR)
cd web && npm install && npm run dev
```

Other subcommands: `shisho migrate`, `shisho openapi`, `shisho encryption {gen,rotate-kek,status}`
(at-rest encryption keys), and `shisho mcp stdio` (serve MCP to a local client over stdio).

Then open the Vite dev server (http://localhost:5173). For a production-style
single binary with the SPA embedded:

```bash
make build            # builds web/dist, then `cargo build --release --features embed-frontend`
./target/release/shisho serve
```

## Codex API key permissions

Shisho requires **Codex 2.9.0 or later**, the first release that gates rating
writes behind their own permission. An older Codex shows as degraded on the
Status page.

Shisho reads each user's library through a Codex API key. At sign-in it mints
a dedicated key itself; a key you create by hand (to replace a minted one)
needs the same permissions:

| Permission | Used for |
|---|---|
| `series:read` | the library snapshot: series, unread counts, external ids, ratings, the want-to-read list |
| `libraries:read` | listing and scoping libraries |
| `books:read` | per-book read progress of recommended series, to notice a stalled read |
| `progress:read` | the same progress data; held so stall checks keep working if Codex gates it separately |
| `ratings:write` | writing a rating and notes back to Codex, only when you ask (`record_feedback` with `pushToCodex`) |
| `want-to-read:write` | adding series to and removing them from your want-to-read queue, only when you ask (`update_want_to_read`) |

These two are the only write permissions Shisho asks for. It never changes
reading progress, queue order, or metadata, never queues a series that is
not already in your Codex library, and never replaces a different rating or
notes already in Codex unless told to overwrite (Codex keeps no rating
history). Grading the queue (`get_candidates` with pool `wantToRead`) only
reads, under `series:read`.

Without `books:read` and `progress:read` everything else works; Shisho only
skips recording stalled reads as abandoned. Without `ratings:write` Shisho
still records your outcome, and reports that the rating did not reach Codex;
without `want-to-read:write`, queue edits are reported as failed.

The key that authorizes the mint (your sign-in, or a pasted key with
`api-keys:write`) must itself hold every permission granted. If it cannot
grant the write permissions, Shisho mints a read-only key instead of
failing sign-in.

A key minted before a permission was added keeps its old set. That includes
every key minted before Shisho asked for `ratings:write` and
`want-to-read:write`: regenerate it from the Codex key panel on the Status
page, or add the permissions to the key in Codex.

Editing the queue from an MCP client also needs the Shisho scope
`library:write`. A claude.ai connector authorized before that scope existed
does not hold it: disconnect and reconnect the connector once.

## Common commands

```bash
make help             # list all targets
make check            # fmt + clippy + tests
make openapi-all      # regenerate the OpenAPI spec and the TypeScript types
make dev-up           # docker compose dev stack (backend hot reload + Vite)
make release-prepare VERSION=1.0.0
```

## Git pre-commit hooks

Uses the [pre-commit](https://pre-commit.com/) framework. Install once, then
it runs `cargo fmt`, `cargo clippy`, frontend lint (biome), and the OpenAPI
sync check on every `git commit`.

```bash
brew install pre-commit   # or: pipx install pre-commit
make setup-hooks          # runs `pre-commit install`
pre-commit run --all-files # optional: run all hooks once across the repo
```

Bypass for a specific commit with `git commit --no-verify` (don't make a habit).

## CI / Build

Two workflows under `.github/workflows/`:

- `ci.yml` runs on every PR: tests (partitioned with cargo-nextest), Rust lint,
  frontend lint / tests / build, and a multi-arch Docker build pushed to GHCR
  with a PR tag.
- `build.yml` runs on push to `main` and on version tags: the same checks, then
  cargo-dist builds platform binaries (with build provenance attestations on public repos; the step skips itself on private repos, where GitHub doesn't support them) and
  pushes multi-arch Docker images. Version tags also create a GitHub Release.

Both publish to `ghcr.io/<owner>/shisho`. Make sure the repository has
"Read and write" workflow permissions enabled under Settings → Actions.

## Configuration

Copy `config/shisho.example.toml` to `config/shisho.toml` and edit.
Any value can be overridden via environment variables using the
`SHISHO_` prefix and `__` for nesting
(e.g. `SHISHO_SERVER__PORT=9000`). YAML config files are also supported.

For docker compose, copy `.env.example` to `.env` (gitignored) to inject
environment overrides into the app containers. The file is optional; compose
runs fine without it. Note that the binary itself does not read `.env`: for
local non-docker runs, export the variables in your shell or via mise/direnv.
