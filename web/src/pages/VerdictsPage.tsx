import {
  Alert,
  Badge,
  Button,
  Group,
  Image,
  Loader,
  NativeSelect,
  Paper,
  SegmentedControl,
  Select,
  SimpleGrid,
  Stack,
  Table,
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
import {
  ClampedTip,
  LinkChips,
  SummaryTip,
  seriesTitle,
  TierBadge,
} from "@/components/verdictBits";
import { useUiStore } from "@/stores/ui";

const TRISTATE = [
  { value: "", label: "Any" },
  { value: "true", label: "Yes" },
  { value: "false", label: "No" },
];

// Structured reading outcomes; `__other__` reveals a free-text field so the
// vocabulary stays open (the MCP record_feedback tool also writes free text).
const OTHER = "__other__";
const OUTCOME_OPTIONS = [
  { value: "loved", label: "Loved" },
  { value: "liked", label: "Liked" },
  { value: "mixed", label: "Mixed" },
  { value: "dropped", label: "Dropped" },
  { value: "dnf", label: "Did not finish" },
  { value: OTHER, label: "Other…" },
];

/// The append-only recommendation log: every verdict the recommend sessions
/// logged, filterable by tier / verified / retracted / outcome and grouped by
/// session. Recording an outcome appends a feedback row (the log is never
/// mutated in place) and feeds the consolidation loop. Render as cards or a
/// table; the choice persists.
export function VerdictsPage() {
  const [filters, setFilters] = useState<VerdictFilters>({});
  const verdicts = useVerdicts(filters);
  const view = useUiStore((s) => s.verdictsView);
  const setView = useUiStore((s) => s.setVerdictsView);

  return (
    <Stack gap="xl">
      <Group justify="space-between" align="flex-end">
        <Title order={2}>Verdicts</Title>
        <SegmentedControl
          value={view}
          onChange={(v) => setView(v as "card" | "table")}
          data={[
            { value: "card", label: "Cards" },
            { value: "table", label: "Table" },
          ]}
        />
      </Group>

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
          {view === "card" ? (
            <SimpleGrid cols={{ base: 1, xl: 2 }} spacing="sm">
              {group.verdicts.map((v) => (
                <VerdictCard key={v.id} verdict={v} />
              ))}
            </SimpleGrid>
          ) : (
            <VerdictsTable verdicts={group.verdicts} />
          )}
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

// ---- Shared bits, reused by both the card and table views ----

function StatusBadges({ verdict }: { verdict: Verdict }) {
  return (
    <>
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
    </>
  );
}

// Status dropdown + optional reason, appended as a feedback row on Save.
function OutcomeControl({ verdict }: { verdict: Verdict }) {
  const [status, setStatus] = useState<string | null>(null);
  const [custom, setCustom] = useState("");
  const [reason, setReason] = useState("");
  const record = useRecordOutcome();

  const outcome = status === OTHER ? custom.trim() : (status ?? "");
  const canSave = outcome.length > 0;

  const submit = () => {
    if (!canSave) return;
    record.mutate(
      { id: verdict.id, outcome, notes: reason.trim() || undefined },
      {
        onSuccess: () => {
          setStatus(null);
          setCustom("");
          setReason("");
          notifications.show({ message: "Outcome recorded." });
        },
        onError: (err) =>
          notifications.show({ color: "red", message: err.message }),
      },
    );
  };

  return (
    <Group gap="xs" wrap="nowrap" align="flex-end">
      <NativeSelect
        label="Outcome"
        data={[{ value: "", label: "Set…" }, ...OUTCOME_OPTIONS]}
        value={status ?? ""}
        onChange={(e) => setStatus(e.currentTarget.value || null)}
        w={130}
        size="xs"
        aria-label={`Set outcome for verdict ${verdict.id}`}
      />
      {status === OTHER && (
        <TextInput
          label="Custom"
          size="xs"
          w={110}
          value={custom}
          onChange={(e) => setCustom(e.currentTarget.value)}
          aria-label={`Custom outcome for verdict ${verdict.id}`}
        />
      )}
      <TextInput
        label="Reason"
        placeholder="optional"
        size="xs"
        w={140}
        value={reason}
        onChange={(e) => setReason(e.currentTarget.value)}
        aria-label={`Reason for verdict ${verdict.id}`}
      />
      <Button
        size="xs"
        variant="light"
        loading={record.isPending}
        disabled={!canSave}
        onClick={submit}
      >
        Save
      </Button>
    </Group>
  );
}

// ---- Card view ----

function VerdictCard({ verdict }: { verdict: Verdict }) {
  return (
    <Paper withBorder p="md" bg={verdict.retracted ? "dark.6" : undefined}>
      <Group align="flex-start" gap="md" wrap="nowrap">
        {verdict.coverUrl && (
          <Image
            src={verdict.coverUrl}
            w={120}
            h={170}
            radius="sm"
            fit="cover"
            alt=""
          />
        )}
        <Stack gap="xs" style={{ flex: 1, minWidth: 0 }}>
          <Group gap="xs" wrap="wrap" align="center">
            <TierBadge tier={verdict.tier} />
            <Text fw={600} size="md">
              {seriesTitle(verdict)}
            </Text>
          </Group>

          <Group gap="xs">
            {verdict.seriesId && (
              <Text size="xs" c="dimmed">
                {verdict.seriesId}
              </Text>
            )}
            <StatusBadges verdict={verdict} />
          </Group>

          <LinkChips verdict={verdict} />

          {/* The series description, inline now that the cards are large. */}
          {verdict.summary && (
            <ClampedTip text={verdict.summary} lines={3} size="xs" dimmed />
          )}

          {/* The recommendation reason: the substance of the verdict. */}
          <ClampedTip text={verdict.rationale} lines={4} />
          {verdict.notes && (
            <Text size="xs" c="dimmed">
              {verdict.notes}
            </Text>
          )}

          {!verdict.retracted && verdict.seriesId && (
            <OutcomeControl verdict={verdict} />
          )}
        </Stack>
      </Group>
    </Paper>
  );
}

// ---- Table view ----

function VerdictsTable({ verdicts }: { verdicts: Verdict[] }) {
  return (
    <Table.ScrollContainer minWidth={720}>
      <Table verticalSpacing="sm" highlightOnHover>
        <Table.Thead>
          <Table.Tr>
            <Table.Th w={44} />
            <Table.Th>Series</Table.Th>
            <Table.Th w={60}>Tier</Table.Th>
            <Table.Th>Status</Table.Th>
            <Table.Th>Links</Table.Th>
            <Table.Th>Reason</Table.Th>
            <Table.Th>Outcome</Table.Th>
          </Table.Tr>
        </Table.Thead>
        <Table.Tbody>
          {verdicts.map((v) => (
            <Table.Tr key={v.id} opacity={v.retracted ? 0.6 : 1}>
              <Table.Td>
                {v.coverUrl && (
                  <Image
                    src={v.coverUrl}
                    w={36}
                    h={51}
                    radius="sm"
                    fit="cover"
                    alt=""
                  />
                )}
              </Table.Td>
              <Table.Td>
                <Group gap={6}>
                  <SummaryTip summary={v.summary} />
                  <Text fw={500} size="sm">
                    {seriesTitle(v)}
                  </Text>
                </Group>
                {v.seriesId && (
                  <Text size="xs" c="dimmed">
                    {v.seriesId}
                  </Text>
                )}
              </Table.Td>
              <Table.Td>
                <TierBadge tier={v.tier} />
              </Table.Td>
              <Table.Td>
                <Group gap={4}>
                  <StatusBadges verdict={v} />
                </Group>
              </Table.Td>
              <Table.Td>
                <LinkChips verdict={v} />
              </Table.Td>
              <Table.Td maw={280}>
                <ClampedTip text={v.rationale} lines={1} />
              </Table.Td>
              <Table.Td>
                {!v.retracted && v.seriesId && <OutcomeControl verdict={v} />}
              </Table.Td>
            </Table.Tr>
          ))}
        </Table.Tbody>
      </Table>
    </Table.ScrollContainer>
  );
}
