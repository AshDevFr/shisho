import { MantineProvider } from "@mantine/core";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import type { ReactNode } from "react";
import { describe, expect, it } from "vitest";
import { server } from "@/mocks/server";
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
        createdAt: "2026-07-01T00:00:00Z",
      },
    ],
  },
];

describe("VerdictsPage", () => {
  it("renders the session-grouped log with its rows", async () => {
    server.use(
      http.get("/api/v1/verdicts", () => HttpResponse.json(oneSession)),
    );
    renderWithProviders(<VerdictsPage />);

    await waitFor(() =>
      expect(screen.getByText("fits the profile")).toBeInTheDocument(),
    );
    expect(screen.getByText("session-a")).toBeInTheDocument();
    expect(screen.getByText("S")).toBeInTheDocument();
    // The title leads; the raw id is still shown as a secondary line.
    expect(
      screen.getByText("Frieren: Beyond Journey's End"),
    ).toBeInTheDocument();
    expect(screen.getByText("mangabaka:1")).toBeInTheDocument();
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
