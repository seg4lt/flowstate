// Public surface of the keyboard layer. Every consumer outside
// `lib/keyboard/` should import from here (or from the legacy
// `lib/keyboard-shortcuts` shim, which re-exports this) so the
// internal file split stays an implementation detail.

export {
  SHORTCUTS,
  ShortcutsDialog,
  TOGGLE_DIFF_EVENT,
  TOGGLE_CONTEXT_EVENT,
  OPEN_EDITOR_PICKER_EVENT,
  LAUNCH_DEFAULT_EDITOR_EVENT,
  OPEN_MODEL_PICKER_EVENT,
  OPEN_EFFORT_PICKER_EVENT,
  ADD_PROJECT_EVENT,
  effectiveBinding,
  detectConflicts,
} from "./registry";
export type { Shortcut, ShortcutCtx, ShortcutGroup } from "./registry";

export { parseDsl, matchChord, chordToDsl } from "./dsl";
export type { KeyChord } from "./dsl";

export { getPlatform, formatChord } from "./platform";
export type { Platform } from "./platform";

export {
  getOverrideStore,
  createLocalStorageOverrideStore,
} from "./overrides";
export type { ShortcutOverrideStore } from "./overrides";
