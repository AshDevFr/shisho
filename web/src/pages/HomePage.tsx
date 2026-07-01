import { Card, Group, Stack, Text, Title } from "@mantine/core";
import { useInfo } from "@/api/info";

/// The landing page inside the app shell. A minimal status view over the public
/// `/info` endpoint — replace it with your app's home once you add a domain.
export function HomePage() {
  const { data: info } = useInfo();

  return (
    <Stack gap="lg">
      <Title order={2}>{info?.name ?? "shisho"}</Title>
      <Text c="dimmed">
        You're connected. This is the scaffold home page; build your app here.
      </Text>
      <Card withBorder w={360}>
        <Stack gap="xs">
          <Row label="Version" value={info?.version ?? "…"} />
          <Row label="Auth mode" value={info?.auth_mode ?? "…"} />
        </Stack>
      </Card>
    </Stack>
  );
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <Group justify="space-between">
      <Text c="dimmed" size="sm">
        {label}
      </Text>
      <Text size="sm">{value}</Text>
    </Group>
  );
}
