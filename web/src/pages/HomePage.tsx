import {
  Alert,
  Badge,
  Button,
  Group,
  Image,
  Loader,
  Paper,
  SegmentedControl,
  SimpleGrid,
  Stack,
  Text,
  Title,
} from "@mantine/core";
import { notifications } from "@mantine/notifications";
import {
  type Recommendation,
  useRecommendations,
  useSyncRecommendations,
} from "@/api/recommendations";
import { NotInterestedButton, OutcomeMenu } from "@/components/OutcomeMenu";
import {
  ClampedTip,
  LinkChips,
  seriesTitle,
  TierBadge,
} from "@/components/verdictBits";
import { timeAgo } from "@/lib/time";
import { type HomeView, useUiStore } from "@/stores/ui";

/// Read next: the caller's standing recommendations, one per series across
/// every session, re-evaluated against the log (outcomes, passes and
/// retractions drop off) and the live library (started series wait under
/// Reading now). Cards or a compact list; the choice persists.
export function HomePage() {
  const recs = useRecommendations();
  const sync = useSyncRecommendations();
  const view = useUiStore((s) => s.homeView);
  const setView = useUiStore((s) => s.setHomeView);
  const data = recs.data;

  const runSync = () =>
    sync.mutate(undefined, {
      onSuccess: (r) =>
        notifications.show({
          message:
            r.outcomesInferred > 0
              ? `Synced with Codex: ${r.outcomesInferred} outcome(s) picked up.`
              : "Synced with Codex: nothing new.",
        }),
      onError: (err) =>
        notifications.show({ color: "red", message: err.message }),
    });

  return (
    <Stack gap="lg">
      <Group justify="space-between" align="flex-end" wrap="wrap">
        <div>
          <Title order={2}>Read next</Title>
          {data && (
            <Text size="sm" c="dimmed">
              {data.recommended.length} recommendation
              {data.recommended.length === 1 ? "" : "s"}
              {data.libraryAsOf &&
                ` · library as of ${timeAgo(data.libraryAsOf)}`}
            </Text>
          )}
        </div>
        <Group gap="sm">
          <SegmentedControl
            value={view}
            onChange={(v) => setView(v as HomeView)}
            data={[
              { value: "card", label: "Cards" },
              { value: "list", label: "List" },
            ]}
          />
          <Button variant="light" loading={sync.isPending} onClick={runSync}>
            Sync with Codex
          </Button>
        </Group>
      </Group>

      {recs.isLoading && <Loader />}

      {recs.error && (
        <Alert color="yellow" title="Recommendations unavailable">
          {recs.error.message}
        </Alert>
      )}

      {data?.libraryGap && (
        <Alert color="yellow" title="Library not checked">
          {data.libraryGap}. Everything still standing in your log is listed,
          including series you may have started.
        </Alert>
      )}

      {data && data.recommended.length === 0 && data.reading.length === 0 && (
        <Text c="dimmed">
          Nothing to read next. Ask Claude for recommendations; they show up
          here until you read them or record an outcome.
        </Text>
      )}

      {data &&
        data.recommended.length > 0 &&
        (view === "card" ? (
          <SimpleGrid cols={{ base: 1, lg: 2 }} spacing="sm">
            {data.recommended.map((r) => (
              <RecommendationCard key={r.verdict.id} rec={r} />
            ))}
          </SimpleGrid>
        ) : (
          <RecommendationList recs={data.recommended} />
        ))}

      {data && data.reading.length > 0 && (
        <Paper withBorder p="md">
          <Title order={4} mb={4}>
            Reading now
          </Title>
          <Text size="sm" c="dimmed" mb="sm">
            Started in Codex, waiting for an outcome.
          </Text>
          <Stack gap="xs">
            {data.reading.map((r) => (
              <ReadingRow key={r.verdict.id} rec={r} />
            ))}
          </Stack>
        </Paper>
      )}
    </Stack>
  );
}

function OwnershipChips({ rec }: { rec: Recommendation }) {
  return (
    <>
      {rec.inLibrary ? (
        <Badge color="green" variant="light" size="sm">
          in library
        </Badge>
      ) : (
        <Badge color="blue" variant="light" size="sm">
          acquirable
        </Badge>
      )}
      {rec.timesRecommended > 1 && (
        <Badge color="gray" variant="light" size="sm">
          recommended {rec.timesRecommended}×
        </Badge>
      )}
    </>
  );
}

