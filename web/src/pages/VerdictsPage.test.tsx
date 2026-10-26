import { MantineProvider } from "@mantine/core";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import type { ReactNode } from "react";
import { beforeEach, describe, expect, it } from "vitest";
import { server } from "@/mocks/server";
import { useUiStore } from "@/stores/ui";
import { VerdictsPage } from "./VerdictsPage";

function renderWithProviders(ui: ReactNode) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <MantineProvider>
      <QueryClientProvider client={client}>{ui}</QueryClientProvider>
    </MantineProvider>,
  );
}

const oneSession = [
  {
    sessionId: "session-a",
    startedAt: "2026-07-01T00:00:00Z",
    verdicts: [
      {
        id: 1,
        seriesId: "mangabaka:1",
        title: "Frieren: Beyond Journey's End",
        summary: "An elf mage journeys after her hero party.",
        coverUrl: "https://covers.test/frieren.jpg",
        mangabakaUrl: "https://mangabaka.org/1",
        codexUrl: "https://codex.test/series/abc",
        tsundokuUrl: "https://tsundoku.test/series/55",
        tier: "S",
        rationale: "fits the profile",
        verified: true,
        retracted: false,
        pending: true,
        createdAt: "2026-07-01T00:00:00Z",
      },
    ],
  },
];

const row = (id: number, title: string, extra = {}) => ({
  id,
  seriesId: `mangabaka:${id}`,
  title,
  tier: "A",
  rationale: `${title} rationale`,
  verified: true,
  retracted: false,
  pending: false,
  createdAt: "2026-07-01T00:00:00Z",
  ...extra,
});

const twoSessions = [
  {
    sessionId: "session-b",
    startedAt: "2026-07-02T00:00:00Z",
    verdicts: [
      row(1, "Second Series", { pending: true }),
      row(3, "Passed Series", { tier: "pass" }),
      row(4, "Retracted Series", {
        retracted: true,
        retractionReason: "failedVerification",
      }),
    ],
  },
  {
    sessionId: "session-a",
    startedAt: "2026-07-01T00:00:00Z",
    verdicts: [
      row(5, "Loved Series", {
        tier: "feedback",
        outcome: "loved",
        rationale: "",
      }),
    ],
  },
];

