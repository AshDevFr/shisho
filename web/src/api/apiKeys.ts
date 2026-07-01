import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@/api/client";
import type { components } from "@/types/api.generated";

export type ApiKey = components["schemas"]["ApiKeyResponse"];
export type CreateKeyRequest = components["schemas"]["CreateKeyRequest"];
export type CreatedKey = components["schemas"]["CreatedKeyResponse"];

export function useApiKeys() {
  return useQuery({
    queryKey: ["apiKeys"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/keys");
      if (error) throw new Error(error.error.message);
      return data;
    },
  });
}

/// POST /keys. The response is the only place the plaintext key ever appears;
/// callers must keep it in transient component state only.
export function useCreateApiKey() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (body: CreateKeyRequest): Promise<CreatedKey> => {
      const { data, error } = await api.POST("/api/v1/keys", { body });
      if (error) throw new Error(error.error.message);
      return data;
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["apiKeys"] });
    },
  });
}

export function useRevokeApiKey() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (id: string) => {
      const { error } = await api.DELETE("/api/v1/keys/{id}", {
        params: { path: { id } },
      });
      if (error) throw new Error(error.error.message);
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["apiKeys"] });
    },
  });
}
