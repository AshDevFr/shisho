import { Anchor, Badge, Group, Text, Tooltip } from "@mantine/core";
import type { Verdict } from "@/api/verdicts";

// Pieces of a verdict row shared by Home and History.

export function seriesTitle(v: Verdict): string {
  return v.title ?? v.seriesId ?? "Unknown series";
}

// Tier -> a saturated color; `autoContrast` then picks readable (dark/light)
// text, so the badge stays legible in both themes (a plain Badge was white
// text on a light background in dark mode).
const TIER_COLORS: Record<string, string> = {
  S: "grape",
  A: "teal",
  B: "blue",
  C: "yellow",
  D: "orange",
};

/// A recommendation tier as a filled badge. Anything else (`pass`, the
/// `feedback` rows) is an outline: not a grade, and a filled gray badge lost
/// its text in dark mode.
export function TierBadge({ tier }: { tier: string }) {
  const color = TIER_COLORS[tier.toUpperCase()];
  if (!color) {
    return (
      <Badge variant="outline" color="gray">
        {tier}
      </Badge>
    );
  }
  return (
    <Badge variant="filled" color={color} autoContrast>
      {tier}
    </Badge>
  );
}

export function LinkChips({ verdict }: { verdict: Verdict }) {
  const links: [string, string | null | undefined][] = [
    ["MangaBaka", verdict.mangabakaUrl],
    ["Codex", verdict.codexUrl],
    ["Tsundoku", verdict.tsundokuUrl],
  ];
  const present = links.filter(([, url]) => url);
  if (present.length === 0) return null;
  return (
    <Group gap="md">
      {present.map(([label, url]) => (
        <Anchor key={label} href={url ?? undefined} target="_blank" size="xs">
          {label} ↗
        </Anchor>
      ))}
    </Group>
  );
}

// The rec reason, clamped with the full text on hover (rationales run long).
// Clamped text that reveals its full content in a tooltip on hover — used for
// both the rec reason and the series summary so a clipped line is never a
// dead end.
export function ClampedTip({
  text,
  lines,
  size = "sm",
  dimmed = false,
}: {
  text: string;
  lines: number;
  size?: string;
  dimmed?: boolean;
}) {
  return (
    <Tooltip
      label={text}
      multiline
      w={380}
      withArrow
      events={{ hover: true, focus: true, touch: true }}
    >
      <Text
        size={size}
        c={dimmed ? "dimmed" : undefined}
        lineClamp={lines}
        style={{ cursor: "default" }}
      >
        {text}
      </Text>
    </Tooltip>
  );
}

// A "Summary" chip revealing the MangaBaka description on hover.
export function SummaryTip({ summary }: { summary?: string | null }) {
  if (!summary) return null;
  return (
    <Tooltip
      label={summary}
      multiline
      w={380}
      withArrow
      events={{ hover: true, focus: true, touch: true }}
    >
      <Badge
        variant="outline"
        color="gray"
        size="sm"
        style={{ cursor: "help" }}
      >
        Summary
      </Badge>
    </Tooltip>
  );
}
