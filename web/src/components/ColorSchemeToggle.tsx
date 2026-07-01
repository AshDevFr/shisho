import {
  ActionIcon,
  type MantineColorScheme,
  Tooltip,
  useMantineColorScheme,
} from "@mantine/core";

/// Three-state color scheme cycle: System (follow OS) → Light → Dark → System.
/// Mantine persists the choice to localStorage and resolves "auto" against the
/// OS preference, so there is no app-level state to keep here.
const NEXT: Record<MantineColorScheme, MantineColorScheme> = {
  auto: "light",
  light: "dark",
  dark: "auto",
};

const LABEL: Record<MantineColorScheme, string> = {
  auto: "System",
  light: "Light",
  dark: "Dark",
};

export function ColorSchemeToggle() {
  const { colorScheme, setColorScheme } = useMantineColorScheme();
  const label = `Color scheme: ${LABEL[colorScheme]}`;

  return (
    <Tooltip label={label} position="right">
      <ActionIcon
        variant="default"
        size="lg"
        aria-label={label}
        onClick={() => setColorScheme(NEXT[colorScheme])}
      >
        <SchemeIcon scheme={colorScheme} />
      </ActionIcon>
    </Tooltip>
  );
}

function SchemeIcon({ scheme }: { scheme: MantineColorScheme }) {
  if (scheme === "light") return <SunIcon />;
  if (scheme === "dark") return <MoonIcon />;
  return <MonitorIcon />;
}

const ICON_PROPS = {
  width: 18,
  height: 18,
  viewBox: "0 0 24 24",
  fill: "none",
  stroke: "currentColor",
  strokeWidth: 2,
  strokeLinecap: "round",
  strokeLinejoin: "round",
} as const;

function SunIcon() {
  return (
    <svg {...ICON_PROPS} aria-hidden="true">
      <circle cx="12" cy="12" r="4" />
      <path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41" />
    </svg>
  );
}

function MoonIcon() {
  return (
    <svg {...ICON_PROPS} aria-hidden="true">
      <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" />
    </svg>
  );
}

function MonitorIcon() {
  return (
    <svg {...ICON_PROPS} aria-hidden="true">
      <rect x="2" y="3" width="20" height="14" rx="2" />
      <path d="M8 21h8M12 17v4" />
    </svg>
  );
}
