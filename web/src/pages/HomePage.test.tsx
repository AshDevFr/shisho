import { MantineProvider } from "@mantine/core";
import { Notifications } from "@mantine/notifications";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import type { ReactNode } from "react";
import { beforeEach, describe, expect, it } from "vitest";
import type { CurrentRecommendations } from "@/api/recommendations";
import { server } from "@/mocks/server";
import { useUiStore } from "@/stores/ui";
import { HomePage } from "./HomePage";

function renderWithProviders(ui: ReactNode) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <MantineProvider>
      <Notifications />
      <QueryClientProvider client={client}>{ui}</QueryClientProvider>
    </MantineProvider>,
  );
}

function rec(id: number, title: string, tier: string, extra = {}) {
  return {
    verdict: {
      id,
      seriesId: `mangabaka:${id}`,
      title,
      tier,
      rationale: `${title} fits the profile`,
      verified: true,
      retracted: false,
      pending: true,
      createdAt: "2026-10-20T00:00:00Z",
    },
    timesRecommended: 1,
    inLibrary: false,
    ...extra,
  };
}

const twoAndOneReading: CurrentRecommendations = {
  libraryAsOf: "2026-10-25T00:00:00Z",
  recommended: [
    rec(1, "The Lighthouse Cartographer", "S"),
    rec(2, "Salt and Iron", "A", { inLibrary: true, timesRecommended: 2 }),
  ],
  reading: [
    rec(3, "Fox Lantern Courier", "B", {
      inLibrary: true,
      booksRead: 3,
      bookCount: 9,
    }),
  ],
};

function serve(body: CurrentRecommendations) {
  server.use(
    http.get("/api/v1/recommendations", () => HttpResponse.json(body)),
  );
}

/// Capture outcome posts as `[verdictId, outcome]`.
function captureOutcomes(): [number, string][] {
  const posted: [number, string][] = [];
  server.use(
    http.post("/api/v1/verdicts/:id/outcome", async ({ request, params }) => {
      const body = (await request.json()) as { outcome: string };
      posted.push([Number(params.id), body.outcome]);
      return HttpResponse.json(
        {
          id: 99,
          tier: "feedback",
          rationale: "",
          verified: false,
          retracted: false,
          createdAt: "2026-10-25T00:00:00Z",
          outcome: body.outcome,
        },
        { status: 201 },
      );
    }),
  );
  return posted;
}

describe("HomePage", () => {
  beforeEach(() => {
    useUiStore.setState({ homeView: "card" });
  });

  it("splits recommended series from the ones being read", async () => {
    serve(twoAndOneReading);
    renderWithProviders(<HomePage />);

    await waitFor(() =>
      expect(
        screen.getByText("The Lighthouse Cartographer"),
      ).toBeInTheDocument(),
    );
    expect(screen.getByText("Salt and Iron")).toBeInTheDocument();
    expect(
      screen.getByText("2 recommendations", { exact: false }),
    ).toBeInTheDocument();
    expect(screen.getByText("in library")).toBeInTheDocument();
    expect(screen.getByText("recommended 2×")).toBeInTheDocument();

    expect(screen.getByText("Reading now")).toBeInTheDocument();
    expect(screen.getByText("Fox Lantern Courier")).toBeInTheDocument();
    expect(screen.getByText("3 of 9 read")).toBeInTheDocument();
  });

  it("records an outcome in one click and undoes it with reopened", async () => {
    serve(twoAndOneReading);
    const posted = captureOutcomes();
    renderWithProviders(<HomePage />);

    fireEvent.click(
      await screen.findByLabelText(
        "Record an outcome for The Lighthouse Cartographer",
      ),
    );
    fireEvent.click(await screen.findByText("Loved"));

    await waitFor(() => expect(posted).toEqual([[1, "loved"]]));
    fireEvent.click(await screen.findByRole("button", { name: "Undo" }));
    await waitFor(() =>
      expect(posted).toEqual([
        [1, "loved"],
        [1, "reopened"],
      ]),
    );
  });

  it("turns a series down with Not interested", async () => {
    serve(twoAndOneReading);
    const posted = captureOutcomes();
    renderWithProviders(<HomePage />);

    fireEvent.click(
      await screen.findByLabelText("Not interested in Salt and Iron"),
    );
    await waitFor(() => expect(posted).toEqual([[2, "not interested"]]));
  });

  it("remembers the list view across renders", async () => {
    serve(twoAndOneReading);
    const first = renderWithProviders(<HomePage />);
    fireEvent.click(await screen.findByText("List"));
    expect(await screen.findByText("S tier")).toBeInTheDocument();
    first.unmount();

    // Persisted to localStorage by the UI store, so a reload keeps it.
    expect(
      JSON.parse(localStorage.getItem("shisho-ui") ?? "{}").state.homeView,
    ).toBe("list");
    renderWithProviders(<HomePage />);
    expect(await screen.findByText("S tier")).toBeInTheDocument();
  });

  it("says when the library could not be read and still lists the log", async () => {
    serve({
      libraryGap: "Codex is not configured on this instance",
      recommended: [rec(1, "The Lighthouse Cartographer", "S")],
      reading: [],
    });
    renderWithProviders(<HomePage />);

    expect(await screen.findByText("Library not checked")).toBeInTheDocument();
    expect(
      screen.getByText("Codex is not configured on this instance", {
        exact: false,
      }),
    ).toBeInTheDocument();
    expect(screen.getByText("The Lighthouse Cartographer")).toBeInTheDocument();
  });
});
