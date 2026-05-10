import type { ProviderKind } from "@/lib/types";

// ---------------------------------------------------------------------------
// Provider brand icons.
//
// Each icon is rendered with `fill="currentColor"` so callers control the
// tint via Tailwind `text-…` classes (see PROVIDER_ICON_COLOR_CLASS below).
// Three of the four marks live on a 24×24 viewBox; the Claude starburst is
// the official 100×100 SVG from Wikimedia Commons (its proportions are
// finicky enough that re-projecting the points by hand would distort the
// rays), so we carry a per-provider viewBox alongside the path data.
//
// Sources:
//   - Claude / Claude Code: Anthropic's "Claude AI symbol" — the radial
//                         starburst Anthropic uses across Claude.ai and
//                         the Claude Code CLI splash. Path lifted from
//                         the Wikimedia Commons file, viewBox preserved.
//                         File: https://commons.wikimedia.org/wiki/File:Claude_AI_symbol.svg
//                         Direct: https://upload.wikimedia.org/wikipedia/commons/b/b0/Claude_AI_symbol.svg
//   - OpenAI / Codex:     simpleicons "openai"
//   - GitHub Copilot:     simpleicons "githubcopilot"
//   - opencode:           project's own favicon at https://opencode.ai
//                         (https://opencode.ai/favicon.svg) — black-and-white
//                         cube logo, normalized to a 24×24 viewBox here.
//
// Each underlying logo is still a trademark of its respective owner; using
// them here to label the provider behind a session is *nominative use*
// (telling the user which AI service their thread is talking to), the same
// pattern every multi-provider AI tool uses today. We don't imply
// endorsement, don't modify the marks, and don't use them outside this
// nominative role.
// ---------------------------------------------------------------------------

interface ProviderIconProps {
  kind: ProviderKind;
  /** Tailwind size + color classes. Defaults to `h-3.5 w-3.5` and the
   *  provider's brand-tint via `currentColor`. Pass e.g.
   *  `text-muted-foreground` to override the tint when the chip is
   *  disabled / not-ready. */
  className?: string;
  /** Visually-hidden label. Defaults to the provider's display name —
   *  the surrounding chip usually has its own label, so the SVG stays
   *  decorative (`aria-hidden`) by default. Pass a string here when the
   *  icon is the only thing carrying meaning. */
  ariaLabel?: string;
}

interface IconDef {
  /** SVG path `d` data, drawn from the source SVG verbatim where possible. */
  d: string;
  /** Per-provider viewBox so we can keep marks in their native coordinate
   *  space rather than rounding them onto a shared 24×24 grid. */
  viewBox: string;
}

