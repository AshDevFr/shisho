import {
  Badge,
  Button,
  Group,
  Loader,
  Modal,
  Stack,
  Table,
  Text,
} from "@mantine/core";
import { notifications } from "@mantine/notifications";
import { useState } from "react";
import { type ApiKey, useApiKeys, useRevokeApiKey } from "@/api/apiKeys";
import { CreateKeyModal } from "@/components/settings/CreateKeyModal";

function formatDate(iso: string | null | undefined) {
  return iso ? new Date(iso).toLocaleDateString() : "never";
}

export function ApiKeysPanel() {
  const { data: keys, isPending, isError } = useApiKeys();
  const revoke = useRevokeApiKey();
  const [createOpen, setCreateOpen] = useState(false);
  const [revokeTarget, setRevokeTarget] = useState<ApiKey | null>(null);

  const confirmRevoke = () => {
    if (!revokeTarget) return;
    revoke.mutate(revokeTarget.id, {
      onSuccess: () => setRevokeTarget(null),
      onError: (err) =>
        notifications.show({
          color: "red",
          title: "Could not revoke the key",
          message: err.message,
        }),
    });
  };

  return (
    <Stack>
      <Group justify="flex-end">
        <Button onClick={() => setCreateOpen(true)}>Create key</Button>
      </Group>

      {isPending && <Loader size="sm" />}
      {isError && <Text c="red">Could not load API keys.</Text>}
      {keys && keys.length === 0 && <Text c="dimmed">No API keys yet.</Text>}
      {keys && keys.length > 0 && (
        <Table>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Name</Table.Th>
              <Table.Th>Scopes</Table.Th>
              <Table.Th>Created</Table.Th>
              <Table.Th>Last used</Table.Th>
              <Table.Th>Expires</Table.Th>
              <Table.Th />
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {keys.map((key) => (
              <Table.Tr key={key.id}>
                <Table.Td>{key.name}</Table.Td>
                <Table.Td>
                  <Group gap="xs">
                    {key.scopes.map((scope) => (
                      <Badge key={scope} variant="light">
                        {scope}
                      </Badge>
                    ))}
                  </Group>
                </Table.Td>
                <Table.Td>{formatDate(key.createdAt)}</Table.Td>
                <Table.Td>{formatDate(key.lastUsed)}</Table.Td>
                <Table.Td>{formatDate(key.expiresAt)}</Table.Td>
                <Table.Td>
                  <Button
                    variant="subtle"
                    color="red"
                    size="compact-sm"
                    onClick={() => setRevokeTarget(key)}
                  >
                    Revoke
                  </Button>
                </Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </Table>
      )}

      <CreateKeyModal
        opened={createOpen}
        onClose={() => setCreateOpen(false)}
      />

      <Modal
        opened={revokeTarget !== null}
        onClose={() => setRevokeTarget(null)}
        title="Revoke API key"
        transitionProps={{ duration: 0 }}
      >
        <Stack>
          <Text>
            Revoke the key "{revokeTarget?.name}"? Tools using it will lose
            access immediately.
          </Text>
          <Group justify="flex-end">
            <Button variant="default" onClick={() => setRevokeTarget(null)}>
              Cancel
            </Button>
            <Button
              color="red"
              onClick={confirmRevoke}
              loading={revoke.isPending}
            >
              Revoke key
            </Button>
          </Group>
        </Stack>
      </Modal>
    </Stack>
  );
}
