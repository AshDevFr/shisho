import {
  Avatar,
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
  type AdminUser,
  useActivateUser,
  useDeactivateUser,
  useUsers,
} from "@/api/admin";

function formatDate(iso: string | null | undefined) {
  return iso ? new Date(iso).toLocaleDateString() : "never";
}

/// Admin user-management table. `currentUserId` is the signed-in admin, who
/// cannot deactivate themselves (the backend rejects it too).
export function UsersPanel({ currentUserId }: { currentUserId?: string }) {
  const { data: users, isPending, isError } = useUsers(true);
  const deactivate = useDeactivateUser();
  const activate = useActivateUser();
  const [confirmTarget, setConfirmTarget] = useState<AdminUser | null>(null);

  const confirmDeactivate = () => {
    if (!confirmTarget) return;
    deactivate.mutate(confirmTarget.id, {
      onSuccess: () => setConfirmTarget(null),
      onError: (err) =>
        notifications.show({
          color: "red",
          title: "Could not deactivate the user",
          message: err.message,
        }),
    });
  };

  return (
    <Stack>
      {isPending && <Loader size="sm" />}
      {isError && <Text c="red">Could not load users.</Text>}
      {users && (
        <Table verticalSpacing="sm">
          <Table.Thead>
            <Table.Tr>
              <Table.Th>User</Table.Th>
              <Table.Th>Provider</Table.Th>
              <Table.Th>Last seen</Table.Th>
              <Table.Th>Status</Table.Th>
              <Table.Th />
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {users.map((user) => {
              const isSelf = user.id === currentUserId;
              return (
                <Table.Tr key={user.id}>
                  <Table.Td>
                    <Group gap="sm">
                      <Avatar src={user.avatarUrl} radius="xl" size="sm">
                        {(user.displayName ?? user.email ?? "?")
                          .slice(0, 1)
                          .toUpperCase()}
                      </Avatar>
                      <Stack gap={0}>
                        <Text size="sm">{user.displayName ?? "(no name)"}</Text>
                        <Text size="xs" c="dimmed">
                          {user.email ?? "(no email)"}
                          {user.isAdmin && " · admin"}
                        </Text>
                      </Stack>
                    </Group>
                  </Table.Td>
                  <Table.Td>{user.provider ?? "-"}</Table.Td>
                  <Table.Td>{formatDate(user.lastSeen)}</Table.Td>
                  <Table.Td>
                    {user.deactivatedAt ? (
                      <Badge color="red" variant="light">
                        deactivated
                      </Badge>
                    ) : (
                      <Badge color="green" variant="light">
                        active
                      </Badge>
                    )}
                  </Table.Td>
                  <Table.Td>
                    <Group gap="xs" justify="flex-end">
                      {user.deactivatedAt ? (
                        <Button
                          variant="subtle"
                          color="green"
                          size="compact-sm"
                          loading={
                            activate.isPending && activate.variables === user.id
                          }
                          onClick={() => activate.mutate(user.id)}
                        >
                          Reactivate
                        </Button>
                      ) : (
                        <Button
                          variant="subtle"
                          color="red"
                          size="compact-sm"
                          disabled={isSelf}
                          title={
                            isSelf
                              ? "You cannot deactivate your own account"
                              : undefined
                          }
                          onClick={() => setConfirmTarget(user)}
                        >
                          Deactivate
                        </Button>
                      )}
                    </Group>
                  </Table.Td>
                </Table.Tr>
              );
            })}
          </Table.Tbody>
        </Table>
      )}

      <Modal
        opened={confirmTarget !== null}
        onClose={() => setConfirmTarget(null)}
        title="Deactivate user"
      >
        <Stack>
          <Text size="sm">
            Deactivate{" "}
            <Text span fw={600}>
              {confirmTarget?.email ?? confirmTarget?.id}
            </Text>
            ? Their sessions and API keys stop working immediately. You can
            reactivate them later.
          </Text>
          <Group justify="flex-end">
            <Button variant="default" onClick={() => setConfirmTarget(null)}>
              Cancel
            </Button>
            <Button
              color="red"
              loading={deactivate.isPending}
              onClick={confirmDeactivate}
            >
              Deactivate
            </Button>
          </Group>
        </Stack>
      </Modal>
    </Stack>
  );
}
