import { HttpResponse, http } from "msw";
import type { components } from "@/types/api.generated";

// Invented demo data for `npm run dev:mock`: a believable signed-in admin with
// a few recommendation sessions, a taste profile with a pending proposal, and
// a status page with one degraded backend. Browser-only; the vitest server
// keeps the minimal defaults in `handlers.ts` so tests stay explicit.

type Verdict = components["schemas"]["VerdictDto"];
type SessionGroup = components["schemas"]["SessionGroup"];

const DAY = 86_400_000;
const ago = (days: number) => new Date(Date.now() - days * DAY).toISOString();

const PALETTE = ["#5b6ee1", "#d9574a", "#3e9e6d", "#c48a2c", "#8e5bd0"];

// A flat SVG "cover" so the demo needs no network access.
function cover(title: string, i: number): string {
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="240" height="340"><rect width="240" height="340" fill="${PALETTE[i % PALETTE.length]}"/><text x="20" y="300" font-family="sans-serif" font-size="20" fill="white">${title.slice(0, 18)}</text></svg>`;
  return `data:image/svg+xml;utf8,${encodeURIComponent(svg)}`;
}

let nextId = 1;
function verdict(
  title: string,
  tier: string,
  days: number,
  extra: Partial<Verdict> = {},
): Verdict {
  const id = nextId++;
  return {
    id,
    seriesId: `mangabaka:${1000 + title.length * 7 + id}`,
    tier,
    rationale: `${title} matches the slow-burn character work in your top-rated series, with a tighter plot than most of what you dropped.`,
    verified: true,
    retracted: false,
    pending: false,
    createdAt: ago(days),
    title,
    coverUrl: cover(title, id),
    summary: `${title}: an invented series used only for the demo. A long synopsis would go here, running a few sentences so the clamp and tooltip get exercised.`,
    mangabakaUrl: "https://mangabaka.dev/",
    tsundokuUrl: "https://example.invalid/tsundoku",
    ...extra,
  };
}

const sessions: SessionGroup[] = [
  {
    sessionId: "rec-2026-10-20",
    startedAt: ago(5),
    verdicts: [
      verdict("The Lighthouse Cartographer", "S", 5),
      verdict("Salt and Iron", "A", 5, {
        codexUrl: "https://example.invalid/codex",
      }),
      verdict("Paper Moon Garrison", "B", 5, { verified: false }),
      verdict("Harbor of Quiet Engines", "pass", 5, {
        rationale:
          "Heavy isekai framing, which your profile lists as a dislike.",
      }),
    ],
  },
  {
    sessionId: "rec-2026-10-06",
    startedAt: ago(19),
    verdicts: [
      verdict("Salt and Iron", "B", 19),
      verdict("Winter Orchard Ledger", "A", 19, {
        codexUrl: "https://example.invalid/codex",
      }),
      verdict("Nine Bells for Odette", "S", 19, {
        retracted: true,
        retractionReason: "failedVerification",
      }),
      verdict("Fox Lantern Courier", "C", 19),
    ],
  },
  {
    sessionId: "rec-2026-09-14",
    startedAt: ago(41),
    verdicts: [
      verdict("Glasswing Academy", "A", 41),
      verdict("Glasswing Academy", "feedback", 12, {
        outcome: "loved",
        notes: "Finished in a weekend.",
        rationale: "",
      }),
      verdict("Second Shift Alchemist", "B", 41),
      verdict("Second Shift Alchemist", "feedback", 3, {
        outcome: "dnf",
        notes: "Inferred: no read progress for 90 days.",
        rationale: "",
      }),
    ],
  },
];

const PROFILE = `# Taste profile

## Likes
- Character-driven drama with a slow build
- Found family, workplace stories, craft and expertise
- Art with strong backgrounds

## Dislikes
- Isekai framing, harem setups
- Long tournament arcs

## Notes
- Finishes short series (under 10 volumes) far more often than long ones
`;

// Home's view of the log above: what still stands, per series. Outcomes
// recorded in the demo hide an entry until it is reopened.
const settled = new Map<number, string>();
function standing(title: string): Verdict {
  const all = sessions.flatMap((s) => s.verdicts);
  const found = all.find((v) => v.title === title && v.outcome == null);
  if (!found) throw new Error(`demo: no verdict for ${title}`);
  return found;
}
// The server's per-series fold, by title (demo titles are unique per series).
function seriesView(v: Verdict) {
  const rows = sessions
    .flatMap((s) => s.verdicts)
    .filter((r) => r.title === v.title)
    .sort((a, b) => a.createdAt.localeCompare(b.createdAt));
  let outcome: string | undefined;
  let lastTier: string | undefined;
  let retracted = false;
  for (const r of rows) {
    if (r.outcome) {
      outcome = r.outcome === "reopened" ? undefined : r.outcome;
      continue;
    }
    retracted = r.retracted;
    if (!r.retracted) lastTier = r.tier;
  }
  // Outcomes recorded in the demo are the newest rows of their series.
  outcome = rows.map((r) => settled.get(r.id)).find(Boolean) ?? outcome;
  const pending =
    outcome == null && !retracted && lastTier != null && lastTier !== "pass";
  return { seriesOutcome: outcome, pending };
}

function recommendations() {
  const entry = (title: string, extra: object = {}) => ({
    verdict: standing(title),
    timesRecommended: 1,
    inLibrary: false,
    ...extra,
  });
  const keep = (e: { verdict: Verdict }) => !settled.has(e.verdict.id);
  return {
    libraryAsOf: ago(0.002),
    recommended: [
      entry("The Lighthouse Cartographer"),
      entry("Salt and Iron", { inLibrary: true, timesRecommended: 2 }),
      entry("Winter Orchard Ledger", { inLibrary: true }),
      entry("Paper Moon Garrison"),
    ].filter(keep),
    reading: [
      entry("Fox Lantern Courier", {
        inLibrary: true,
        booksRead: 3,
        bookCount: 9,
      }),
    ].filter(keep),
  };
}

export const demoHandlers = [
  http.get("/api/v1/recommendations", () =>
    HttpResponse.json(recommendations()),
  ),
  http.post("/api/v1/recommendations/sync", () =>
    HttpResponse.json({
      libraryAsOf: ago(0),
      outcomesInferred: 0,
      questionsRaised: 0,
      questionsResolved: 0,
      retuned: false,
    }),
  ),
  http.get("/api/v1/info", () =>
    HttpResponse.json({
      name: "shisho",
      version: "0.1.2",
      auth_mode: "multi_user",
    }),
  ),
  http.get("/auth/me", () =>
    HttpResponse.json({
      id: "user-demo",
      email: "reader@example.com",
      displayName: "Demo Reader",
      avatarUrl: null,
      isAdmin: true,
    }),
  ),
  http.get("/auth/providers", () =>
    HttpResponse.json([
      { name: "oidc", kind: "oidc", label: "Sign in with SSO" },
    ]),
  ),
  http.get("/api/v1/whoami", () =>
    HttpResponse.json({ userId: "user-demo", scopes: [] }),
  ),
  http.get("/api/v1/verdicts", ({ request }) => {
    const q = new URL(request.url).searchParams;
    const viewed = sessions.map((s) => ({
      ...s,
      verdicts: s.verdicts.map((v) => ({ ...v, ...seriesView(v) })),
    }));
    const keep = (v: Verdict) =>
      (!q.get("tier") || v.tier === q.get("tier")) &&
      (!q.get("outcome") || v.seriesOutcome === q.get("outcome")) &&
      (!q.has("verified") || String(v.verified) === q.get("verified")) &&
      (!q.has("retracted") || String(v.retracted) === q.get("retracted")) &&
      (!q.get("state") || (q.get("state") === "pending") === v.pending) &&
      (q.get("feedback") !== "false" || v.outcome == null);
    return HttpResponse.json(
      viewed
        .map((s) => ({ ...s, verdicts: s.verdicts.filter(keep) }))
        .filter((s) => s.verdicts.length > 0),
    );
  }),
  http.post("/api/v1/verdicts/:id/outcome", async ({ request, params }) => {
    const body = (await request.json()) as { outcome: string; notes?: string };
    if (body.outcome === "reopened") settled.delete(Number(params.id));
    else settled.set(Number(params.id), body.outcome);
    const source = sessions
      .flatMap((s) => s.verdicts)
      .find((v) => v.id === Number(params.id));
    return HttpResponse.json(
      verdict(source?.title ?? "Unknown", "feedback", 0, {
        outcome: body.outcome,
        notes: body.notes,
        rationale: "",
      }),
      { status: 201 },
    );
  }),
  http.get("/api/v1/profile", () =>
    HttpResponse.json({
      version: 4,
      content: PROFILE,
      updatedAt: ago(9),
      pendingProposal: {
        version: 5,
        baseVersion: 4,
        changeSummary:
          "Add a note on dropping series with weekly-chapter pacing",
        content: `${PROFILE}- Drops series that drag in weekly-chapter pacing\n`,
        diff: "@@ -14,3 +14,4 @@\n ## Notes\n - Finishes short series (under 10 volumes) far more often than long ones\n+- Drops series that drag in weekly-chapter pacing\n",
      },
    }),
  ),
  http.get("/api/v1/profile/versions", () =>
    HttpResponse.json(
      [4, 3, 2, 1].map((v) => ({
        version: v,
        baseVersion: v - 1,
        status: "applied",
        changeSummary: v === 1 ? "Initial profile" : `Consolidation ${v - 1}`,
        createdAt: ago(9 + (4 - v) * 14),
        diff: "@@ -1 +1 @@\n-old line\n+new line\n",
      })),
    ),
  ),
  http.get("/api/v1/status", () =>
    HttpResponse.json({
      mintedKey: { present: true, codexUserSub: "codex-demo" },
      health: {
        status: "degraded",
        db: { reachable: true },
        codex: { configured: true, reachable: true, required: true },
        tsundoku: {
          configured: true,
          reachable: false,
          required: false,
          error: "connection refused",
        },
        mangabaka: {
          apiFallback: true,
          dumpLoaded: true,
          dumpModifiedAt: ago(6),
          lastRefreshAt: ago(1),
          negativeCacheSize: 12,
          refreshRunning: false,
          seriesRows: 123456,
        },
        caches: {
          librarySnapshotLastWarmedAt: ago(0.01),
          tsundokuLastWarmedAt: ago(0.3),
        },
      },
      library: {
        asOf: ago(0.01),
        seriesCount: 214,
        unresolvableCount: 2,
        unresolvable: [
          {
            seriesId: "codex:7f3a",
            title: "Untitled Anthology Vol. 2",
            rawSources: [],
            codexUrl: "https://example.invalid/codex",
          },
          {
            seriesId: "codex:91bc",
            title: "Weekly Shorts Collection",
            rawSources: ["custom:abc"],
            codexUrl: "https://example.invalid/codex",
          },
        ],
      },
    }),
  ),
  http.get("/api/v1/keys", () =>
    HttpResponse.json([
      {
        id: "key-1",
        name: "laptop claude code",
        scopes: ["candidates:read", "verdicts:read", "verdicts:write"],
        createdAt: ago(30),
        lastUsed: ago(1),
      },
    ]),
  ),
  http.get("/api/v1/connections", () =>
    HttpResponse.json([
      {
        clientId: "claude-ai",
        clientName: "Claude",
        connectedAt: ago(20),
        expiresAt: ago(-10),
        scopes: ["candidates:read", "verdicts:write", "profile:write"],
      },
    ]),
  ),
  http.get("/api/v1/admin/users", () =>
    HttpResponse.json([
      {
        id: "user-demo",
        displayName: "Demo Reader",
        email: "reader@example.com",
        isAdmin: true,
        provider: "oidc",
        lastSeen: ago(0),
      },
      {
        id: "user-2",
        displayName: "Second Reader",
        email: "second@example.com",
        isAdmin: false,
        provider: "oidc",
        lastSeen: ago(12),
      },
    ]),
  ),
  http.get("/api/v1/admin/encryption", () =>
    HttpResponse.json({ enabled: true, kekVersion: 2, lastRotatedAt: ago(60) }),
  ),
];
