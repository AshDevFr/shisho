import { create } from "zustand";
import { persist } from "zustand/middleware";

export type VerdictsView = "card" | "table";
export type HomeView = "card" | "list";

// UI preferences persisted to localStorage (survives reloads).
interface UiState {
  /// How the Verdicts page renders its rows.
  verdictsView: VerdictsView;
  setVerdictsView: (v: VerdictsView) => void;
  /// How Home renders the current recommendations.
  homeView: HomeView;
  setHomeView: (v: HomeView) => void;
}

export const useUiStore = create<UiState>()(
  persist(
    (set) => ({
      verdictsView: "card",
      setVerdictsView: (verdictsView) => set({ verdictsView }),
      homeView: "card",
      setHomeView: (homeView) => set({ homeView }),
    }),
    { name: "shisho-ui" },
  ),
);
