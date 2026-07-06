import {
  Alert,
  Button,
  Checkbox,
  Code,
  Group,
  Modal,
  Select,
  Stack,
  Text,
  TextInput,
} from "@mantine/core";
import { useClipboard } from "@mantine/hooks";
import { notifications } from "@mantine/notifications";
import { useState } from "react";
import { type CreatedKey, useCreateApiKey } from "@/api/apiKeys";

/// Scope checkboxes grouped by resource. Each value is the exact
/// `resource:action` string the API expects, so the UI never invents a scope
/// the server doesn't know about. Keep this in lockstep with the
/// `Resource`/`Action` taxonomy in `shisho-auth` (the server rejects any scope
/// not in its `VALID_SCOPES`).
const SCOPE_GROUPS: { resource: string; actions: string[] }[] = [
  { resource: "candidates", actions: ["read"] },
  { resource: "lookup", actions: ["read"] },
  { resource: "library", actions: ["read"] },
  { resource: "verdicts", actions: ["read", "write"] },
  { resource: "profile", actions: ["read", "write"] },
  { resource: "status", actions: ["read", "write"] },
  { resource: "keys", actions: ["manage"] },
];

/// A read-only default (least privilege): the caller adds write scopes
/// deliberately. The server requires at least one scope, so this is never empty.
const DEFAULT_SCOPES = [
  "candidates:read",
  "lookup:read",
  "library:read",
  "verdicts:read",
  "profile:read",
  "status:read",
];

const EXPIRY_OPTIONS = [
  { value: "never", label: "Never" },
  { value: "30", label: "30 days" },
  { value: "90", label: "90 days" },
  { value: "365", label: "1 year" },
];

/// Turn an expiry selection into an ISO 8601 instant (or undefined for
/// "never"). Computed client-side; the server stores it verbatim.
function expiresAtFrom(selection: string): string | undefined {
  if (selection === "never") return undefined;
  const days = Number(selection);
  return new Date(Date.now() + days * 24 * 60 * 60 * 1000).toISOString();
}

interface CreateKeyModalProps {
  opened: boolean;
  onClose: () => void;
}

/// Create-key form that swaps to a one-time secret reveal on success. The
/// plaintext key lives only in `createdKey` local state; closing the modal
/// discards it and resets the mutation so no cache retains it.
export function CreateKeyModal({ opened, onClose }: CreateKeyModalProps) {
  const create = useCreateApiKey();
  const clipboard = useClipboard();
  const [name, setName] = useState("");
  const [scopes, setScopes] = useState<string[]>(DEFAULT_SCOPES);
  const [expiry, setExpiry] = useState("never");
  const [nameError, setNameError] = useState<string | null>(null);
  const [createdKey, setCreatedKey] = useState<CreatedKey | null>(null);

  const close = () => {
    setName("");
    setScopes(DEFAULT_SCOPES);
    setExpiry("never");
    setNameError(null);
    setCreatedKey(null);
    create.reset();
    onClose();
  };

  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    if (name.trim() === "") {
      setNameError("Name is required");
      return;
    }
    create.mutate(
      { name: name.trim(), scopes, expiresAt: expiresAtFrom(expiry) },
      {
        onSuccess: setCreatedKey,
        onError: (err) =>
          notifications.show({
            color: "red",
            title: "Could not create the key",
            message: err.message,
          }),
      },
    );
  };

  return (
    <Modal
      opened={opened}
      onClose={close}
      title={createdKey ? "API key created" : "Create API key"}
      transitionProps={{ duration: 0 }}
    >
      {createdKey ? (
        <Stack>
          <Alert color="yellow" title="One-time secret">
            Store this key now: it will not be shown again.
          </Alert>
          <Code block data-testid="created-key-secret">
            {createdKey.key}
          </Code>
          <Group justify="flex-end">
            <Button
              variant="default"
              onClick={() => clipboard.copy(createdKey.key)}
            >
              {clipboard.copied ? "Copied" : "Copy"}
            </Button>
            <Button onClick={close}>Done</Button>
          </Group>
        </Stack>
      ) : (
        <form onSubmit={submit}>
          <Stack>
            <TextInput
              label="Name"
              placeholder="laptop-cli"
              required
              value={name}
              error={nameError}
              onChange={(e) => {
                setName(e.currentTarget.value);
                setNameError(null);
              }}
            />
            <Checkbox.Group label="Scopes" value={scopes} onChange={setScopes}>
              <Stack gap="xs" mt="xs">
                {SCOPE_GROUPS.map((group) => (
                  <div key={group.resource}>
                    <Text size="sm" fw={500} tt="capitalize">
                      {group.resource}
                    </Text>
                    <Group gap="md" mt={4}>
                      {group.actions.map((action) => (
                        <Checkbox
                          key={`${group.resource}:${action}`}
                          value={`${group.resource}:${action}`}
                          label={action}
                        />
                      ))}
                    </Group>
                  </div>
                ))}
              </Stack>
            </Checkbox.Group>
            <Select
              label="Expires"
              data={EXPIRY_OPTIONS}
              value={expiry}
              onChange={(value) => setExpiry(value ?? "never")}
              allowDeselect={false}
              comboboxProps={{ withinPortal: false }}
            />
            <Group justify="flex-end">
              <Button variant="default" onClick={close}>
                Cancel
              </Button>
              <Button
                type="submit"
                disabled={name.trim() === "" || scopes.length === 0}
                loading={create.isPending}
              >
                Create
              </Button>
            </Group>
          </Stack>
        </form>
      )}
    </Modal>
  );
}
