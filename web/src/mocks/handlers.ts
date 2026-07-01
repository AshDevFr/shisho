import { HttpResponse, http } from "msw";

// Mock API handlers, shared by the browser worker (dev:mock) and the node
// server (vitest). Add a handler per endpoint you want to fake.
export const handlers = [
  http.get("/api/v1/health", () => HttpResponse.json({ status: "ok" })),
  http.get("/api/v1/info", () =>
    HttpResponse.json({
      name: "shisho",
      version: "0.1.0",
      auth_mode: "single_user",
    }),
  ),
];
