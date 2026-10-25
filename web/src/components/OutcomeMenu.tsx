import { Button, Group, Menu, Text } from "@mantine/core";
import { notifications } from "@mantine/notifications";
import { useRecordOutcome } from "@/api/verdicts";

// The outcomes a reader picks from. Anything finer goes through History's
// free-text field; the MCP record_feedback tool takes free text too.
export const OUTCOMES = [
  { value: "loved", label: "Loved" },
  { value: "liked", label: "Liked" },
  { value: "mixed", label: "Mixed" },
  { value: "dropped", label: "Dropped" },
  { value: "dnf", label: "Did not finish" },
] as const;

const NOT_INTERESTED = "not interested";
// Withdraws the series' latest outcome (the log is append-only, so an undo is
// one more row), which returns it to the list.
const REOPENED = "reopened";

/// Record an outcome in one click, then offer Undo in the toast.
function useOneClickOutcome(verdictId: number, title: string) {
  const record = useRecordOutcome();

  const undo = (toastId: string) => {
    notifications.hide(toastId);
    record.mutate(
      { id: verdictId, outcome: REOPENED },
      {
        onError: (err) =>
          notifications.show({ color: "red", message: err.message }),
      },
    );
  };

  const choose = (outcome: string, label: string) => {
    record.mutate(
      { id: verdictId, outcome },
      {
        onSuccess: () => {
          const toastId = `outcome-${verdictId}-${Date.now()}`;
          notifications.show({
            id: toastId,
            // Long enough to reach Undo.
            autoClose: 8000,
            message: (
              <Group justify="space-between" wrap="nowrap">
                <Text size="sm">
                  {title}: {label}
                </Text>
                <Button
                  size="compact-xs"
                  variant="light"
                  onClick={() => undo(toastId)}
                >
                  Undo
                </Button>
              </Group>
            ),
          });
        },
        onError: (err) =>
          notifications.show({ color: "red", message: err.message }),
      },
    );
  };

  return { choose, pending: record.isPending };
}

/// "Outcome ▾": choosing an entry saves it immediately.
export function OutcomeMenu({
  verdictId,
  title,
}: {
  verdictId: number;
  title: string;
}) {
  const { choose, pending } = useOneClickOutcome(verdictId, title);
  return (
    <Menu position="bottom-start" withinPortal>
      <Menu.Target>
        <Button
          size="compact-sm"
          variant="default"
          loading={pending}
          aria-label={`Record an outcome for ${title}`}
        >
          Outcome ▾
        </Button>
      </Menu.Target>
      <Menu.Dropdown>
        {OUTCOMES.map((o) => (
          <Menu.Item key={o.value} onClick={() => choose(o.value, o.label)}>
            {o.label}
          </Menu.Item>
        ))}
      </Menu.Dropdown>
    </Menu>
  );
}

/// One click: turned down without reading.
export function NotInterestedButton({
  verdictId,
  title,
}: {
  verdictId: number;
  title: string;
}) {
  const { choose, pending } = useOneClickOutcome(verdictId, title);
  return (
    <Button
      size="compact-sm"
      variant="subtle"
      color="gray"
      fw={500}
      c="dimmed"
      loading={pending}
      onClick={() => choose(NOT_INTERESTED, "Not interested")}
      aria-label={`Not interested in ${title}`}
    >
      Not interested
    </Button>
  );
}
