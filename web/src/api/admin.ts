import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@/api/client";
import type { components } from "@/types/api.generated";

export type AdminUser = components["schemas"]["AdminUser"];
export type EncryptionStatus = components["schemas"]["EncryptionStatus"];

/// GET /admin/encryption: at-rest encryption status (admin only). Read-only;
/// never returns key material. `enabled` so it only fires for an admin.
export function useEncryptionStatus(enabled: boolean) {
  return useQuery({
    queryKey: ["admin", "encryption"],
    enabled,
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/admin/encryption");
      if (error) throw new Error(error.error.message);
      return data;
    },
  });
}

/// GET /admin/users: the admin user-management list (multi-user, admin only).
/// `enabled` so the query only fires for an admin viewing the Settings page.
export function useUsers(enabled: boolean) {
  return useQuery({
    queryKey: ["admin", "users"],
    enabled,
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/admin/users");
      if (error) throw new Error(error.error.message);
      return data;
    },
  });
}

export function useDeactivateUser() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (id: string) => {
      const { error } = await api.POST("/api/v1/admin/users/{id}/deactivate", {
        params: { path: { id } },
      });
      if (error) throw new Error(error.error.message);
    },
    onSuccess: () =>
      queryClient.invalidateQueries({ queryKey: ["admin", "users"] }),
  });
}

export function useActivateUser() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (id: string) => {
      const { error } = await api.POST("/api/v1/admin/users/{id}/activate", {
        params: { path: { id } },
      });
      if (error) throw new Error(error.error.message);
    },
    onSuccess: () =>
      queryClient.invalidateQueries({ queryKey: ["admin", "users"] }),
  });
}
