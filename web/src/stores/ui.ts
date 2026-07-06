import { create } from "zustand";
import { persist } from "zustand/middleware";

export type VerdictsView = "card" | "table";

// UI preferences persisted to localStorage (survives reloads).
interface UiState {
  /// How the Verdicts page renders its rows.
  verdictsView: VerdictsView;
  setVerdictsView: (v: VerdictsView) => void;
}

export const useUiStore = create<UiState>()(
  persist(
    (set) => ({
      verdictsView: "card",
      setVerdictsView: (verdictsView) => set({ verdictsView }),
    }),
    { name: "shisho-ui" },
  ),
);
