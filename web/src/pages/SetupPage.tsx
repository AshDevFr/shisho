import {
  Alert,
  Button,
  Card,
  Code,
  CopyButton,
  Group,
  Loader,
  Stack,
  Text,
  Title,
} from "@mantine/core";
import { useGenerateSetupKey, useSetupState } from "@/api/setup";
import { useAuthStore } from "@/stores/auth";

/// Read the one-shot setup token from the URL (?token=...). The wizard is
/// reachable only via the tokenized link printed at first `shisho serve`.
function tokenFromUrl(): string | null {
  if (typeof window === "undefined") return null;
  return new URLSearchParams(window.location.search).get("token");
}

/// The provider TOML the operator adds to switch to multi-user mode. The wizard
/// only *shows* this; the config file stays the source of truth.
const PROVIDER_TOML = `[auth]
mode = "multi_user"

[[auth.providers]]
name = "google"
kind = "oidc"
client_id = "<your-client-id>"
client_secret = "env:GOOGLE_CLIENT_SECRET"
issuer = "https://accounts.google.com"`;

/// First-run wizard. Single-user: generate and reveal an admin key, which logs
/// this browser in. Multi-user: show the provider config to add.
export function SetupPage() {
  const token = tokenFromUrl();
  const { data, isPending, isError, error } = useSetupState(token);
  const generate = useGenerateSetupKey(token);
  const setApiKey = useAuthStore((s) => s.setApiKey);

  if (!token) {
    return (
      <Centered>
        <Alert color="red" title="Missing setup token">
          Open the <Code>/setup?token=…</Code> link printed in the server log on
          first run.
        </Alert>
      </Centered>
    );
  }

  if (isPending) {
    return (
      <Centered>
        <Loader />
      </Centered>
    );
  }

  if (isError) {
    return (
      <Centered>
        <Alert color="red" title="Setup is not available">
          {error.message}. This instance may already be configured, or the token
          is wrong.
        </Alert>
      </Centered>
    );
  }

  const key = generate.data?.key;

  return (
    <Centered>
      <Card withBorder w={560} padding="xl">
        <Stack>
          <Title order={2}>Welcome</Title>
          <Text c="dimmed" size="sm">
            Finish setting up this instance. This page is available only once,
            on first run.
          </Text>

          {data.authMode === "multi_user" ? (
            <Stack gap="xs">
              <Text size="sm">
                This instance runs in multi-user mode. Add at least one identity
                provider to your config and restart:
              </Text>
              <Code block>{PROVIDER_TOML}</Code>
            </Stack>
          ) : key ? (
            <Stack gap="xs">
              <Alert color="teal" title="Your admin API key">
                <Text size="sm" mb="xs">
                  Copy it now: it is shown only once.
                </Text>
                <Group>
                  <Code style={{ wordBreak: "break-all" }}>{key}</Code>
                  <CopyButton value={key}>
                    {({ copied, copy }) => (
                      <Button size="compact-sm" onClick={copy}>
                        {copied ? "Copied" : "Copy"}
                      </Button>
                    )}
                  </CopyButton>
                </Group>
              </Alert>
              <Button onClick={() => setApiKey(key)}>Continue</Button>
            </Stack>
          ) : (
            <Stack gap="xs">
              <Text size="sm">
                Generate an admin API key to get started. You can manage and
                revoke keys later in Settings.
              </Text>
              {generate.isError && (
                <Alert color="red" title="Could not generate a key">
                  {generate.error.message}
                </Alert>
              )}
              <Button
                onClick={() => generate.mutate()}
                loading={generate.isPending}
              >
                Generate admin key
              </Button>
            </Stack>
          )}
        </Stack>
      </Card>
    </Centered>
  );
}

function Centered({ children }: { children: React.ReactNode }) {
  return (
    <Group justify="center" align="center" mih="100vh" p="md">
      {children}
    </Group>
  );
}
