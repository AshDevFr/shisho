import { MantineProvider } from "@mantine/core";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import type { ReactNode } from "react";
import { describe, expect, it } from "vitest";
import { server } from "@/mocks/server";
import { StatusPage } from "./StatusPage";

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

const statusBody = {
  health: {
    status: "degraded",
    db: { reachable: true },
    codex: { configured: true, reachable: true, required: true },
    tsundoku: { configured: false, reachable: false, required: false },
    mangabaka: {
      dumpLoaded: false,
      refreshRunning: false,
      negativeCacheSize: 0,
      apiFallback: true,
      seriesRows: 0,
    },
    caches: {
      librarySnapshotLastWarmedAt: null,
      tsundokuLastWarmedAt: null,
    },
  },
  library: {
    asOf: "2026-07-01T00:00:00Z",
    seriesCount: 3,
    unresolvableCount: 1,
    unresolvable: [
      {
        seriesId: "550e-1",
        title: "Doujin Local",
        rawSources: ["comicinfo:doujin-local"],
        codexUrl: "https://codex.test/series/550e-1",
      },
    ],
  },
  mintedKey: { present: true, codexUserSub: "codex-alice" },
};

describe("StatusPage", () => {
  it("renders health, dump, unresolvable, and key panels", async () => {
    server.use(
      http.get("/api/v1/status", () => HttpResponse.json(statusBody)),
      http.get("/auth/providers", () =>
        HttpResponse.json([{ name: "mock", label: "Mock" }]),
      ),
    );
    renderWithProviders(<StatusPage />);

    await waitFor(() => expect(screen.getByText("Health")).toBeInTheDocument());
    expect(screen.getByText("MangaBaka dump")).toBeInTheDocument();
    expect(screen.getByText("API-only (no dump)")).toBeInTheDocument();
    // Unresolvable series with a Codex deep link.
    const link = screen.getByRole("link", { name: "Doujin Local" });
    expect(link).toHaveAttribute("href", "https://codex.test/series/550e-1");
    // Regenerate routes through the OIDC login flow (the link appears once the
    // providers query resolves).
    const regen = await screen.findByRole("link", {
      name: "Regenerate via Mock",
    });
    expect(regen).toHaveAttribute(
      "href",
      "/auth/login/mock?regenerate=true&redirect_to=/status",
    );
  });

  it("triggers a dump refresh", async () => {
    let posted = false;
    server.use(
      http.get("/api/v1/status", () => HttpResponse.json(statusBody)),
      http.get("/auth/providers", () => HttpResponse.json([])),
      http.post("/api/v1/status/dump-refresh", () => {
        posted = true;
        return new HttpResponse(null, { status: 202 });
      }),
    );
    renderWithProviders(<StatusPage />);

    await waitFor(() =>
      expect(screen.getByText("MangaBaka dump")).toBeInTheDocument(),
    );
    fireEvent.click(screen.getByRole("button", { name: "Refresh now" }));
    await waitFor(() => expect(posted).toBe(true));
  });

  it("surfaces a service-unavailable error", async () => {
    server.use(
      http.get("/api/v1/status", () =>
        HttpResponse.json(
          {
            error: { message: "the recommendation service is not configured" },
          },
          { status: 503 },
        ),
      ),
    );
    renderWithProviders(<StatusPage />);

    await waitFor(() =>
      expect(screen.getByText(/not configured/)).toBeInTheDocument(),
    );
  });
});
