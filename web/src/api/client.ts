import createClient from "openapi-fetch";
import { useAuthStore } from "@/stores/auth";
import type { paths } from "@/types/api.generated";

// Typed against the generated OpenAPI paths. Regenerate after backend changes:
//   make openapi-all
//
// baseUrl is the current origin so requests stay same-origin: Vite proxies /api
// to the backend in dev, and rust-embed serves the SPA and the API from one
// binary in prod. (An absolute base is also required under jsdom, where the
// test fetch cannot resolve relative URLs.)
//
// The `fetch` wrapper defers to the global fetch at call time. openapi-fetch
// otherwise captures globalThis.fetch when the client is created, which would
// miss test mocks (MSW) that patch fetch after this module loads.
const baseUrl = typeof window !== "undefined" ? window.location.origin : "";

export const api = createClient<paths>({
  baseUrl,
  fetch: (...args) => fetch(...args),
});

// Every request carries the stored API key as a Bearer token (single-user
// mode). Reading the store at request time (not module load) picks up key
// changes immediately. In multi-user mode there is no key and the session
// cookie authenticates instead.
api.use({
  onRequest({ request }) {
    const apiKey = useAuthStore.getState().apiKey;
    if (apiKey) {
      request.headers.set("Authorization", `Bearer ${apiKey}`);
    }
    return request;
  },
});

// A rejected key means the stored one is stale (revoked/expired): drop it so
// the key gate comes back instead of every view failing quietly.
api.use({
  onResponse({ response }) {
    if (response.status === 401) {
      useAuthStore.getState().clearApiKey();
    }
    return response;
  },
});
