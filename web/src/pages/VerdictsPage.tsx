import {
  Alert,
  Badge,
  Button,
  Group,
  Loader,
  Paper,
  Select,
  Stack,
  Text,
  TextInput,
  Title,
} from "@mantine/core";
import { notifications } from "@mantine/notifications";
import { useState } from "react";
import {
  useRecordOutcome,
  useVerdicts,
  type Verdict,
  type VerdictFilters,
} from "@/api/verdicts";

const TRISTATE = [
  { value: "", label: "Any" },
  { value: "true", label: "Yes" },
  { value: "false", label: "No" },
];

/// The append-only recommendation log: every verdict the recommend sessions
/// logged, filterable by tier / verified / retracted / outcome and grouped by
/// session. Recording an outcome appends a feedback row (the log is never
/// mutated in place) and feeds the consolidation loop.
export function VerdictsPage() {
  const [filters, setFilters] = useState<VerdictFilters>({});
  const verdicts = useVerdicts(filters);

  return (
    <Stack gap="xl">
      <Title order={2}>Verdicts</Title>

      <Filters value={filters} onChange={setFilters} />

      {verdicts.isLoading && <Loader />}

      {verdicts.error && (
        <Alert color="yellow" title="Verdict log unavailable">
          {verdicts.error.message}
        </Alert>
      )}

      {verdicts.data && verdicts.data.length === 0 && (
        <Text c="dimmed">No verdicts match these filters.</Text>
      )}

      {verdicts.data?.map((group) => (
        <Paper key={group.sessionId} withBorder p="md">
          <Group gap="xs" mb="sm">
            <Title order={5}>Session</Title>
            <Badge variant="light">{group.sessionId}</Badge>
            <Text size="xs" c="dimmed">
              {new Date(group.startedAt).toLocaleString()}
            </Text>
          </Group>
          <Stack gap="sm">
            {group.verdicts.map((v) => (
              <VerdictRow key={v.id} verdict={v} />
            ))}
          </Stack>
        </Paper>
      ))}
    </Stack>
  );
}

function Filters({
  value,
  onChange,
}: {
  value: VerdictFilters;
  onChange: (f: VerdictFilters) => void;
}) {
  const triValue = (b?: boolean) =>
    b === undefined ? "" : b ? "true" : "false";
  const parseTri = (s: string | null) =>
    s === "true" ? true : s === "false" ? false : undefined;

  return (
    <Group align="flex-end">
      <TextInput
        label="Tier"
        placeholder="e.g. S, A, pass"
        value={value.tier ?? ""}
        onChange={(e) =>
          onChange({ ...value, tier: e.currentTarget.value || undefined })
        }
      />
      <TextInput
        label="Outcome"
        placeholder="e.g. loved"
        value={value.outcome ?? ""}
        onChange={(e) =>
          onChange({ ...value, outcome: e.currentTarget.value || undefined })
        }
      />
      <Select
        label="Verified"
        data={TRISTATE}
        value={triValue(value.verified)}
        onChange={(s) => onChange({ ...value, verified: parseTri(s) })}
        w={110}
      />
      <Select
        label="Retracted"
        data={TRISTATE}
        value={triValue(value.retracted)}
        onChange={(s) => onChange({ ...value, retracted: parseTri(s) })}
        w={110}
      />
      {(value.tier ||
        value.outcome ||
        value.verified !== undefined ||
        value.retracted !== undefined) && (
        <Button variant="subtle" onClick={() => onChange({})}>
          Clear
        </Button>
      )}
    </Group>
  );
}

function VerdictRow({ verdict }: { verdict: Verdict }) {
  return (
    <Paper withBorder p="sm" bg={verdict.retracted ? "dark.6" : undefined}>
      <Group justify="space-between" align="flex-start">
        <Stack gap={4}>
          <Group gap="xs">
            <Badge>{verdict.tier}</Badge>
            {verdict.verified && (
              <Badge color="green" variant="light" size="sm">
                verified
              </Badge>
            )}
            {verdict.retracted && (
              <Badge color="red" variant="light" size="sm">
                retracted
                {verdict.retractionReason && `: ${verdict.retractionReason}`}
              </Badge>
            )}
            {verdict.outcome && (
              <Badge color="grape" variant="light" size="sm">
                outcome: {verdict.outcome}
              </Badge>
            )}
          </Group>
          {verdict.seriesId && (
            <Text size="xs" c="dimmed">
              {verdict.seriesId}
            </Text>
          )}
          <Text size="sm">{verdict.rationale}</Text>
          {verdict.notes && (
            <Text size="xs" c="dimmed">
              {verdict.notes}
            </Text>
          )}
        </Stack>
        {verdict.seriesId && !verdict.retracted && (
          <OutcomeEditor verdict={verdict} />
        )}
      </Group>
    </Paper>
  );
}

function OutcomeEditor({ verdict }: { verdict: Verdict }) {
  const [outcome, setOutcome] = useState("");
  const record = useRecordOutcome();

  const submit = () => {
    if (!outcome.trim()) return;
    record.mutate(
      { id: verdict.id, outcome: outcome.trim() },
      {
        onSuccess: () => {
          setOutcome("");
          notifications.show({ message: "Outcome recorded." });
        },
        onError: (err) =>
          notifications.show({ color: "red", message: err.message }),
      },
    );
  };

  return (
    <Group gap="xs" wrap="nowrap">
      <TextInput
        placeholder="Set outcome…"
        size="xs"
        value={outcome}
        onChange={(e) => setOutcome(e.currentTarget.value)}
        aria-label={`Set outcome for verdict ${verdict.id}`}
      />
      <Button
        size="xs"
        variant="light"
        loading={record.isPending}
        onClick={submit}
      >
        Save
      </Button>
    </Group>
  );
}
