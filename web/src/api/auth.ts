import { useQuery } from "@tanstack/react-query";

// The `/auth/*` routes live at the server root (not under `/api/v1`) and are
// not part of the OpenAPI document, so they are fetched directly rather than
// through the typed `openapi-fetch` client. Requests are same-origin so the
// session cookie rides along automatically.

export interface Provider {
  name: string;
  kind: string;
  label: string;
}

export interface Me {
  id: string;
  email: string | null;
  displayName: string | null;
  avatarUrl: string | null;
  isAdmin: boolean;
}

const base = typeof window !== "undefined" ? window.location.origin : "";

export async function fetchProviders(): Promise<Provider[]> {
  const res = await fetch(`${base}/auth/providers`, {
    credentials: "same-origin",
  });
  if (!res.ok) throw new Error(`failed to load providers (${res.status})`);
  return res.json();
}

/// The current user, or `null` when not authenticated (401).
export async function fetchMe(): Promise<Me | null> {
  const res = await fetch(`${base}/auth/me`, { credentials: "same-origin" });
  if (res.status === 401) return null;
  if (!res.ok) throw new Error(`failed to load current user (${res.status})`);
  return res.json();
}

export async function logout(): Promise<void> {
  await fetch(`${base}/auth/logout`, {
    method: "POST",
    credentials: "same-origin",
  });
}

export function useProviders() {
  return useQuery({ queryKey: ["auth", "providers"], queryFn: fetchProviders });
}

/// The current session user. Disabled in single-user mode (no sessions);
/// `data === null` means the request is authenticated-as-nobody (show login).
export function useMe(enabled: boolean) {
  return useQuery({
    queryKey: ["auth", "me"],
    queryFn: fetchMe,
    enabled,
    retry: false,
  });
}
