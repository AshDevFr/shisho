import { MantineProvider } from "@mantine/core";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, RouterProvider } from "@tanstack/react-router";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { server } from "@/mocks/server";
import { createAppRouter } from "@/router";

function signedIn() {
  server.use(
    http.get("/api/v1/info", () =>
      HttpResponse.json({
        name: "shisho",
        version: "0.1.2",
        auth_mode: "multi_user",
      }),
    ),
    http.get("/auth/me", () =>
      HttpResponse.json({
        id: "user-1",
        email: "reader@example.com",
        displayName: "Reader",
        avatarUrl: null,
        isAdmin: false,
      }),
    ),
    http.get("/api/v1/verdicts", () => HttpResponse.json([])),
    http.get("/api/v1/recommendations", () =>
      HttpResponse.json({ recommended: [], reading: [] }),
    ),
    http.get("/api/v1/keys", () => HttpResponse.json([])),
    http.get("/api/v1/connections", () => HttpResponse.json([])),
  );
}

function renderAt(path: string) {
  const router = createAppRouter(
    createMemoryHistory({ initialEntries: [path] }),
  );
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  render(
    <MantineProvider>
      <QueryClientProvider client={client}>
        <RouterProvider router={router} />
      </QueryClientProvider>
    </MantineProvider>,
  );
  return router;
}

describe("Layout", () => {
  it("names the pages Home, Taste profile, History, Status and Account", async () => {
    signedIn();
    renderAt("/history");
    await waitFor(() =>
      expect(screen.getByRole("link", { name: "History" })).toBeInTheDocument(),
    );
    const names = screen
      .getAllByRole("link")
      .map((a) => a.textContent)
      .filter(Boolean);
    expect(names).toEqual([
      "Home",
      "Taste profile",
      "History",
      "Status",
      "Account",
    ]);
  });

  it("closes the phone menu when a page is chosen", async () => {
    signedIn();
    renderAt("/");
    const burger = await screen.findByRole("button", {
      name: "Toggle navigation",
    });
    fireEvent.click(burger);
    await waitFor(() =>
      expect(burger).toHaveAttribute("aria-expanded", "true"),
    );

    fireEvent.click(screen.getByRole("link", { name: "History" }));
    await waitFor(() =>
      expect(burger).toHaveAttribute("aria-expanded", "false"),
    );
  });

  it("sends the old Verdicts and Profile paths to their new pages", async () => {
    signedIn();
    const verdicts = renderAt("/verdicts");
    await waitFor(() =>
      expect(verdicts.state.location.pathname).toBe("/history"),
    );

    const profile = renderAt("/profile");
    await waitFor(() =>
      expect(profile.state.location.pathname).toBe("/account"),
    );
  });
});
