import { useQuery } from "@tanstack/react-query";
import { api } from "@/api/client";
import type { components } from "@/types/api.generated";

export type Info = components["schemas"]["InfoResponse"];

/// GET /info: instance name, version, and auth mode. Public endpoint.
export function useInfo() {
  return useQuery({
    queryKey: ["info"],
    queryFn: async () => {
      // /info is an infallible public endpoint (declares only a 200), so the
      // client types `error` as `never`; guard on missing data instead.
      const { data } = await api.GET("/api/v1/info");
      if (!data) throw new Error("Failed to load instance info");
      return data;
    },
  });
}
