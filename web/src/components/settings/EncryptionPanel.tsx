import { Alert, Badge, Group, Loader, Stack, Text } from "@mantine/core";
import { type EncryptionStatus, useEncryptionStatus } from "@/api/admin";

function formatDate(iso: string | null | undefined) {
  return iso ? new Date(iso).toLocaleDateString() : "never";
}

function Row({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <Group justify="space-between">
      <Text c="dimmed" size="sm">
        {label}
      </Text>
      <Text size="sm">{value}</Text>
    </Group>
  );
}

function Details({ status }: { status: EncryptionStatus }) {
  if (!status.enabled) {
    return (
      <Stack gap="xs">
        <Group justify="space-between">
          <Text c="dimmed" size="sm">
            Status
          </Text>
          <Badge color="gray" data-testid="encryption-status-off">
            Off
          </Badge>
        </Group>
        <Text c="dimmed" size="xs">
          Encryption at rest is off. Configure a key to encrypt the database;
          generate one with <code>shisho encryption gen</code> and set it under{" "}
          <code>[encryption]</code> in the config.
        </Text>
      </Stack>
    );
  }

  return (
    <Stack gap="xs">
      <Group justify="space-between">
        <Text c="dimmed" size="sm">
          Status
        </Text>
        <Badge color="teal" data-testid="encryption-status-on">
          On
        </Badge>
      </Group>
      <Row label="Key version" value={status.kekVersion ?? "—"} />
      <Row label="Last rotated" value={formatDate(status.lastRotatedAt)} />
    </Stack>
  );
}

/// Read-only at-rest encryption status (admin only). Key management is a
/// deployment-layer concern (config + the `shisho encryption` CLI), so this panel
/// only reports state — it never exposes or mutates key material.
export function EncryptionPanel() {
  const { data, isPending, isError, error } = useEncryptionStatus(true);

  if (isPending) {
    return <Loader size="sm" data-testid="encryption-panel-loading" />;
  }
  if (isError) {
    return (
      <Alert color="red" title="Could not load encryption status">
        {error.message}
      </Alert>
    );
  }
  return <Details status={data} />;
}
