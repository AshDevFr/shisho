import { create } from "zustand";
import { persist } from "zustand/middleware";

// The SPA authenticates with the same Bearer API keys as every other client.
// In single-user mode the key is entered once (the gate in Layout) and
// persisted in localStorage; the api client middleware attaches it to every
// request. In multi-user mode the session cookie does the work and this stays
// null.
interface AuthState {
  apiKey: string | null;
  setApiKey: (key: string) => void;
  clearApiKey: () => void;
}

export const useAuthStore = create<AuthState>()(
  persist(
    (set) => ({
      apiKey: null,
      setApiKey: (key) => set({ apiKey: key.trim() || null }),
      clearApiKey: () => set({ apiKey: null }),
    }),
    { name: "shisho-api-key" },
  ),
);
