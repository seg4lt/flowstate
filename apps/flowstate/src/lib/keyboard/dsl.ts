// Keyboard DSL — single source of truth for "what combo is this binding".
//
// Every shortcut in the registry stores ONE string like "mod+shift+d"
// (its `defaultBinding`); the parser resolves it to a normalized
// KeyChord that both the runtime matcher and the platform glyph
// formatter consume. Persisted overrides round-trip through the same
// DSL — that's why `chordToDsl` exists. No platform glyphs ever leak
// into this file; that's `platform.ts`'s job.

import { getPlatform } from "./platform";

/**
 * A parsed binding. `mod` is the platform-resolved primary modifier
 * (metaKey on Mac, ctrlKey elsewhere); `meta` and `ctrl` are still
 * exposed separately so a binding that explicitly uses `cmd` or
 * `ctrl` (rather than `mod`) round-trips faithfully — useful when a
 * user wants a binding pinned to one platform's modifier even on the
 * other (e.g. a Mac power-user binding `ctrl+f` to keep the Emacs
 * habit).
 */
export interface KeyChord {
  key: string;
  mod: boolean;
  shift: boolean;
  alt: boolean;
  ctrl: boolean;
  meta: boolean;
}

// Tokens accepted in the DSL beyond the modifier names below. Maps
// to the canonical `KeyboardEvent.key` value (lowercased) so runtime
// comparison is a cheap string compare. Anything not in this map is
// passed through as `key.toLowerCase()` — letters, digits, brackets,
// punctuation all already match KeyboardEvent.key directly.
const KEY_ALIASES: Record<string, string> = {
  esc: "escape",
  escape: "escape",
  enter: "enter",
  return: "enter",
  space: " ",
  spacebar: " ",
  up: "arrowup",
  down: "arrowdown",
  left: "arrowleft",
  right: "arrowright",
  arrowup: "arrowup",
  arrowdown: "arrowdown",
  arrowleft: "arrowleft",
  arrowright: "arrowright",
  tab: "tab",
  backspace: "backspace",
  bs: "backspace",
  del: "delete",
  delete: "delete",
  // `?` is delivered as the literal character on Shift+/ on US
  // layouts but as `/` with shiftKey on others. Bindings that want
  // the question-mark glyph should use `shift+/` and rely on the
  // matcher's "shift+/ also matches event.key === '?'" fallback.
};

const MODIFIER_TOKENS = new Set([
  "mod",
  "cmd",
  "command",
  "meta",
  "super",
  "win",
  "ctrl",
  "control",
  "shift",
  "alt",
  "opt",
  "option",
]);

/**
 * Parse one DSL string into a KeyChord. Throws on malformed input —
 * shortcuts are static module-level constants in the registry, so a
 * bad DSL string is a build-time-style error and we want it loud.
 *
 * Whitespace and case are ignored. Leading/trailing `+` is rejected.
 * Order of modifiers doesn't matter ("shift+mod+d" === "mod+shift+d").
 */
export function parseDsl(dsl: string): KeyChord {
  const trimmed = dsl.trim().toLowerCase();
  if (trimmed.length === 0) {
    throw new Error(`keyboard DSL: empty binding`);
  }
  // Split on `+` but only between tokens — "+" itself can be the
  // bound key (e.g. `mod++` for Cmd-plus). Strategy: if the binding
  // ends with `++` treat the trailing `+` as the key; otherwise
  // straight split.
  let parts: string[];
  if (trimmed.endsWith("++")) {
    parts = trimmed.slice(0, -2).split("+").filter(Boolean);
    parts.push("+");
  } else {
    parts = trimmed.split("+").filter(Boolean);
  }
  if (parts.length === 0) {
    throw new Error(`keyboard DSL: malformed "${dsl}"`);
  }

  const platform = getPlatform();
  let mod = false;
  let shift = false;
  let alt = false;
  let ctrl = false;
  let meta = false;
  let key: string | null = null;

  for (const raw of parts) {
    const tok = raw;
    if (MODIFIER_TOKENS.has(tok)) {
      switch (tok) {
        case "mod":
          mod = true;
          break;
        case "cmd":
        case "command":
        case "meta":
        case "super":
        case "win":
          meta = true;
          break;
        case "ctrl":
        case "control":
          ctrl = true;
          break;
        case "shift":
          shift = true;
          break;
        case "alt":
        case "opt":
        case "option":
          alt = true;
          break;
      }
      continue;
    }
    if (key !== null) {
      throw new Error(
        `keyboard DSL "${dsl}": multiple non-modifier keys ("${key}", "${tok}")`,
      );
    }
    key = KEY_ALIASES[tok] ?? tok;
  }

  if (key === null) {
    throw new Error(`keyboard DSL "${dsl}": no key (only modifiers)`);
  }
  // Resolve `mod` against the current platform so the runtime check
  // is a cheap field compare. If both `mod` and the platform's raw
  // modifier were specified explicitly, that's fine — they collapse
  // to the same flag.
  if (mod) {
    if (platform === "mac") meta = true;
    else ctrl = true;
  }

  return { key, mod, shift, alt, ctrl, meta };
}

/**
 * Round-trip a chord back to its DSL form for storage. Always emits
 * `mod` rather than the platform-specific modifier so the same
 * persisted string works on both Mac and Win/Linux. Modifier order
 * is canonicalized (mod, ctrl, alt, shift) so equality compares are
 * trivial against parsed defaults.
 */
export function chordToDsl(c: KeyChord): string {
  const parts: string[] = [];
  if (c.mod) parts.push("mod");
  if (c.ctrl && !c.mod) parts.push("ctrl");
  if (c.meta && !c.mod) parts.push("cmd");
  if (c.alt) parts.push("alt");
  if (c.shift) parts.push("shift");
  parts.push(c.key);
  return parts.join("+");
}

/**
 * Compare a parsed chord against a live KeyboardEvent. Strict on
 * modifiers — a binding for `mod+d` does NOT fire when the user
 * presses `mod+shift+d`. That's important for rebinding-safety: if
 * the user adds `mod+shift+d` later, it must own the keystroke
 * unambiguously.
 *
 * The `?` / `/` overlap is the one fuzzy case. US layouts deliver
 * Shift+/ as event.key === "?" with shiftKey=true; other layouts
 * deliver "/" with shiftKey=true. We treat them as the same key
 * when the binding is `shift+/`.
 */
export function matchChord(c: KeyChord, e: KeyboardEvent): boolean {
  const evKey = (e.key ?? "").toLowerCase();
  const keyMatches =
    evKey === c.key ||
    (c.shift && c.key === "/" && evKey === "?") ||
    (c.shift && c.key === "?" && evKey === "/");
  if (!keyMatches) return false;
  if (c.shift !== e.shiftKey) return false;
  if (c.alt !== e.altKey) return false;
  if (c.meta !== e.metaKey) return false;
  if (c.ctrl !== e.ctrlKey) return false;
  return true;
}
