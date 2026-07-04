import {
  AppShell,
  Avatar,
  Button,
  Center,
  Group,
  Loader,
  NavLink,
  Paper,
  PasswordInput,
  Stack,
  Text,
  Title,
} from "@mantine/core";
import { useQueryClient } from "@tanstack/react-query";
import { Link, Navigate, Outlet, useRouterState } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";
import { logout, type Me, useMe } from "@/api/auth";
import { useInfo } from "@/api/info";
import { useAuthStore } from "@/stores/auth";
import { ColorSchemeToggle } from "./ColorSchemeToggle";

// Route paths the navbar can link to. Keeping `to` a literal union (not
// `string`) preserves TanStack Router's typed-link checking. Add your own
// pages here as the app grows.
type NavPath =
  | "/"
  | "/taste-profile"
  | "/verdicts"
  | "/status"
  | "/profile"
  | "/settings";
// (all domain + account nav paths; kept as a literal union for typed links)
type NavItem = { to: NavPath; label: string };

// The domain pages (the primary user's own recommendation data) show in both
// auth modes, ahead of the account/instance settings entries.
const BASE_NAV: NavItem[] = [
  { to: "/", label: "Home" },
  { to: "/taste-profile", label: "Taste profile" },
  { to: "/verdicts", label: "Verdicts" },
  { to: "/status", label: "Status" },
];

/// The navbar entries for the current mode/role.
///
/// - Single-user: the domain pages plus one flat "Settings" entry.
/// - Multi-user: the domain pages, a per-user "Profile" (account) entry, plus an
///   "Admin" entry (→ /settings) only for admins.
function navItems(multiUser: boolean, isAdmin: boolean): NavItem[] {
  if (!multiUser) {
    return [...BASE_NAV, { to: "/settings", label: "Settings" }];
  }
  const items: NavItem[] = [...BASE_NAV, { to: "/profile", label: "Profile" }];
  if (isAdmin) {
    items.push({ to: "/settings", label: "Admin" });
  }
  return items;
}

/// The top-level gate. Single-user mode keeps the API-key gate; multi-user mode
/// is session-based (login redirect + user menu + logout).
export function Layout() {
  const info = useInfo();
  const multiUser = info.data?.auth_mode === "multi_user";

  // Resolve the session user only in multi-user mode.
  const me = useMe(multiUser);

  if (info.isLoading) {
    return <FullscreenLoader />;
  }

  if (multiUser) {
    if (me.isLoading) {
      return <FullscreenLoader />;
    }
    if (!me.data) {
      // Unauthenticated: bounce to the login route (no loop — /login is
      // outside this layout).
      return <Navigate to="/login" />;
    }
    return <Shell user={me.data} multiUser />;
  }

  return <SingleUserGate />;
}

/// The app shell (navbar + content). `user` is present in multi-user mode and
/// drives the account row with logout.
function Shell({
  user,
  multiUser = false,
}: {
  user?: Me;
  multiUser?: boolean;
}) {
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const queryClient = useQueryClient();

  const onLogout = async () => {
    await logout();
    // Drop the cached user so the layout re-evaluates and redirects to /login.
    await queryClient.invalidateQueries({ queryKey: ["auth", "me"] });
  };

  const items = navItems(multiUser, user?.isAdmin === true);

  return (
    <AppShell navbar={{ width: 200, breakpoint: "sm" }} padding="md">
      <AppShell.Navbar p="xs">
        <Title order={3} p="sm">
          shisho
        </Title>
        {items.map((item) => (
          <NavLink
            key={item.to}
            component={Link}
            to={item.to}
            label={item.label}
            active={pathname === item.to}
          />
        ))}
        <Stack mt="auto" p="xs" gap="xs">
          <Group justify="flex-start">
            <ColorSchemeToggle />
          </Group>
          {user && (
            <>
              <Group gap="xs" wrap="nowrap">
                <Avatar src={user.avatarUrl} radius="xl" size="sm" />
                <Text size="sm" truncate>
                  {user.displayName ?? user.email ?? "Account"}
                </Text>
              </Group>
              <Button variant="subtle" size="xs" onClick={onLogout}>
                Log out
              </Button>
            </>
          )}
        </Stack>
      </AppShell.Navbar>
      <AppShell.Main>
        <Outlet />
      </AppShell.Main>
    </AppShell>
  );
}

function FullscreenLoader() {
  return (
    <Center h="100vh">
      <Loader />
    </Center>
  );
}

/// API-key gate: single-user mode authenticates with a Bearer key entered once
/// and persisted in localStorage.
function SingleUserGate() {
  const apiKey = useAuthStore((s) => s.apiKey);
  if (!apiKey) {
    return <ApiKeyGate />;
  }
  return <Shell />;
}

function ApiKeyGate() {
  const setApiKey = useAuthStore((s) => s.setApiKey);
  const [value, setValue] = useState("");

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (value.trim()) setApiKey(value);
  };

  return (
    <Center h="100vh">
      <Paper withBorder p="xl" w={420}>
        <form onSubmit={submit}>
          <Stack>
            <Title order={2}>shisho</Title>
            <Text c="dimmed" size="sm">
              Enter an API key to connect. The server prints one on first run;
              keys are managed under Settings or via <code>/api/v1/keys</code>.
            </Text>
            <PasswordInput
              label="API key"
              placeholder="shisho_..."
              value={value}
              onChange={(e) => setValue(e.currentTarget.value)}
              data-autofocus
            />
            <Group justify="flex-end">
              <Button type="submit" disabled={!value.trim()}>
                Connect
              </Button>
            </Group>
          </Stack>
        </form>
      </Paper>
    </Center>
  );
}
