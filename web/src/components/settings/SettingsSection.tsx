import { Stack, Title } from "@mantine/core";
import type { ReactNode } from "react";

/// A titled settings block. Shared by the Profile and Admin/Settings pages so
/// section headers and spacing stay consistent across both.
export function SettingsSection({
  title,
  children,
}: {
  title: string;
  children: ReactNode;
}) {
  return (
    <Stack gap="sm">
      <Title order={3}>{title}</Title>
      {children}
    </Stack>
  );
}
