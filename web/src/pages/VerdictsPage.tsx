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
  Switch,
  Table,
  Text,
  TextInput,
  Title,
  Tooltip,
} from "@mantine/core";
import { notifications } from "@mantine/notifications";
import { useState } from "react";
import {
  type SessionGroup,
  useRecordOutcome,
  useVerdicts,
  type Verdict,
  type VerdictFilters,
} from "@/api/verdicts";
import { OutcomeMenu } from "@/components/OutcomeMenu";
import {
  ClampedTip,
  LinkChips,
  SummaryTip,
  seriesTitle,
  TierBadge,
} from "@/components/verdictBits";
import { formatWhen } from "@/lib/time";
import { useUiStore } from "@/stores/ui";

const TIER_FILTER = ["S", "A", "B", "C", "D", "pass"];
// Stored outcome values (the server normalizes what it is sent to these).
const OUTCOME_FILTER = [
  { value: "loved", label: "Loved" },
  { value: "liked", label: "Liked" },
  { value: "mixed", label: "Mixed" },
  { value: "dropped", label: "Dropped" },
  { value: "dnf", label: "Did not finish" },
  { value: "not_interested", label: "Not interested" },
  { value: "other", label: "Other" },
];

const STATES = [
  { value: "pending", label: "Pending" },
  { value: "settled", label: "Settled" },
  { value: "all", label: "All" },
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
  { value: "not interested", label: "Not interested" },
  { value: OTHER, label: "Other…" },
];

/// Whether an outcome can be recorded against this row: only a recommendation
/// whose series is still pending. Feedback rows are outcomes already, a pass
/// was never a recommendation, and a retraction withdrew it.
function takesOutcome(v: Verdict): boolean {
  return (
    v.pending &&
    !v.retracted &&
    v.outcome == null &&
    v.seriesId != null &&
    v.tier.toLowerCase() !== "pass"
  );
}

