import { useMutation, useQuery } from "@tanstack/react-query";
import { api } from "@/api/client";
import type { components } from "@/types/api.generated";

export type SetupState = components["schemas"]["SetupStateResponse"];
export type SetupKey = components["schemas"]["SetupKeyResponse"];

/// GET /setup: first-run state, gated by the one-shot token from the startup
/// log. A 404 means setup is closed (already configured) or the token is wrong.
export function useSetupState(token: string | null) {
  return useQuery({
    queryKey: ["setup", token],
    enabled: !!token,
    retry: false,
    queryFn: async (): Promise<SetupState> => {
      const { data, error } = await api.GET("/api/v1/setup", {
        params: { query: { token: token ?? "" } },
      });
      if (error) throw new Error(error.error.message);
      return data;
    },
  });
}

/// POST /setup/key: mint and reveal an admin key, consuming the token. The
/// plaintext appears only here.
export function useGenerateSetupKey(token: string | null) {
  return useMutation({
    mutationFn: async (): Promise<SetupKey> => {
      const { data, error } = await api.POST("/api/v1/setup/key", {
        params: { query: { token: token ?? "" } },
      });
      if (error) throw new Error(error.error.message);
      return data;
    },
  });
}