describe("VerdictsPage", () => {
  beforeEach(() => {
    useUiStore.setState({ verdictsView: "card" });
  });

  it("renders the session-grouped log with its rows", async () => {
    server.use(
      http.get("/api/v1/verdicts", () => HttpResponse.json(oneSession)),
    );
    renderWithProviders(<VerdictsPage />);

    await waitFor(() =>
      expect(screen.getByText("fits the profile")).toBeInTheDocument(),
    );
    expect(screen.getByText("session-a")).toBeInTheDocument();
    // The tier badge (the Tier filter lists "S" as an option too).
    expect(
      screen.getAllByText("S").some((el) => el.closest(".mantine-Badge-root")),
    ).toBe(true);
    // The title leads; the raw id is still shown as a secondary line.
    expect(
      screen.getByText("Frieren: Beyond Journey's End"),
    ).toBeInTheDocument();
    // The raw id is a hover detail now, not a line under the title.
    expect(screen.queryByText("mangabaka:1")).not.toBeInTheDocument();
    // Deep links to all three sources.
    expect(screen.getByRole("link", { name: /MangaBaka/ })).toHaveAttribute(
      "href",
      "https://mangabaka.org/1",
    );
    expect(screen.getByRole("link", { name: /Codex/ })).toHaveAttribute(
      "href",
      "https://codex.test/series/abc",
    );
    expect(screen.getByRole("link", { name: /Tsundoku/ })).toHaveAttribute(
      "href",
      "https://tsundoku.test/series/55",
    );
  });

  it("records an outcome and refetches", async () => {
    let posted = false;
    server.use(
      http.get("/api/v1/verdicts", () => HttpResponse.json(oneSession)),
      http.post("/api/v1/verdicts/:id/outcome", async () => {
        posted = true;
        return HttpResponse.json(
          {
            id: 2,
            seriesId: "mangabaka:1",
            tier: "feedback",
            rationale: "outcome recorded in the web UI",
            verified: false,
            retracted: false,
            outcome: "loved",
            createdAt: "2026-07-02T00:00:00Z",
          },
          { status: 201 },
        );
      }),
    );
    renderWithProviders(<VerdictsPage />);

    await waitFor(() =>
      expect(screen.getByText("fits the profile")).toBeInTheDocument(),
    );

    // Pick a structured outcome, then Save.
    fireEvent.change(screen.getByLabelText("Set outcome for verdict 1"), {
      target: { value: "loved" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(posted).toBe(true));
  });

  it("lists every session in one table with a session column", async () => {
    useUiStore.setState({ verdictsView: "table" });
    server.use(
      http.get("/api/v1/verdicts", () => HttpResponse.json(twoSessions)),
    );
    renderWithProviders(<VerdictsPage />);

    await waitFor(() =>
      expect(screen.getByText("Second Series")).toBeInTheDocument(),
    );
    expect(screen.getAllByRole("table")).toHaveLength(1);
    expect(
      screen.getByRole("columnheader", { name: "Session" }),
    ).toBeInTheDocument();
    // The session is a column: on every row, not a container per session.
    expect(screen.getAllByText("session-b")).toHaveLength(3);
    expect(screen.getAllByText("session-a")).toHaveLength(1);
  });

  it("opens on pending without feedback rows and switches to settled", async () => {
    const queries: string[] = [];
    server.use(
      http.get("/api/v1/verdicts", ({ request }) => {
        const q = new URL(request.url).searchParams;
        queries.push(q.toString());
        return HttpResponse.json(
          q.get("state") === "settled"
            ? [
                {
                  sessionId: "session-a",
                  startedAt: "2026-07-01T00:00:00Z",
                  verdicts: [
                    row(7, "Finished Series", { seriesOutcome: "loved" }),
                  ],
                },
              ]
            : twoSessions,
        );
      }),
    );
    renderWithProviders(<VerdictsPage />);

    await waitFor(() =>
      expect(queries[0]).toBe("state=pending&feedback=false"),
    );
    fireEvent.click(screen.getByText("Settled"));
    // A settled recommendation shows how its series ended on its own row.
    expect(await screen.findByText("outcome: loved")).toBeInTheDocument();
    expect(queries.at(-1)).toBe("state=settled&feedback=false");

    fireEvent.click(screen.getByLabelText("Show feedback rows"));
    await waitFor(() => expect(queries.at(-1)).toBe("state=settled"));
    fireEvent.click(screen.getByText("All"));
    await waitFor(() => expect(queries.at(-1)).toBe(""));
  });

  it("offers an outcome only on standing recommendations", async () => {
    server.use(
      http.get("/api/v1/verdicts", () => HttpResponse.json(twoSessions)),
    );
    renderWithProviders(<VerdictsPage />);

    await waitFor(() =>
      expect(screen.getByText("Second Series")).toBeInTheDocument(),
    );
    // Standing: id 1. Pass (3), retracted (4) and feedback (5): none.
    expect(
      screen.getByLabelText("Set outcome for verdict 1"),
    ).toBeInTheDocument();
    for (const id of [3, 4, 5]) {
      expect(
        screen.queryByLabelText(`Set outcome for verdict ${id}`),
      ).not.toBeInTheDocument();
    }
  });

  it("shows an actionable message when the caller has no Codex identity", async () => {
    server.use(
      http.get("/api/v1/verdicts", () =>
        HttpResponse.json(
          {
            error: {
              message:
                "no Codex identity yet: sign in to mint a Codex key before viewing your verdict log",
            },
          },
          { status: 409 },
        ),
      ),
    );
    renderWithProviders(<VerdictsPage />);

    await waitFor(() =>
      expect(screen.getByText(/no Codex identity yet/)).toBeInTheDocument(),
    );
  });
});
