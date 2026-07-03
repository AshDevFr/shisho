import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@/api/client";
import type { components } from "@/types/api.generated";

export type TasteProfile = components["schemas"]["ProfileResponse"];
export type ProfileVersion = components["schemas"]["VersionDto"];
export type PendingProposal = components["schemas"]["PendingProposalDto"];

const PROFILE_KEY = ["tasteProfile"] as const;
const VERSIONS_KEY = ["tasteProfile", "versions"] as const;

/// GET /profile. The caller's current taste profile plus any pending proposal.
/// A 409 ("no Codex identity yet") surfaces as the query error so the page can
/// show the mint-first state.
export function useTasteProfile() {
  return useQuery({
    queryKey: PROFILE_KEY,
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/profile");
      if (error) throw new Error(error.error.message);
      return data;
    },
    retry: false,
  });
}

/// GET /profile/versions. Version history, newest first, each with its diff.
export function useProfileVersions() {
  return useQuery({
    queryKey: VERSIONS_KEY,
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/profile/versions");
      if (error) throw new Error(error.error.message);
      return data;
    },
    retry: false,
  });
}

function useProfileInvalidation() {
  const queryClient = useQueryClient();
  return () => {
    queryClient.invalidateQueries({ queryKey: PROFILE_KEY });
    queryClient.invalidateQueries({ queryKey: VERSIONS_KEY });
  };
}

/// PUT /profile. Direct edit: applies a new version immediately.
export function useEditProfile() {
  const invalidate = useProfileInvalidation();
  return useMutation({
    mutationFn: async (body: components["schemas"]["EditProfileRequest"]) => {
      const { data, error } = await api.PUT("/api/v1/profile", { body });
      if (error) throw new Error(error.error.message);
      return data;
    },
    onSuccess: invalidate,
  });
}

/// POST /profile/proposals/{version}/approve. Apply a pending proposal.
export function useApproveProposal() {
  const invalidate = useProfileInvalidation();
  return useMutation({
    mutationFn: async (version: number) => {
      const { data, error } = await api.POST(
        "/api/v1/profile/proposals/{version}/approve",
        { params: { path: { version } } },
      );
      if (error) throw new Error(error.error.message);
      return data;
    },
    onSuccess: invalidate,
  });
}

/// POST /profile/proposals/{version}/reject. Discard a pending proposal.
export function useRejectProposal() {
  const invalidate = useProfileInvalidation();
  return useMutation({
    mutationFn: async (version: number) => {
      const { error } = await api.POST(
        "/api/v1/profile/proposals/{version}/reject",
        { params: { path: { version } } },
      );
      if (error) throw new Error(error.error.message);
    },
    onSuccess: invalidate,
  });
}
