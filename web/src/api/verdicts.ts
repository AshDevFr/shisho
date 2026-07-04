import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@/api/client";
import type { components } from "@/types/api.generated";

export type SessionGroup = components["schemas"]["SessionGroup"];
export type Verdict = components["schemas"]["VerdictDto"];

/// Filters for the verdict-log query. Undefined fields do not constrain.
export type VerdictFilters = {
  tier?: string;
  verified?: boolean;
  retracted?: boolean;
  outcome?: string;
};

const VERDICTS_KEY = ["verdicts"] as const;

/// GET /verdicts. The caller's append-only recommendation log, filtered and
/// grouped by session (most recent first). A 409 ("no Codex identity yet")
/// surfaces as the query error.
export function useVerdicts(filters: VerdictFilters) {
  return useQuery({
    queryKey: [...VERDICTS_KEY, filters],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/verdicts", {
        params: {
          query: {
            tier: filters.tier || undefined,
            verified: filters.verified,
            retracted: filters.retracted,
            outcome: filters.outcome || undefined,
          },
        },
      });
      if (error) throw new Error(error.error.message);
      return data;
    },
    retry: false,
  });
}

/// POST /verdicts/{id}/outcome. Records a reading outcome by appending a
/// feedback row (the log stays append-only); the correction shows up as a new
/// entry on refetch.
export function useRecordOutcome() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (args: {
      id: number;
      outcome: string;
      notes?: string;
    }) => {
      const { data, error } = await api.POST("/api/v1/verdicts/{id}/outcome", {
        params: { path: { id: args.id } },
        body: { outcome: args.outcome, notes: args.notes },
      });
      if (error) throw new Error(error.error.message);
      return data;
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: VERDICTS_KEY });
    },
  });
}
