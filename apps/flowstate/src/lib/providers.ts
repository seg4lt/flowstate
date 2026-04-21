// Single source of truth for provider metadata. Adding a provider
// means adding one entry here + the Rust enum — no more hunting
// through five frontend files for color/label/order tables.
//
// `label` is the display name; `color` is a Tailwind BG class used
// for the sidebar / settings dots; `hex` is the recharts-friendly
// fill color for the cost chart; `order` is the stable sidebar /
// settings list ordering; `defaultEnabled` mirrors the old
// DEFAULT_ENABLED_PROVIDERS set; `slashPrefix` is the composer
// invocation prefix for skill-style commands (Codex uses `$`,
// everyone else uses `/`).

import type { ProviderKind } from "./types";

export interface ProviderMeta {
  label: string;
  /** Tailwind BG class (e.g. `"bg-amber-500"`) used by the sidebar
   *  dot and the settings row dot. */
  color: string;
  /** Raw hex used by the usage cost chart (recharts can't read
   *  Tailwind). Kept in sync with `color` by inspection. */
  hex: string;
  order: number;
  defaultEnabled: boolean;
  /** Skill-command prefix the provider uses when invoking non-core
   *  slash commands. Codex historically uses `$`, every other
   *  provider uses `/`. See `slash-commands.ts`. */
  slashPrefix: "/" | "$";
}

export const PROVIDER_META: Record<ProviderKind, ProviderMeta> = {
  claude: {
    label: "Claude",
    color: "bg-amber-500",
    hex: "#f59e0b",
    order: 0,
    defaultEnabled: true,
    slashPrefix: "/",
  },
  claude_cli: {
    label: "Claude 2",
    color: "bg-purple-500",
    hex: "#a855f7",
    order: 1,
    defaultEnabled: false,
    slashPrefix: "/",
  },
  codex: {
    label: "Codex",
    color: "bg-green-500",
    hex: "#10b981",
    order: 2,
    defaultEnabled: false,
    slashPrefix: "$",
  },
  github_copilot: {
    label: "GitHub Copilot",
    color: "bg-blue-500",
    hex: "#3b82f6",
    order: 3,
    defaultEnabled: true,
    slashPrefix: "/",
  },
  github_copilot_cli: {
    label: "GitHub Copilot 2",
    color: "bg-cyan-500",
    hex: "#06b6d4",
    order: 4,
    defaultEnabled: false,
    slashPrefix: "/",
  },
  opencode: {
    label: "opencode",
    color: "bg-orange-500",
    hex: "#f97316",
    order: 5,
    // Opt-in — the adapter depends on the external `opencode`
    // binary being on PATH. Showing it enabled by default on a
    // fresh install would produce a warning badge in Settings for
    // every user who hasn't installed opencode yet.
    defaultEnabled: false,
    slashPrefix: "/",
  },
};

/** Provider kinds in canonical display order. Derived from
 *  `PROVIDER_META` so adding a new provider only requires a single
 *  edit. */
export const PROVIDER_KINDS: readonly ProviderKind[] = (
  Object.entries(PROVIDER_META) as [ProviderKind, ProviderMeta][]
)
  .sort((a, b) => a[1].order - b[1].order)
  .map(([kind]) => kind);

/** Set of providers enabled out of the box. */
export const DEFAULT_ENABLED_PROVIDERS: ReadonlySet<ProviderKind> = new Set(
  PROVIDER_KINDS.filter((k) => PROVIDER_META[k].defaultEnabled),
);

/** Tailwind-class color table. Retained for call sites that want the
 *  class string directly. Prefer `PROVIDER_META[kind].color` in new
 *  code. */
export const PROVIDER_COLORS: Record<ProviderKind, string> = Object.fromEntries(
  PROVIDER_KINDS.map((k) => [k, PROVIDER_META[k].color]),
) as Record<ProviderKind, string>;

/** `{kind,label}` list in canonical order. Mirrors the old
 *  `ALL_PROVIDERS` shape used by sidebar dropdowns. */
export const ALL_PROVIDERS: readonly { kind: ProviderKind; label: string }[] =
  PROVIDER_KINDS.map((kind) => ({ kind, label: PROVIDER_META[kind].label }));