function Actions({ rec }: { rec: Recommendation }) {
  const title = seriesTitle(rec.verdict);
  return (
    <Group gap="xs">
      <OutcomeMenu verdictId={rec.verdict.id} title={title} />
      <NotInterestedButton verdictId={rec.verdict.id} title={title} />
    </Group>
  );
}

function RecommendationCard({ rec }: { rec: Recommendation }) {
  const v = rec.verdict;
  return (
    <Paper withBorder p="md">
      <Group align="flex-start" gap="md" wrap="nowrap">
        {v.coverUrl && (
          <Image
            src={v.coverUrl}
            w={110}
            h={156}
            radius="sm"
            fit="cover"
            alt=""
          />
        )}
        <Stack gap="xs" style={{ flex: 1, minWidth: 0 }}>
          <Group gap="xs" wrap="wrap">
            <TierBadge tier={v.tier} />
            <Text fw={600}>{seriesTitle(v)}</Text>
          </Group>
          <Group gap="xs">
            <OwnershipChips rec={rec} />
          </Group>
          <LinkChips verdict={v} />
          {v.summary && (
            <ClampedTip text={v.summary} lines={3} size="xs" dimmed />
          )}
          <ClampedTip text={v.rationale} lines={4} />
          <Text size="xs" c="dimmed">
            Recommended {timeAgo(v.createdAt)}
          </Text>
          <Actions rec={rec} />
        </Stack>
      </Group>
    </Paper>
  );
}

function RecommendationList({ recs }: { recs: Recommendation[] }) {
  // The list arrives ordered by tier, so a heading starts at each change.
  return (
    <Paper withBorder px="md" py="xs">
      {recs.map((r, i) => {
        const tier = r.verdict.tier;
        const first = i === 0 || recs[i - 1].verdict.tier !== tier;
        return (
          <div key={r.verdict.id}>
            {first && (
              <Text
                size="xs"
                fw={700}
                c="dimmed"
                tt="uppercase"
                mt={i === 0 ? 4 : "md"}
              >
                {tier} tier
              </Text>
            )}
            <ListRow rec={r} />
          </div>
        );
      })}
    </Paper>
  );
}

function ListRow({ rec }: { rec: Recommendation }) {
  const v = rec.verdict;
  return (
    <Group
      gap="sm"
      py="xs"
      wrap="wrap"
      align="center"
      style={{ borderBottom: "1px solid var(--mantine-color-default-border)" }}
    >
      <Group gap="sm" wrap="nowrap" style={{ flex: "1 1 320px", minWidth: 0 }}>
        {v.coverUrl && (
          <Image
            src={v.coverUrl}
            w={40}
            h={56}
            radius="sm"
            fit="cover"
            alt=""
          />
        )}
        <Stack gap={2} style={{ minWidth: 0 }}>
          <Group gap="xs" wrap="wrap">
            <TierBadge tier={v.tier} />
            <Text fw={600} size="sm">
              {seriesTitle(v)}
            </Text>
            <OwnershipChips rec={rec} />
          </Group>
          <ClampedTip text={v.rationale} lines={2} />
          <Group gap="md">
            <LinkChips verdict={v} />
            <Text size="xs" c="dimmed">
              Recommended {timeAgo(v.createdAt)}
            </Text>
          </Group>
        </Stack>
      </Group>
      <Actions rec={rec} />
    </Group>
  );
}

function ReadingRow({ rec }: { rec: Recommendation }) {
  const v = rec.verdict;
  const progress =
    rec.booksRead != null && rec.bookCount != null
      ? `${rec.booksRead} of ${rec.bookCount} read`
      : "started";
  return (
    <Group gap="sm" wrap="wrap" justify="space-between">
      <Group gap="sm" wrap="nowrap" style={{ minWidth: 0 }}>
        {v.coverUrl && (
          <Image
            src={v.coverUrl}
            w={32}
            h={45}
            radius="sm"
            fit="cover"
            alt=""
          />
        )}
        <Text fw={600} size="sm">
          {seriesTitle(v)}
        </Text>
        <Badge color="yellow" variant="light" size="sm">
          {progress}
        </Badge>
      </Group>
      <OutcomeMenu verdictId={v.id} title={seriesTitle(v)} />
    </Group>
  );
}
