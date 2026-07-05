import {
  Alert,
  Anchor,
  Badge,
  Button,
  Code,
  Group,
  List,
  Loader,
  Paper,
  Stack,
  Text,
  Title,
} from "@mantine/core";
import { notifications } from "@mantine/notifications";
import { useProviders } from "@/api/auth";
import { type Status, useDumpRefresh, useStatus } from "@/api/status";

/// Operational status: backend reachability, cache freshness, the MangaBaka
/// dump age + refresh trigger, the caller's unresolvable-series list (with
/// Codex deep links), and minted-key state with an OIDC re-auth regenerate.
export function StatusPage() {
  const status = useStatus();

  return (
    <Stack gap="xl">
      <Title order={2}>Status</Title>

      {status.isLoading && <Loader />}
      {status.error && (
        <Alert color="yellow" title="Status unavailable">
          {status.error.message}
        </Alert>
      )}

      {status.data && (
        <>
          <HealthPanel health={status.data.health} />
          <DumpPanel health={status.data.health} />
          {status.data.library && (
            <UnresolvablePanel library={status.data.library} />
          )}
          <CodexKeyPanel mintedKey={status.data.mintedKey} />
        </>
      )}
    </Stack>
  );
}

function healthColor(s: string) {
  return s === "ok" ? "green" : s === "degraded" ? "yellow" : "red";
}

function BackendRow({
  label,
  backend,
}: {
  label: string;
  backend: Status["health"]["codex"];
}) {
  const color = !backend.configured
    ? "gray"
    : backend.reachable
      ? "green"
      : backend.required
        ? "red"
        : "yellow";
  const text = !backend.configured
    ? "not configured"
    : backend.reachable
      ? "reachable"
      : "unreachable";
  return (
    <Group gap="xs">
      <Text fw={500} w={110}>
        {label}
      </Text>
      <Badge color={color} variant="light">
        {text}
      </Badge>
      {backend.error && (
        <Text size="xs" c="dimmed">
          {backend.error}
        </Text>
      )}
    </Group>
  );
}

function HealthPanel({ health }: { health: Status["health"] }) {
  return (
    <Paper withBorder p="md">
      <Group gap="xs" mb="sm">
        <Title order={4}>Health</Title>
        <Badge color={healthColor(health.status)}>{health.status}</Badge>
      </Group>
      <Stack gap={6}>
        <Group gap="xs">
          <Text fw={500} w={110}>
            Database
          </Text>
          <Badge color={health.db.reachable ? "green" : "red"} variant="light">
            {health.db.reachable ? "reachable" : "unreachable"}
          </Badge>
        </Group>
        <BackendRow label="Codex" backend={health.codex} />
        <BackendRow label="Tsundoku" backend={health.tsundoku} />
        <Group gap="xs" mt={4}>
          <Text size="sm" c="dimmed">
            Library snapshot warmed:{" "}
            {health.caches.librarySnapshotLastWarmedAt ?? "never"}
          </Text>
        </Group>
      </Stack>
    </Paper>
  );
}

function DumpPanel({ health }: { health: Status["health"] }) {
  const refresh = useDumpRefresh();
  const mb = health.mangabaka;

  return (
    <Paper withBorder p="md">
      <Group justify="space-between" mb="sm">
        <Title order={4}>MangaBaka dump</Title>
        <Button
          size="xs"
          variant="light"
          loading={refresh.isPending || mb.refreshRunning}
          disabled={mb.refreshRunning}
          onClick={() =>
            refresh.mutate(undefined, {
              onSuccess: () =>
                notifications.show({ message: "Dump refresh started." }),
              onError: (err) =>
                notifications.show({ color: "red", message: err.message }),
            })
          }
        >
          {mb.refreshRunning ? "Refreshing…" : "Refresh now"}
        </Button>
      </Group>
      <Stack gap={6}>
        <Group gap="xs">
          <Badge color={mb.dumpLoaded ? "green" : "yellow"} variant="light">
            {mb.dumpLoaded ? "dump loaded" : "API-only (no dump)"}
          </Badge>
          {mb.seriesRows != null && (
            <Text size="sm" c="dimmed">
              {mb.seriesRows.toLocaleString()} series
            </Text>
          )}
        </Group>
        <Text size="sm" c="dimmed">
          Dump modified: {mb.dumpModifiedAt ?? "—"}
        </Text>
        {mb.lastRefreshError && (
          <Text size="sm" c="red">
            Last refresh failed: {mb.lastRefreshError}
          </Text>
        )}
      </Stack>
    </Paper>
  );
}

function UnresolvablePanel({
  library,
}: {
  library: NonNullable<Status["library"]>;
}) {
  return (
    <Paper withBorder p="md">
      <Group gap="xs" mb="sm">
        <Title order={4}>Unresolvable series</Title>
        <Badge variant="light">{library.unresolvableCount}</Badge>
      </Group>
      {library.unresolvableCount === 0 ? (
        <Text c="dimmed">
          Every series in your library maps to a known provider.
        </Text>
      ) : (
        <>
          <Text size="sm" c="dimmed" mb="sm">
            These series can't be matched to the catalog. Fix the mapping in
            Codex (the source of truth); Shisho never patches it locally.
          </Text>
          <List spacing="xs">
            {library.unresolvable.map((u) => (
              <List.Item key={u.seriesId}>
                <Group gap="xs">
                  {u.codexUrl ? (
                    <Anchor href={u.codexUrl} target="_blank" fw={500}>
                      {u.title}
                    </Anchor>
                  ) : (
                    <Text fw={500}>{u.title}</Text>
                  )}
                  <Code>{u.rawSources.join(", ")}</Code>
                </Group>
              </List.Item>
            ))}
          </List>
        </>
      )}
    </Paper>
  );
}

function CodexKeyPanel({ mintedKey }: { mintedKey: Status["mintedKey"] }) {
  const providers = useProviders();

  return (
    <Paper withBorder p="md">
      <Title order={4} mb="sm">
        Codex key
      </Title>
      <Stack gap="sm">
        <Group gap="xs">
          <Badge color={mintedKey.present ? "green" : "gray"} variant="light">
            {mintedKey.present ? "minted" : "none"}
          </Badge>
          {mintedKey.codexUserSub && (
            <Text size="sm" c="dimmed">
              {mintedKey.codexUserSub}
            </Text>
          )}
        </Group>
        <Text size="sm" c="dimmed">
          Regenerating signs you in again to mint a fresh least-privilege key.
        </Text>
        <Group gap="xs">
          {providers.data?.map((p) => (
            <Button
              key={p.name}
              size="xs"
              variant="default"
              component="a"
              href={`/auth/login/${p.name}?regenerate=true&redirect_to=/status`}
            >
              Regenerate via {p.label}
            </Button>
          ))}
          {providers.data?.length === 0 && (
            <Text size="sm" c="dimmed">
              No OIDC provider configured; the key is minted at sign-in.
            </Text>
          )}
        </Group>
      </Stack>
    </Paper>
  );
}
