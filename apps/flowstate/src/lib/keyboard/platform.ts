// Platform detection + display formatting. The ONE place glyphs
// live — adding Win/Linux polish later means editing two maps in
// this file, no shortcut definitions touched.

import type { KeyChord } from "./dsl";

export type Platform = "mac" | "win" | "linux";

let cachedPlatform: Platform | null = null;

/**
 * Detect the current platform. Uses `navigator.platform` first
 * (Tauri's WKWebView on Mac always reports "MacIntel" / "MacARM");
 * falls back to `navigator.userAgent` for browsers / runtimes that
 * have started returning empty `platform`. Result is cached because
 * the value can't change inside a session — re-reading `navigator`
 * on every keystroke would be wasteful.
 *
 * SSR / non-browser fallback returns "linux" — the choice is
 * arbitrary but `mod` resolves to `ctrl` there which matches what
 * Tauri non-Mac builds use.
 */
export function getPlatform(): Platform {
  if (cachedPlatform !== null) return cachedPlatform;
  if (typeof navigator === "undefined") {
    cachedPlatform = "linux";
    return cachedPlatform;
  }
  const platformStr = (navigator.platform || "").toLowerCase();
  const userAgent = (navigator.userAgent || "").toLowerCase();
  const haystack = `${platformStr} ${userAgent}`;
  if (haystack.includes("mac")) {
    cachedPlatform = "mac";
  } else if (haystack.includes("win")) {
    cachedPlatform = "win";
  } else {
    cachedPlatform = "linux";
  }
  return cachedPlatform;
}

// Per-platform glyph maps. Strings here are what users see in the
// cheatsheet — nothing matches against them at runtime.
//
// Mac uses Apple's standard modifier glyphs (⌘ ⇧ ⌥ ⌃) which most
// users recognize from the macOS menu bar. Win/Linux use textual
// "Ctrl"/"Shift"/"Alt" because their native conventions don't have a
// universally-recognized symbol set — abusing the Mac glyphs there
// would just confuse users.
const MOD_GLYPH: Record<Platform, string> = {
  mac: "⌘",
  win: "Ctrl",
  linux: "Ctrl",
};
const CMD_GLYPH: Record<Platform, string> = {
  mac: "⌘",
  win: "Win",
  linux: "Super",
};
const CTRL_GLYPH: Record<Platform, string> = {
  mac: "⌃",
  win: "Ctrl",
  linux: "Ctrl",
};
const ALT_GLYPH: Record<Platform, string> = {
  mac: "⌥",
  win: "Alt",
  linux: "Alt",
};
const SHIFT_GLYPH: Record<Platform, string> = {
  mac: "⇧",
  win: "Shift",
  linux: "Shift",
};

// Special key labels — same on every platform but extracted so they
// share the formatChord call site.
const KEY_LABEL: Record<string, string> = {
  enter: "↵",
  escape: "Esc",
  arrowup: "↑",
  arrowdown: "↓",
  arrowleft: "←",
  arrowright: "→",
  tab: "Tab",
  backspace: "⌫",
  delete: "Del",
  " ": "Space",
};

/**
 * Format a chord as the array of display chips the cheatsheet
 * renders (one chip per element, joined visually with thin gaps).
 * On Mac the chips are 1-character glyphs; on Win/Linux they're
 * short text labels.
 *
 * The chord MUST be parsed via `parseDsl` first so its `mod` flag
 * has been resolved. `formatChord` doesn't re-resolve — it just
 * renders what's there.
 */
export function formatChord(c: KeyChord): string[] {
  const platform = getPlatform();
  const parts: string[] = [];
  // When `mod` was specified, prefer its single glyph and skip the
  // raw `meta`/`ctrl` flags it set during parse — otherwise a
  // `mod+shift+d` binding would render as "⌘ ⌘ ⇧ D" (the platform
  // glyph fires twice). When the user explicitly used `cmd`/`ctrl`
  // (no `mod`), render those raw glyphs verbatim.
  if (c.mod) {
    parts.push(MOD_GLYPH[platform]);
  } else {
    if (c.meta) parts.push(CMD_GLYPH[platform]);
    if (c.ctrl) parts.push(CTRL_GLYPH[platform]);
  }
  if (c.alt) parts.push(ALT_GLYPH[platform]);
  if (c.shift) parts.push(SHIFT_GLYPH[platform]);
  parts.push(formatKey(c.key));
  return parts;
}

function formatKey(key: string): string {
  const label = KEY_LABEL[key];
  if (label) return label;
  if (key.length === 1) return key.toUpperCase();
  // Multi-char fallback — leave as-is so unmapped keys still render
  // legibly. Should be rare; add to KEY_LABEL when one comes up.
  return key;
}
