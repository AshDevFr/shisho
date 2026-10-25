import {
  createRootRoute,
  createRoute,
  createRouter,
  Outlet,
  redirect,
} from "@tanstack/react-router";
import { Layout } from "@/components/Layout";
import { HomePage } from "@/pages/HomePage";
import { LoginPage } from "@/pages/LoginPage";
import { ProfilePage } from "@/pages/ProfilePage";
import { SettingsPage } from "@/pages/SettingsPage";
import { SetupPage } from "@/pages/SetupPage";
import { StatusPage } from "@/pages/StatusPage";
import { TasteProfilePage } from "@/pages/TasteProfilePage";
import { VerdictsPage } from "@/pages/VerdictsPage";

// Code-based routing keeps the scaffold self-contained (no router codegen
// plugin). Switch to file-based routing later if the route tree grows.
const rootRoute = createRootRoute({ component: () => <Outlet /> });

// The login page sits outside the app shell so the auth guard can redirect to
// it without a layout loop. It is only reachable in multi-user mode; in
// single-user mode it redirects home.
const loginRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/login",
  component: LoginPage,
});

// The first-run wizard sits outside the app shell and its auth gate: it is the
// pre-authentication entry point, reachable only via the tokenized link.
const setupRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/setup",
  component: SetupPage,
});

// The app views share the AppShell (and its auth gate).
const layoutRoute = createRoute({
  getParentRoute: () => rootRoute,
  id: "layout",
  component: Layout,
});

const homeRoute = createRoute({
  getParentRoute: () => layoutRoute,
  path: "/",
  component: HomePage,
});

// Per-user account settings (multi-user). Single-user mode has no separate
// profile; the page redirects to /settings there.
const profileRoute = createRoute({
  getParentRoute: () => layoutRoute,
  path: "/account",
  component: ProfilePage,
});

// The taste profile: the primary user's own recommendation data. Available in
// both auth modes (unlike the account /profile page).
const tasteProfileRoute = createRoute({
  getParentRoute: () => layoutRoute,
  path: "/taste-profile",
  component: TasteProfilePage,
});

// History: the append-only verdict log with outcome correction. Both auth
// modes.
const verdictsRoute = createRoute({
  getParentRoute: () => layoutRoute,
  path: "/history",
  component: VerdictsPage,
});

// The pages' former paths, kept so old bookmarks land on the renamed pages.
const legacyVerdictsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/verdicts",
  beforeLoad: () => {
    throw redirect({ to: "/history" });
  },
});
const legacyProfileRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/profile",
  beforeLoad: () => {
    throw redirect({ to: "/account" });
  },
});

// Operational status: health, dump refresh, unresolvable list, key regenerate.
const statusRoute = createRoute({
  getParentRoute: () => layoutRoute,
  path: "/status",
  component: StatusPage,
});

// Instance settings: the Admin page in multi-user mode, the flat Settings page
// in single-user mode.
const settingsRoute = createRoute({
  getParentRoute: () => layoutRoute,
  path: "/settings",
  component: SettingsPage,
});

const routeTree = rootRoute.addChildren([
  loginRoute,
  setupRoute,
  legacyVerdictsRoute,
  legacyProfileRoute,
  layoutRoute.addChildren([
    homeRoute,
    tasteProfileRoute,
    verdictsRoute,
    statusRoute,
    profileRoute,
    settingsRoute,
  ]),
]);

/// Tests build their own router over a memory history; the app uses the
/// singleton below.
export function createAppRouter(
  history?: Parameters<typeof createRouter>[0]["history"],
) {
  return createRouter({ routeTree, history });
}

export const router = createAppRouter();

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
