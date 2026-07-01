import {
  type CSSVariablesResolver,
  createTheme,
  type MantineColorsTuple,
} from "@mantine/core";

// Central app theme, matching the server-rendered OAuth pages: a pine/emerald
// teal accent, warm-paper light surfaces, deep-slate dark surfaces, a serif
// display face for headings, and softly rounded corners. Most components pick
// this up automatically through Mantine tokens; the few hardcoded badge color
// maps reference palette names that still resolve here.

// Teal accent. Index 6 is the light primary (pine), index 3 the dark primary
// (bright teal) — see `primaryShade` below.
const teal: MantineColorsTuple = [
  "#e6f7f3",
  "#c8efe8",
  "#94e0d3",
  "#5cd3c1",
  "#33c9b4",
  "#16ab98",
  "#0c7a6b",
  "#0a6256",
  "#084c44",
  "#06332e",
];

// Slate neutrals for dark mode. Index 7 is the page body, 6 the elevated
// surfaces (cards, inputs, navbar), 4/5 the borders, 0–2 the text tiers.
const dark: MantineColorsTuple = [
  "#c9ccd4",
  "#a7adba",
  "#808997",
  "#5b6373",
  "#3d4452",
  "#2a2f3b",
  "#1d212a",
  "#14161d",
  "#0e1014",
  "#08090c",
];

const SERIF = 'ui-serif, Georgia, "Times New Roman", serif';

export const theme = createTheme({
  primaryColor: "teal",
  // Brighter teal in dark mode, deep pine in light.
  primaryShade: { light: 6, dark: 3 },
  // Auto-pick legible text on filled accents (dark text on the bright teal).
  autoContrast: true,
  colors: { teal, dark },
  // Warm off-white for light-mode surfaces (cards, popovers, inputs); the page
  // body is nudged a shade warmer in cssVariablesResolver below.
  white: "#fffdf9",
  fontFamily:
    '-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, system-ui, sans-serif',
  headings: { fontFamily: SERIF, fontWeight: "600" },
  defaultRadius: "md",
  radius: { xs: "6px", sm: "8px", md: "11px", lg: "16px", xl: "22px" },
});

// Passed to MantineProvider (not part of the theme object). Sets the page body
// background distinct from the off-white card surfaces.
export const cssVariablesResolver: CSSVariablesResolver = () => ({
  variables: {},
  // Warm paper for the page body, distinct from the off-white card surfaces.
  light: { "--mantine-color-body": "#f3f0e9" },
  // Deep slate body, a step below the elevated dark surfaces.
  dark: { "--mantine-color-body": "#101218" },
});
