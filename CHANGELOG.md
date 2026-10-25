# Changelog

All notable changes to shisho will be documented in this file.

## [0.1.3] - 2026-10-25

### Features

- *(api)* Serve current recommendations re-evaluated against the log and library
- *(web)* Make Home the read-next list of current recommendations
- *(web)* Turn Verdicts into History with one aligned table, rename Profile to Account

### Bug Fixes

- *(web)* Make the app usable on phones and show readable Status times

## [0.1.2] - 2026-10-25

### Features

- *(sweeps)* Run the full reconcile for every user on the schedule
- *(mcp)* Add a reconcile tool that syncs feedback from Codex on demand

## [0.1.1] - 2026-10-24

### Features

- *(reconciliation)* Record outcomes from ratings set in Codex
- *(eval)* Save eval fixtures de-identified
- *(reconciliation)* Record stalled reads of recommendations as DNF
- *(tuning)* Choose ranking parameters per user from their own library

### Miscellaneous Tasks

- Credit the project to a public handle

## [0.1.0] - 2026-10-24

### Features

- Add shisho-ids crate for external-ID normalization
- Add shisho-cache SWR primitive
- *(config)* Add Codex/Tsundoku/MangaBaka/cache/sweeps config sections
- *(codex)* Add typed Codex client with least-privilege key minting
- *(tsundoku)* Add typed Tsundoku client for the acquirable pool
- *(mangabaka)* Add the metadata backbone (offline dump + API fallback)
- *(cli)* Add `shisho mangabaka refresh` subcommand
- *(db)* Add the per-user judgment schema entities
- *(db)* Re-author the domain schema migrations
- *(db)* Add the domain repositories and their DB tests
- *(db)* Store the minted Codex credential on the user row
- *(auth)* Mint a least-privilege Codex key at sign-in
- *(auth)* Resolve per-request Codex identity with a /me validation cache
- *(auth)* Regenerate the Codex key and mint only when needed
- *(services)* Add the per-user library snapshot and read-state fold
- *(services)* Cache Tsundoku queries with feed-cursor invalidation
- *(services)* Add the MangaBaka-backbone metadata resolver
- *(services)* Re-export the verdict effective-state fold
- *(services)* Add candidate pools, lookup, library search, and sweeps
- *(mcp)* Render prompts from embedded minijinja templates
- *(api)* Construct the domain services and hold them in AppState
- *(auth)* Add the MCP domain tool scopes
- *(mcp)* Carry the caller's full identity into the stdio service
- *(mcp)* Mount the nine domain recommendation tools
- *(mcp)* Serve the recommend/consolidate prompts and the taste-profile resource
- *(api)* Add the taste-profile REST endpoints
- *(web)* Add the taste-profile page
- *(api)* Add the verdict-log endpoints
- *(web)* Add the verdicts page
- *(api)* Add the status endpoints and share the health probe
- *(web)* Add the status page with an OIDC-reauth key regenerate
- *(serve)* Register the background jobs on the running server
- *(cli)* Add the sweep subcommands
- *(report)* Add the weekly-report prompt and headless runner
- *(mcp)* Annotate tools as read-only or write for client auto-approval
- *(web)* Richer verdicts page with titles, links, and a card/table view
- *(web)* Two-up verdict cards with bigger covers and inline summary
- *(library)* Keep the caller's Codex rating values and sort by them
- *(eval)* Replay a user's favourites to measure backlog rankers
- *(candidates)* Rank the library pool by the caller's taste
- *(candidates)* Gate pornographic series and surface content rating
- *(prompts)* Verify ranked finalists and run one pool per session
- *(mangabaka)* Index dump genres, tags and franchise links
- *(candidates)* Add a discover pool drawn from the MangaBaka dump
- *(candidates)* Rank franchise entries by what the caller has read
- *(candidates)* Re-rank the acquirable pool for each caller
- *(affinity)* Weight rare tags higher and credit shared authors
- *(affinity)* Learn from recorded reading outcomes
- *(profile)* Enforce a never-list declared in the taste profile

### Bug Fixes

- *(web)* Null-safe the nullable status/profile fields for the production build
- *(oauth)* Grant MCP connectors the real tool scopes, not the example scaffold
- *(connections)* Show the refresh-token expiry, not the 1-hour access expiry
- *(verdicts)* Resolve Codex links from the external-index, not the cold snapshot
- *(web)* Make the verdict tier badge legible in dark mode
- *(web)* Give the verdict summary the same clamp-and-tooltip as the reason
- *(mangabaka)* Migrate default host to api.mangabaka.org
- *(db)* Drop redundant borrow in the sqlcipher key pragma
- *(verdicts)* One feedback write path for the web UI and MCP
- *(mcp)* Show the underlying cause in tool error messages
- *(codex)* Treat a 204 rating response as no rating
- *(lookup)* Keep ownership when the live rating fetch fails
- *(candidates)* Keep read and rated series out of the library pool
- *(candidates)* Stop a candidate citing itself as a reason
- *(mangabaka)* Add side tables to a dump already in service
- *(library)* Report the full match count from search_library
- *(web)* Pin TypeScript 6 so API type generation runs again
- *(build)* Generate the changelog on a project's first release

### Refactor

- *(auth)* Drop the read/write scope aliases, keep only admin
- *(auth)* Remove the placeholder example scope resource
- *(codex)* Migrate off the deprecated full=true query parameter
- *(services)* One batched Tsundoku resolver and shared id keys

### Documentation

- Add generated project overview
- Move project overview into the spec repo (.shisho/docs)

### Performance

- *(codex)* Cache the external-index as a reusable local library index
- *(candidates)* Anti-join the acquirable pool against the cached Codex index
- *(candidates)* Fetch candidate details concurrently in both pools
- *(candidates)* Ship compact candidates instead of full metadata
- *(affinity)* Tune the taste fold against a real library

### Testing

- *(codex)* Replay mint and libraries from recorded fixtures
- *(mcp)* Add an end-to-end recommend-flow test over a wiremock Codex

### Miscellaneous Tasks

- Initial scaffold
- Bump lint cache key prefix to escape a poisoned Actions cache
- Give cargo caches a weekly TTL via a rotating key bucket
- Re-trigger run (previous run killed by a transient network outage)