const ICONS: Record<ProviderKind, IconDef> = {
  // Claude / Claude Code starburst — radial rays around a hollow centre.
  // 100×100 viewBox preserved from Wikimedia Commons; the path uses many
  // sub-pixel coordinates that wouldn't survive a hand-rescale to 24×24.
  claude: {
    viewBox: "0 0 100 100",
    d: "m19.6 66.5 19.7-11 .3-1-.3-.5h-1l-3.3-.2-11.2-.3L14 53l-9.5-.5-2.4-.5L0 49l.2-1.5 2-1.3 2.9.2 6.3.5 9.5.6 6.9.4L38 49.1h1.6l.2-.7-.5-.4-.4-.4L29 41l-10.6-7-5.6-4.1-3-2-1.5-2-.6-4.2 2.7-3 3.7.3.9.2 3.7 2.9 8 6.1L37 36l1.5 1.2.6-.4.1-.3-.7-1.1L33 25l-6-10.4-2.7-4.3-.7-2.6c-.3-1-.4-2-.4-3l3-4.2L28 0l4.2.6L33.8 2l2.6 6 4.1 9.3L47 29.9l2 3.8 1 3.4.3 1h.7v-.5l.5-7.2 1-8.7 1-11.2.3-3.2 1.6-3.8 3-2L61 2.6l2 2.9-.3 1.8-1.1 7.7L59 27.1l-1.5 8.2h.9l1-1.1 4.1-5.4 6.9-8.6 3-3.5L77 13l2.3-1.8h4.3l3.1 4.7-1.4 4.9-4.4 5.6-3.7 4.7-5.3 7.1-3.2 5.7.3.4h.7l12-2.6 6.4-1.1 7.6-1.3 3.5 1.6.4 1.6-1.4 3.4-8.2 2-9.6 2-14.3 3.3-.2.1.2.3 6.4.6 2.8.2h6.8l12.6 1 3.3 2 1.9 2.7-.3 2-5.1 2.6-6.8-1.6-16-3.8-5.4-1.3h-.8v.4l4.6 4.5 8.3 7.5L89 80.1l.5 2.4-1.3 2-1.4-.2-9.2-7-3.6-3-8-6.8h-.5v.7l1.8 2.7 9.8 14.7.5 4.5-.7 1.4-2.6 1-2.7-.6-5.8-8-6-9-4.7-8.2-.5.4-2.9 30.2-1.3 1.5-3 1.2-2.5-2-1.4-3 1.4-6.2 1.6-8 1.3-6.4 1.2-7.9.7-2.6v-.2H49L43 72l-9 12.3-7.2 7.6-1.7.7-3-1.5.3-2.8L24 86l10-12.8 6-7.9 4-4.6-.1-.5h-.3L17.2 77.4l-4.7.6-2-2 .2-3 1-1 8-5.5Z",
  },
  // OpenAI knot mark.
  codex: {
    viewBox: "0 0 24 24",
    d: "M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108 6.0462 6.0462 0 0 0-6.5098-2.9A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9 6.0462 6.0462 0 0 0 .7427 7.0966 5.98 5.98 0 0 0 .511 4.9107 6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557 6.0557 0 0 0 5.7718-4.2058 5.9894 5.9894 0 0 0 3.9977-2.9001 6.0557 6.0557 0 0 0-.7475-7.0729zm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804 4.7783-2.7582a.7948.7948 0 0 0 .3927-.6813v-6.7369l2.02 1.1686a.071.071 0 0 1 .038.052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944zm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142.0852 4.783 2.7582a.7712.7712 0 0 0 .7806 0l5.8428-3.3685v2.3324a.0804.0804 0 0 1-.0332.0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464zM2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664.7664 0 0 0 .3879.6765l5.8144 3.3543-2.0201 1.1685a.0757.0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963 3.8558L13.1038 8.364 15.1192 7.2a.0757.0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765 8.1042v-5.6772a.79.79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852-4.7735-2.7818a.7759.7759 0 0 0-.7854 0L9.409 9.2297V6.8974a.0662.0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802 4.66zM8.3065 12.863l-2.02-1.1638a.0804.0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757-3.4537l-.142.0805L8.704 5.459a.7948.7948 0 0 0-.3927.6813zm1.0976-2.3654l2.602-1.4998 2.6069 1.4998v2.9994l-2.5974 1.4997-2.6067-1.4997Z",
  },
  // GitHub Copilot — robot head silhouette with two "eye" rectangles.
  github_copilot: {
    viewBox: "0 0 24 24",
    d: "M23.922 16.997C23.061 18.492 18.063 22.02 12 22.02 5.937 22.02.939 18.492.078 16.997A.641.641 0 0 1 0 16.741v-2.869a.883.883 0 0 1 .053-.22c.372-.935 1.347-2.292 2.605-2.656.167-.429.414-1.055.644-1.517a10.098 10.098 0 0 1-.052-1.086c0-1.331.282-2.499 1.132-3.368.397-.406.89-.717 1.474-.952C7.255 2.937 9.248 1.98 11.978 1.98c2.731 0 4.767.957 6.166 2.093.584.235 1.077.546 1.474.952.85.869 1.132 2.037 1.132 3.368 0 .368-.014.733-.052 1.086.23.462.477 1.088.644 1.517 1.258.364 2.233 1.721 2.605 2.656a.841.841 0 0 1 .053.22v2.869a.641.641 0 0 1-.078.256Zm-11.75-5.992h-.344a4.359 4.359 0 0 1-.355.508c-.77.947-1.918 1.492-3.508 1.492-1.725 0-2.989-.359-3.782-1.259a2.137 2.137 0 0 1-.085-.104L4 11.746v6.585c1.435.779 4.514 2.179 8 2.179 3.486 0 6.565-1.4 8-2.179v-6.585l-.098-.104s-.033.045-.085.104c-.793.9-2.057 1.259-3.782 1.259-1.59 0-2.738-.545-3.508-1.492a4.359 4.359 0 0 1-.355-.508Zm2.328 3.25c.549 0 1 .451 1 1v2c0 .549-.451 1-1 1-.549 0-1-.451-1-1v-2c0-.549.451-1 1-1Zm-5 0c.549 0 1 .451 1 1v2c0 .549-.451 1-1 1-.549 0-1-.451-1-1v-2c0-.549.451-1 1-1Zm3.313-6.185c.136 1.057.403 1.913.878 2.497.442.544 1.134.938 2.344.938 1.573 0 2.292-.337 2.657-.751.384-.435.558-1.15.558-2.361 0-1.14-.243-1.847-.705-2.319-.477-.488-1.319-.862-2.824-1.025-1.487-.161-2.192.138-2.533.529-.269.307-.437.808-.438 1.578v.021c0 .265.021.562.063.893Zm-1.626 0c.042-.331.063-.628.063-.894v-.02c-.001-.77-.169-1.271-.438-1.578-.341-.391-1.046-.69-2.533-.529-1.505.163-2.347.537-2.824 1.025-.462.472-.705 1.179-.705 2.319 0 1.211.175 1.926.558 2.361.365.414 1.084.751 2.657.751 1.21 0 1.902-.394 2.344-.938.475-.584.742-1.44.878-2.497Z",
  },
  // opencode — frame-with-window silhouette derived from the
  // black-and-white cube favicon at https://opencode.ai/favicon.svg.
  // Original at viewBox 512×512; coordinates here are scaled by
  // 24/512 (= 0.046875) and rendered with fill-rule:evenodd so the
  // inner rectangle reads as a hole.
  opencode: {
    viewBox: "0 0 24 24",
    d: "M18 19.5H6V4.5H18V19.5ZM15 7.5H9V16.5H15V7.5Z",
  },
};

