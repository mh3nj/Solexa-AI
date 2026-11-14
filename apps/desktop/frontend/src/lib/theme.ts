import { BUILTIN_THEMES, type BuiltinTheme } from "../themes/defaults";

export type ThemeKind = "dark" | "light";

export interface Theme {
  id: string;
  name: string;
  kind: ThemeKind;
  builtin?: boolean;
  bg: string;
  panel: string;
  line: string;
  ink: string;
  muted: string;
  accent: string;
  ok: string;
  danger: string;
}

export const THEME_TOKENS = [
  "bg",
  "panel",
  "line",
  "ink",
  "muted",
  "accent",
  "ok",
  "danger",
] as const;

export type ThemeToken = (typeof THEME_TOKENS)[number];

/** The app's original palette, kept as a built-in theme so nothing looks
 * different until someone chooses another one. */
export const GRAPHITE_THEME: Theme = {
  id: "solexa-graphite",
  name: "SOLEXA Graphite",
  kind: "dark",
  builtin: true,
  bg: "#12151b",
  panel: "#1a1f28",
  line: "#2a3140",
  ink: "#e7e9ee",
  muted: "#8a93a6",
  accent: "#e3a857",
  ok: "#4fd1c5",
  danger: "#e1616b",
};

export const BUILTIN: Theme[] = [GRAPHITE_THEME, ...(BUILTIN_THEMES as BuiltinTheme[])];

export const DEFAULT_THEME_ID = GRAPHITE_THEME.id;

export function findTheme(id: string | undefined, custom: Theme[]): Theme {
  if (id) {
    const found = [...BUILTIN, ...custom].find((theme) => theme.id === id);
    if (found) return found;
  }
  return GRAPHITE_THEME;
}

const HEX = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i;

export function isHexColor(value: string): boolean {
  return HEX.test(value.trim());
}

function channels(hex: string): [number, number, number] {
  let digits = hex.trim().replace(/^#/, "");
  if (digits.length === 3) {
    digits = digits
      .split("")
      .map((ch) => ch + ch)
      .join("");
  }
  if (digits.length !== 6) return [0, 0, 0];
  return [
    parseInt(digits.slice(0, 2), 16),
    parseInt(digits.slice(2, 4), 16),
    parseInt(digits.slice(4, 6), 16),
  ];
}

/** "18 21 27" so Tailwind's `rgb(var(--c-bg) / <alpha-value>)` works. */
export function rgbTriplet(hex: string): string {
  return channels(hex).join(" ");
}

/** Black or white, whichever reads better on this background. */
export function readableInk(hex: string): string {
  const [r, g, b] = channels(hex).map((value) => {
    const c = value / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  });
  const luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
  return luminance > 0.45 ? "#12151b" : "#ffffff";
}

export function applyTheme(theme: Theme): void {
  const root = document.documentElement;
  for (const token of THEME_TOKENS) {
    root.style.setProperty(`--c-${token}`, rgbTriplet(theme[token]));
  }
  root.style.setProperty("--c-on-accent", rgbTriplet(readableInk(theme.accent)));
  root.dataset.themeKind = theme.kind;
  root.style.colorScheme = theme.kind;
}

/** Inline style object for previewing a theme in a sub-tree. */
export function themeStyle(theme: Theme): Record<string, string> {
  const style: Record<string, string> = {};
  for (const token of THEME_TOKENS) {
    style[`--c-${token}`] = rgbTriplet(theme[token]);
  }
  style["--c-on-accent"] = rgbTriplet(readableInk(theme.accent));
  return style;
}

export function sanitizeTheme(candidate: Partial<Theme>): Theme | null {
  const tokens = THEME_TOKENS.every((token) => {
    const value = candidate[token];
    return typeof value === "string" && isHexColor(value);
  });
  if (!tokens) return null;
  const kind: ThemeKind = candidate.kind === "light" ? "light" : "dark";
  return {
    id: candidate.id ?? `custom-${Date.now()}`,
    name: (candidate.name ?? "").trim() || "Untitled theme",
    kind,
    builtin: false,
    bg: candidate.bg!,
    panel: candidate.panel!,
    line: candidate.line!,
    ink: candidate.ink!,
    muted: candidate.muted!,
    accent: candidate.accent!,
    ok: candidate.ok!,
    danger: candidate.danger!,
  };
}
