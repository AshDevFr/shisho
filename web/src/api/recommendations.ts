import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@/api/client";
import type { components } from "@/types/api.generated";

export type Recommendation = components["schemas"]["RecommendationDto"];
export type CurrentRecommendations =
  components["schemas"]["CurrentRecommendationsResponse"];

export const RECOMMENDATIONS_KEY = ["recommendations"] as const;

/// GET /recommendations. The caller's standing recommendations, one per
/// series across sessions, split into recommended and reading by their live
/// library. A 409 ("no Codex identity yet") surfaces as the query error.
export function useRecommendations() {
  return useQuery({
    queryKey: RECOMMENDATIONS_KEY,
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/recommendations");
      if (error) throw new Error(error.error.message);
      return data;
    },
    retry: false,
  });
}

/// POST /recommendations/sync. Re-reads the library from Codex and records
/// what it shows (ratings, stalled and resumed reads), then refetches.
export function useSyncRecommendations() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async () => {
      const { data, error } = await api.POST("/api/v1/recommendations/sync");
      if (error) throw new Error(error.error.message);
      return data;
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: RECOMMENDATIONS_KEY });
      queryClient.invalidateQueries({ queryKey: ["verdicts"] });
    },
  });
}
