import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@/api/client";
import type { components } from "@/types/api.generated";

export type Status = components["schemas"]["StatusResponse"];

const STATUS_KEY = ["status"] as const;

/// GET /status. Backend reachability, cache freshness, dump age/refresh state,
/// the caller's unresolvable list, and minted-key state. Polls while a dump
/// refresh is running so the UI reflects progress.
export function useStatus() {
  return useQuery({
    queryKey: STATUS_KEY,
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/status");
      if (error) throw new Error(error.error.message);
      return data;
    },
    retry: false,
    // While a refresh is in flight, refetch so the dump age / running flag
    // update without a manual reload.
    refetchInterval: (query) =>
      query.state.data?.health.mangabaka.refreshRunning ? 4000 : false,
  });
}

/// POST /status/dump-refresh. Triggers a background MangaBaka dump refresh and
/// returns immediately (202). Invalidate status so the running state shows.
export function useDumpRefresh() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async () => {
      const { error } = await api.POST("/api/v1/status/dump-refresh");
      if (error) throw new Error(error.error.message);
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: STATUS_KEY });
    },
  });
}
