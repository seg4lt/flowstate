/**
 * Markdown-specific syntax highlight palette.
 *
 * Bound to the most-used Lezer tags so most languages
 * (`@codemirror/lang-*`) get a usable colour out of the box, plus the
 * markdown-body tags (`heading`, `link`, `emphasis`, `strong`,
 * `strikethrough`) so headers, bold, etc. inherit a consistent colour
 * with the live-preview decorations.
 *
 * Colours map to flowstate's shadcn palette via CSS custom properties
 * (defined in `apps/flowstate/src/index.css`). Falling back to
 * `var(--foreground)` for tags without a direct equivalent.
 */

import { HighlightStyle } from "@codemirror/language";
import { tags as t } from "@lezer/highlight";

export const markdownHighlightStyle = HighlightStyle.define([
  // ── Identifiers ──────────────────────────────────────────────
  {
    tag: t.function(t.variableName),
    color: "var(--primary)",
    fontWeight: "600",
  },
  { tag: t.function(t.propertyName), color: "var(--primary)" },
  {
    tag: t.definition(t.variableName),
    color: "var(--primary)",
    fontWeight: "600",
  },
  { tag: t.definition(t.propertyName), color: "var(--primary)" },
  // ── Keywords ─────────────────────────────────────────────────
  { tag: t.controlKeyword, color: "var(--primary)", fontWeight: "600" },
  { tag: t.modifier, color: "var(--primary)", fontWeight: "600" },
  { tag: t.operatorKeyword, color: "var(--primary)" },
  // ── Operators / punctuation ──────────────────────────────────
  { tag: t.operator, color: "var(--foreground)" },
  { tag: t.punctuation, color: "var(--muted-foreground)" },
  { tag: t.bracket, color: "var(--muted-foreground)" },
  { tag: t.angleBracket, color: "var(--muted-foreground)" },
  { tag: t.squareBracket, color: "var(--muted-foreground)" },
  { tag: t.brace, color: "var(--muted-foreground)" },
  { tag: t.separator, color: "var(--muted-foreground)" },
  // ── Markdown body ────────────────────────────────────────────
  { tag: t.heading, color: "var(--foreground)", fontWeight: "700" },
  { tag: t.link, color: "var(--primary)" },
  { tag: t.url, color: "var(--primary)" },
  { tag: t.emphasis, fontStyle: "italic" },
  { tag: t.strong, fontWeight: "700" },
  {
    tag: t.strikethrough,
    textDecoration: "line-through",
    color: "var(--muted-foreground)",
  },
  // ── Misc ─────────────────────────────────────────────────────
  { tag: t.invalid, color: "var(--destructive)" },
]);