/// History: the append-only recommendation log, every verdict the recommend
/// sessions logged and every outcome since. It opens on what is still
/// pending (the series Home lists); Settled holds the ones that ended, each
/// row carrying its series' outcome, so the feedback rows that recorded those
/// outcomes stay hidden unless asked for. Cards group by session; the table
/// is one list with a session column. Recording an outcome appends a feedback
/// row (the log is never mutated in place).
export function VerdictsPage() {
  const [filters, setFilters] = useState<VerdictFilters>({
    state: "pending",
    feedback: false,
  });
  const verdicts = useVerdicts(filters);
  const view = useUiStore((s) => s.verdictsView);
  const setView = useUiStore((s) => s.setVerdictsView);

  return (
    <Stack gap="xl">
      <Group justify="space-between" align="flex-end">
        <Title order={2}>History</Title>
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
        <Text c="dimmed">
          {filters.state === "pending" && !filters.tier
            ? "Nothing pending: every recommendation has an outcome."
            : "Nothing matches these filters."}
        </Text>
      )}

      {verdicts.data &&
        verdicts.data.length > 0 &&
        (view === "card" ? (
          verdicts.data.map((group) => (
            <Paper key={group.sessionId} withBorder p="md">
              <Group gap="xs" mb="sm">
                <Title order={5}>Session</Title>
                <Badge variant="light">{group.sessionId}</Badge>
                <Text size="xs" c="dimmed">
                  {formatWhen(group.startedAt)}
                </Text>
              </Group>
              <SimpleGrid cols={{ base: 1, xl: 2 }} spacing="sm">
                {group.verdicts.map((v) => (
                  <VerdictCard key={v.id} verdict={v} />
                ))}
              </SimpleGrid>
            </Paper>
          ))
        ) : (
          <VerdictsTable groups={verdicts.data} />
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
  // A pending series has no outcome, so the Outcome filter only applies to
  // the other two states.
  const setState = (v: string) => {
    const state = v === "all" ? undefined : (v as VerdictFilters["state"]);
    onChange({
      ...value,
      state,
      outcome: state === "pending" ? undefined : value.outcome,
    });
  };

  return (
    <Group align="flex-end" wrap="wrap">
      <SegmentedControl
        aria-label="Show"
        value={value.state ?? "all"}
        onChange={setState}
        data={STATES}
      />
      <Select
        label="Tier"
        placeholder="Any"
        data={TIER_FILTER}
        value={value.tier ?? null}
        onChange={(s) => onChange({ ...value, tier: s ?? undefined })}
        clearable
        w={110}
      />
      {value.state !== "pending" && (
        <Select
          label="Outcome"
          placeholder="Any"
          data={OUTCOME_FILTER}
          value={value.outcome ?? null}
          onChange={(s) => onChange({ ...value, outcome: s ?? undefined })}
          clearable
          w={170}
        />
      )}
      <Switch
        label="Show feedback rows"
        checked={value.feedback !== false}
        onChange={(e) =>
          onChange({
            ...value,
            feedback: e.currentTarget.checked ? undefined : false,
          })
        }
        mb={8}
      />
    </Group>
  );
}

// ---- Shared bits, reused by both the card and table views ----

/// The series title, with its canonical id on hover (the id is for
/// cross-checking with the MCP tools, not for reading).
function SeriesTitle({ verdict, size }: { verdict: Verdict; size: string }) {
  return (
    <Tooltip label={verdict.seriesId} disabled={!verdict.seriesId} withArrow>
      <Text fw={600} size={size} style={{ cursor: "default" }}>
        {seriesTitle(verdict)}
      </Text>
    </Tooltip>
  );
}

function StatusBadges({ verdict }: { verdict: Verdict }) {
  return (
    <>
      {verdict.verified && (
        <Badge color="green" variant="light" size="sm">
          verified
        </Badge>
      )}
      {verdict.retracted && (
        <Tooltip
          label={verdict.retractionReason}
          disabled={!verdict.retractionReason}
          withArrow
        >
          <Badge color="red" variant="light" size="sm">
            retracted
          </Badge>
        </Tooltip>
      )}
      {(verdict.outcome ?? verdict.seriesOutcome) && (
        <Badge color="grape" variant="light" size="sm">
          outcome: {verdict.outcome ?? verdict.seriesOutcome}
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
    <Group gap="xs" wrap="wrap" align="flex-end">
      <NativeSelect
        label="Outcome"
        data={[{ value: "", label: "Set…" }, ...OUTCOME_OPTIONS]}
        value={status ?? ""}
        onChange={(e) => setStatus(e.currentTarget.value || null)}
        w={140}
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
  // A retracted card is faded with a dashed border: readable in both colour
  // schemes, and still visibly not a recommendation.
  return (
    <Paper
      withBorder
      p="md"
      opacity={verdict.retracted ? 0.65 : 1}
      style={verdict.retracted ? { borderStyle: "dashed" } : undefined}
    >
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
            <SeriesTitle verdict={verdict} size="md" />
          </Group>

          <Group gap="xs">
            <StatusBadges verdict={verdict} />
          </Group>

          <LinkChips verdict={verdict} />

          {verdict.summary && (
            <ClampedTip text={verdict.summary} lines={3} size="xs" dimmed />
          )}

          {/* The recommendation reason: the substance of the verdict. */}
          {verdict.rationale && (
            <ClampedTip text={verdict.rationale} lines={4} />
          )}
          {verdict.notes && (
            <Text size="xs" c="dimmed">
              {verdict.notes}
            </Text>
          )}

          {takesOutcome(verdict) && <OutcomeControl verdict={verdict} />}
        </Stack>
      </Group>
    </Paper>
  );
}

// ---- Table view ----

/// One table across sessions (most recent first, as the groups arrive), so
/// the columns line up; the session is a column rather than a container.
function VerdictsTable({ groups }: { groups: SessionGroup[] }) {
  const rows = groups.flatMap((g) =>
    g.verdicts.map((v) => ({ verdict: v, session: g })),
  );
  return (
    <Table.ScrollContainer minWidth={960}>
      <Table verticalSpacing="sm" highlightOnHover>
        <Table.Thead>
          <Table.Tr>
            <Table.Th w={52} />
            <Table.Th>Series</Table.Th>
            <Table.Th>Tier</Table.Th>
            <Table.Th>Status</Table.Th>
            <Table.Th>Reason</Table.Th>
            <Table.Th>Session</Table.Th>
            <Table.Th>Outcome</Table.Th>
          </Table.Tr>
        </Table.Thead>
        <Table.Tbody>
          {rows.map(({ verdict: v, session }) => (
            <Table.Tr key={v.id} opacity={v.retracted ? 0.65 : 1}>
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
              <Table.Td miw={200}>
                <SeriesTitle verdict={v} size="sm" />
                <Group gap={8} wrap="wrap">
                  <SummaryTip summary={v.summary} />
                  <LinkChips verdict={v} />
                </Group>
              </Table.Td>
              <Table.Td style={{ whiteSpace: "nowrap" }}>
                <TierBadge tier={v.tier} />
              </Table.Td>
              <Table.Td>
                <Group gap={4} wrap="wrap" miw={90}>
                  <StatusBadges verdict={v} />
                </Group>
              </Table.Td>
              <Table.Td miw={240} maw={420}>
                {v.rationale && <ClampedTip text={v.rationale} lines={2} />}
                {v.notes && (
                  <Text size="xs" c="dimmed" lineClamp={2}>
                    {v.notes}
                  </Text>
                )}
              </Table.Td>
              <Table.Td style={{ whiteSpace: "nowrap" }}>
                <Text size="xs">{session.sessionId}</Text>
                <Text size="xs" c="dimmed">
                  {new Date(v.createdAt).toLocaleDateString()}
                </Text>
              </Table.Td>
              <Table.Td>
                {takesOutcome(v) && (
                  <OutcomeMenu verdictId={v.id} title={seriesTitle(v)} />
                )}
              </Table.Td>
            </Table.Tr>
          ))}
        </Table.Tbody>
      </Table>
    </Table.ScrollContainer>
  );
}