/** Brand-tint Tailwind class per provider. Mirrors the shades used in
 *  `PROVIDER_COLORS` (which are `bg-…` classes) but in their `text-…`
 *  counterparts so the SVG's `currentColor` picks up the right tint. */
export const PROVIDER_ICON_COLOR_CLASS: Record<ProviderKind, string> = {
  // Anthropic's coral/peach starburst tint — closer to the on-brand
  // colour used on Claude.ai than the previous amber.
  claude: "text-orange-400",
  codex: "text-green-500",
  github_copilot: "text-blue-500",
  opencode: "text-orange-500",
};

const PROVIDER_LABEL: Record<ProviderKind, string> = {
  claude: "Claude",
  codex: "Codex",
  github_copilot: "GitHub Copilot",
  opencode: "opencode",
};

/**
 * Square brand icon for a provider — drop-in replacement for the
 * 2×2 colored dot that used to label provider chips.
 *
 * The SVG itself is decorative by default (the chip beside it carries
 * the textual label) — pass `ariaLabel` only when this icon is the
 * sole signal of which provider is active.
 *
 * Some providers' marks (`opencode`, the GitHub Copilot bridge) use
 * `fill-rule: evenodd` to render holes — we set it on the path to be
 * safe; SVGs that don't need it ignore the attribute.
 */
export function ProviderIcon({ kind, className, ariaLabel }: ProviderIconProps) {
  const label = ariaLabel ?? PROVIDER_LABEL[kind];
  const decorative = ariaLabel === undefined;
  const icon = ICONS[kind];
  return (
    <svg
      viewBox={icon.viewBox}
      role={decorative ? undefined : "img"}
      aria-hidden={decorative ? true : undefined}
      aria-label={decorative ? undefined : label}
      className={className ?? "h-3.5 w-3.5"}
      fill="currentColor"
    >
      {!decorative && <title>{label}</title>}
      <path d={icon.d} fillRule="evenodd" clipRule="evenodd" />
    </svg>
  );
}
