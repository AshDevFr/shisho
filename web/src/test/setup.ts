import "@testing-library/jest-dom/vitest";
import { afterAll, afterEach, beforeAll, vi } from "vitest";
import { server } from "@/mocks/server";

// jsdom doesn't implement these browser APIs, but Mantine (and many UI libs)
// call them on mount. Provide minimal stubs so component tests can render.
Object.defineProperty(window, "matchMedia", {
  writable: true,
  value: vi.fn().mockImplementation((query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    addListener: vi.fn(),
    removeListener: vi.fn(),
    dispatchEvent: vi.fn(),
  })),
});

// A real class (not an arrow-returning vi.fn) so components that do
// `new ResizeObserver()` — Mantine's Select/ScrollArea — can construct it.
class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
window.ResizeObserver = ResizeObserverStub as unknown as typeof ResizeObserver;

window.scrollTo = vi.fn() as unknown as typeof window.scrollTo;

// fireEvent doesn't wrap async UI-library state updates (Mantine popovers,
// floating-ui positioning, etc.) in act(), even though the tests still await
// the visible outcome via findBy*/waitFor. The resulting "not wrapped in
// act(...)" lines are noise rather than real bugs. Swap to
// @testing-library/user-event if you'd rather fix the cause; until then,
// suppress just that one message and let everything else through.
const originalConsoleError = console.error;
console.error = (...args: unknown[]) => {
  const first = args[0];
  if (typeof first === "string" && first.includes("not wrapped in act(")) {
    return;
  }
  originalConsoleError(...args);
};

// MSW: intercept API calls for the whole test run.
beforeAll(() => server.listen({ onUnhandledFrame: "bypass" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());
