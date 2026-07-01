import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@/api/client";
import type { components } from "@/types/api.generated";

export type Connection = components["schemas"]["ConnectionResponse"];

/// GET /connections. The OAuth clients (e.g. an MCP connector) holding a live
/// grant on the signed-in user's account, grouped one row per client.
export function useConnections() {
  return useQuery({
    queryKey: ["connections"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/connections");
      if (error) throw new Error(error.error.message);
      return data;
    },
  });
}

/// DELETE /connections/{clientId}. Revokes every token the client holds for the
/// user; the connector stops authenticating immediately.
export function useRevokeConnection() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (clientId: string) => {
      const { error } = await api.DELETE("/api/v1/connections/{client_id}", {
        params: { path: { client_id: clientId } },
      });
      if (error) throw new Error(error.error.message);
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["connections"] });
    },
  });
}
