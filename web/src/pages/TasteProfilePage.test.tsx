import { MantineProvider } from "@mantine/core";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import type { ReactNode } from "react";
import { describe, expect, it } from "vitest";
import { server } from "@/mocks/server";
import { TasteProfilePage } from "./TasteProfilePage";

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

describe("TasteProfilePage", () => {
  it("renders the current profile, the pending proposal, and the history", async () => {
    server.use(
      http.get("/api/v1/profile", () =>
        HttpResponse.json({
          content: "# Loves\n- isekai",
          version: 1,
          updatedAt: "2026-07-01T00:00:00Z",
          pendingProposal: {
            version: 2,
            baseVersion: 1,
            content: "# Loves\n- isekai\n- mystery",
            changeSummary: "add mystery",
            diff: "+ mystery",
          },
        }),
      ),
      http.get("/api/v1/profile/versions", () =>
        HttpResponse.json([
          {
            version: 1,
            baseVersion: 0,
            status: "applied",
            changeSummary: "first version",
            createdAt: "2026-07-01T00:00:00Z",
            diff: "+ isekai",
          },
        ]),
      ),
    );

    renderWithProviders(<TasteProfilePage />);

    await waitFor(() => expect(screen.getByText(/isekai/)).toBeInTheDocument());
    expect(screen.getByText("Current")).toBeInTheDocument();
    expect(screen.getByText("Pending proposal")).toBeInTheDocument();
    expect(screen.getByText("add mystery")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Approve" })).toBeInTheDocument();
    // The history query resolves independently; wait for it.
    expect(await screen.findByText("History")).toBeInTheDocument();
    expect(screen.getByText("first version")).toBeInTheDocument();
  });

  it("shows an actionable message when the caller has no Codex identity", async () => {
    server.use(
      http.get("/api/v1/profile", () =>
        HttpResponse.json(
          {
            error: {
              message:
                "no Codex identity yet: sign in to mint a Codex key before using your taste profile",
            },
          },
          { status: 409 },
        ),
      ),
    );

    renderWithProviders(<TasteProfilePage />);

    await waitFor(() =>
      expect(screen.getByText(/no Codex identity yet/)).toBeInTheDocument(),
    );
  });
});
