import {
  Alert,
  Button,
  Center,
  Loader,
  Paper,
  Stack,
  Text,
  Title,
} from "@mantine/core";
import { Navigate } from "@tanstack/react-router";
import { useProviders } from "@/api/auth";
import { useInfo } from "@/api/info";

/// The multi-user login page: a button per configured provider. Each button is
/// a plain link to `/auth/login/:name`, a full-page navigation that hands off
/// to the provider's OAuth redirect. Single-user mode has no login, so this
/// redirects home there.
export function LoginPage() {
  const info = useInfo();
  const providers = useProviders();

  if (info.data && info.data.auth_mode !== "multi_user") {
    return <Navigate to="/" />;
  }

  return (
    <Center h="100vh">
      <Paper withBorder p="xl" w={420}>
        <Stack>
          <Title order={2}>shisho</Title>
          <Text c="dimmed" size="sm">
            Sign in to continue.
          </Text>

          {providers.isLoading && <Loader data-testid="login-loading" />}
          {providers.isError && (
            <Alert color="red">Could not load sign-in options.</Alert>
          )}
          {providers.data?.length === 0 && (
            <Text size="sm" c="dimmed">
              No login providers are configured.
            </Text>
          )}
          {providers.data?.map((provider) => (
            <Button
              key={provider.name}
              component="a"
              href={`/auth/login/${provider.name}`}
              fullWidth
            >
              Continue with {provider.label}
            </Button>
          ))}
        </Stack>
      </Paper>
    </Center>
  );
}
