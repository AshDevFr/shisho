import { Stack, Title } from "@mantine/core";
import { Navigate } from "@tanstack/react-router";
import { useInfo } from "@/api/info";
import { ApiKeysPanel } from "@/components/settings/ApiKeysPanel";
import { ConnectedAppsPanel } from "@/components/settings/ConnectedAppsPanel";
import { SettingsSection } from "@/components/settings/SettingsSection";

/// Per-user account settings (multi-user mode): the things that are "about you"
/// rather than about the instance. Single-user mode has no separate profile —
/// everything lives on the Settings page — so it redirects there.
export function ProfilePage() {
  const { data: info } = useInfo();
  const multiUser = info?.auth_mode === "multi_user";

  if (info && !multiUser) {
    return <Navigate to="/settings" />;
  }

  return (
    <Stack gap="xl">
      <Title order={2}>Profile</Title>
      <SettingsSection title="API keys">
        <ApiKeysPanel />
      </SettingsSection>
      <SettingsSection title="Connected applications">
        <ConnectedAppsPanel />
      </SettingsSection>
    </Stack>
  );
}
