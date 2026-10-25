import { Stack, Title } from "@mantine/core";
import { Navigate } from "@tanstack/react-router";
import { useMe } from "@/api/auth";
import { useInfo } from "@/api/info";
import { ApiKeysPanel } from "@/components/settings/ApiKeysPanel";
import { EncryptionPanel } from "@/components/settings/EncryptionPanel";
import { SettingsSection } from "@/components/settings/SettingsSection";
import { UsersPanel } from "@/components/settings/UsersPanel";

/// Instance-level settings.
///
/// - Multi-user: this is the **Admin** page (instance-level, sensitive or
///   mutating concerns). Non-admins have no admin area, so they are sent to
///   their Profile. The nav entry is also hidden for them.
/// - Single-user: one operator (implicitly the admin), no admin/profile
///   distinction, so everything collapses onto one flat Settings page —
///   including the Encryption status, which the operator can read like an admin.
export function SettingsPage() {
  const { data: info } = useInfo();
  const multiUser = info?.auth_mode === "multi_user";
  // `me` only exists in multi-user mode; gate the admin view on it.
  const { data: me } = useMe(multiUser);

  if (multiUser) {
    // Wait for `me` before deciding; redirect non-admins to their Profile.
    if (me && me.isAdmin !== true) {
      return <Navigate to="/account" />;
    }
    return (
      <Stack gap="xl">
        <Title order={2}>Admin</Title>
        <SettingsSection title="Users">
          <UsersPanel currentUserId={me?.id} />
        </SettingsSection>
        <SettingsSection title="Encryption">
          <EncryptionPanel />
        </SettingsSection>
      </Stack>
    );
  }

  // Single-user: one flat page with every applicable panel.
  return (
    <Stack gap="xl">
      <Title order={2}>Settings</Title>
      <SettingsSection title="API keys">
        <ApiKeysPanel />
      </SettingsSection>
      <SettingsSection title="Encryption">
        <EncryptionPanel />
      </SettingsSection>
    </Stack>
  );
}
