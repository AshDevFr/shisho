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
import {
  type Connection,
  useConnections,
  useRevokeConnection,
} from "@/api/connections";

function formatDate(iso: string | null | undefined) {
  return iso ? new Date(iso).toLocaleDateString() : "—";
}

function appName(conn: Connection) {
  return conn.clientName?.trim() || conn.clientId;
}

/// Connected applications: OAuth clients (an MCP connector and the like)
/// holding a live grant on the user's account. Revoking disconnects the app
/// immediately. This is the "revoke access" the consent screen promises.
export function ConnectedAppsPanel() {
  const { data: connections, isPending, isError } = useConnections();
  const revoke = useRevokeConnection();
  const [revokeTarget, setRevokeTarget] = useState<Connection | null>(null);

  const confirmRevoke = () => {
    if (!revokeTarget) return;
    revoke.mutate(revokeTarget.clientId, {
      onSuccess: () => setRevokeTarget(null),
      onError: (err) =>
        notifications.show({
          color: "red",
          title: "Could not disconnect the application",
          message: err.message,
        }),
    });
  };

  return (
    <Stack>
      <Text c="dimmed" size="sm">
        Applications you have authorized to access your account on your behalf,
        such as an MCP connector. Disconnecting revokes their access
        immediately.
      </Text>

      {isPending && <Loader size="sm" />}
      {isError && <Text c="red">Could not load connected applications.</Text>}
      {connections && connections.length === 0 && (
        <Text c="dimmed">No applications connected.</Text>
      )}
      {connections && connections.length > 0 && (
        <Table>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Application</Table.Th>
              <Table.Th>Permissions</Table.Th>
              <Table.Th>Connected</Table.Th>
              <Table.Th>Expires</Table.Th>
              <Table.Th />
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {connections.map((conn) => (
              <Table.Tr key={conn.clientId}>
                <Table.Td>{appName(conn)}</Table.Td>
                <Table.Td>
                  <Group gap="xs">
                    {conn.scopes.map((scope) => (
                      <Badge key={scope} variant="light">
                        {scope}
                      </Badge>
                    ))}
                  </Group>
                </Table.Td>
                <Table.Td>{formatDate(conn.connectedAt)}</Table.Td>
                <Table.Td>{formatDate(conn.expiresAt)}</Table.Td>
                <Table.Td>
                  <Button
                    variant="subtle"
                    color="red"
                    size="compact-sm"
                    onClick={() => setRevokeTarget(conn)}
                  >
                    Disconnect
                  </Button>
                </Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </Table>
      )}

      <Modal
        opened={revokeTarget !== null}
        onClose={() => setRevokeTarget(null)}
        title="Disconnect application"
        transitionProps={{ duration: 0 }}
      >
        <Stack>
          <Text>
            Disconnect "{revokeTarget && appName(revokeTarget)}"? It will lose
            access to your account immediately and must be re-authorized to
            reconnect.
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
              Disconnect
            </Button>
          </Group>
        </Stack>
      </Modal>
    </Stack>
  );
}
