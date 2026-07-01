import { MantineProvider } from "@mantine/core";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, it } from "vitest";
import { HomePage } from "./HomePage";

function renderWithProviders(ui: ReactNode) {
  const client = new QueryClient();
  return render(
    <MantineProvider>
      <QueryClientProvider client={client}>{ui}</QueryClientProvider>
    </MantineProvider>,
  );
}

describe("HomePage", () => {
  it("renders the instance name and version from the mocked /info", async () => {
    renderWithProviders(<HomePage />);
    await waitFor(() => {
      expect(screen.getByText("0.1.0")).toBeInTheDocument();
    });
    expect(screen.getByText("single_user")).toBeInTheDocument();
  });
});
