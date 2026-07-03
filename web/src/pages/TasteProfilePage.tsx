import {
  Accordion,
  Alert,
  Badge,
  Button,
  Code,
  Group,
  Loader,
  Paper,
  Stack,
  Text,
  Textarea,
  Title,
} from "@mantine/core";
import { notifications } from "@mantine/notifications";
import { useEffect, useState } from "react";
import {
  useApproveProposal,
  useEditProfile,
  useProfileVersions,
  useRejectProposal,
  useTasteProfile,
} from "@/api/tasteProfile";

/// The taste profile: the standing description of what the user loves, likes,
/// dislikes, and is allergic to. Read the current version, review and
/// approve/reject the pending consolidation proposal, browse the version
/// history with diffs, and edit directly. Available in both auth modes: this is
/// the primary user's own recommendation data.
export function TasteProfilePage() {
  const profile = useTasteProfile();

  return (
    <Stack gap="xl">
      <Title order={2}>Taste profile</Title>

      {profile.isLoading && <Loader />}

      {profile.error && (
        <Alert color="yellow" title="Taste profile unavailable">
          {profile.error.message}
        </Alert>
      )}

      {profile.data && (
        <>
          {profile.data.pendingProposal && (
            <PendingProposalReview proposal={profile.data.pendingProposal} />
          )}
          <CurrentProfile
            content={profile.data.content}
            version={profile.data.version}
            updatedAt={profile.data.updatedAt}
          />
          <VersionHistory />
        </>
      )}
    </Stack>
  );
}

function CurrentProfile({
  content,
  version,
  updatedAt,
}: {
  content: string;
  version: number;
  updatedAt?: string;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(content);
  const edit = useEditProfile();

  // Re-seed the editor when the underlying profile changes (e.g. after an
  // approval) unless the user is mid-edit.
  useEffect(() => {
    if (!editing) setDraft(content);
  }, [content, editing]);

  const save = () => {
    edit.mutate(
      { content: draft, baseVersion: version },
      {
        onSuccess: () => {
          setEditing(false);
          notifications.show({ message: "Profile updated." });
        },
        onError: (err) =>
          notifications.show({ color: "red", message: err.message }),
      },
    );
  };

  return (
    <Paper withBorder p="md">
      <Group justify="space-between" mb="sm">
        <Group gap="xs">
          <Title order={4}>Current</Title>
          <Badge variant="light">v{version}</Badge>
          {updatedAt && (
            <Text size="xs" c="dimmed">
              updated {new Date(updatedAt).toLocaleString()}
            </Text>
          )}
        </Group>
        {!editing && (
          <Button variant="light" size="xs" onClick={() => setEditing(true)}>
            Edit
          </Button>
        )}
      </Group>

      {editing ? (
        <Stack gap="sm">
          <Textarea
            value={draft}
            onChange={(e) => setDraft(e.currentTarget.value)}
            autosize
            minRows={8}
            aria-label="Profile content"
          />
          <Group>
            <Button onClick={save} loading={edit.isPending}>
              Save as new version
            </Button>
            <Button
              variant="subtle"
              onClick={() => {
                setDraft(content);
                setEditing(false);
              }}
            >
              Cancel
            </Button>
          </Group>
        </Stack>
      ) : content ? (
        <Code block>{content}</Code>
      ) : (
        <Text c="dimmed">
          No taste profile yet. Run the consolidate prompt, or edit directly to
          create the first version.
        </Text>
      )}
    </Paper>
  );
}

function PendingProposalReview({
  proposal,
}: {
  proposal: NonNullable<
    ReturnType<typeof useTasteProfile>["data"]
  >["pendingProposal"];
}) {
  const approve = useApproveProposal();
  const reject = useRejectProposal();
  if (!proposal) return null;
  const busy = approve.isPending || reject.isPending;

  return (
    <Alert color="blue" title="Pending proposal" variant="light">
      <Stack gap="sm">
        <Text size="sm">{proposal.changeSummary}</Text>
        <Code block>{proposal.diff}</Code>
        <Group>
          <Button
            loading={approve.isPending}
            disabled={busy}
            onClick={() =>
              approve.mutate(proposal.version, {
                onSuccess: () =>
                  notifications.show({ message: "Proposal applied." }),
                onError: (err) =>
                  notifications.show({ color: "red", message: err.message }),
              })
            }
          >
            Approve
          </Button>
          <Button
            variant="default"
            loading={reject.isPending}
            disabled={busy}
            onClick={() =>
              reject.mutate(proposal.version, {
                onSuccess: () =>
                  notifications.show({ message: "Proposal rejected." }),
                onError: (err) =>
                  notifications.show({ color: "red", message: err.message }),
              })
            }
          >
            Reject
          </Button>
        </Group>
      </Stack>
    </Alert>
  );
}

function VersionHistory() {
  const versions = useProfileVersions();
  if (!versions.data || versions.data.length === 0) return null;

  return (
    <Paper withBorder p="md">
      <Title order={4} mb="sm">
        History
      </Title>
      <Accordion variant="separated">
        {versions.data.map((v) => (
          <Accordion.Item key={v.version} value={String(v.version)}>
            <Accordion.Control>
              <Group gap="xs">
                <Badge variant="light">v{v.version}</Badge>
                <StatusBadge status={v.status} />
                <Text size="sm">{v.changeSummary}</Text>
              </Group>
            </Accordion.Control>
            <Accordion.Panel>
              <Code block>{v.diff}</Code>
            </Accordion.Panel>
          </Accordion.Item>
        ))}
      </Accordion>
    </Paper>
  );
}

function StatusBadge({ status }: { status: string }) {
  const color =
    status === "applied"
      ? "green"
      : status === "pending"
        ? "blue"
        : status === "rejected"
          ? "red"
          : "gray";
  return (
    <Badge color={color} variant="light" size="sm">
      {status}
    </Badge>
  );
}
