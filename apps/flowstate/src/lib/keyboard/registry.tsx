import * as React from "react";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  isPopoutWindow,
  popoutThread,
  readPopoutPinPref,
  setPopoutPinned,
} from "@/lib/popout";
import type { SessionSummary } from "@/lib/types";
import { parseDsl } from "./dsl";
import { formatChord } from "./platform";
import { getOverrideStore, type ShortcutOverrideStore } from "./overrides";

// ─── shortcut registry ─────────────────────────────────────────────
//
// Single source of truth for app-wide keyboard shortcuts. Both the
// global keydown handler (`useGlobalShortcuts`) and the help cheatsheet
// (`ShortcutsDialog`) read from this same array so a new shortcut only
// has to be defined once. Per-view shortcuts (CodeView's tab bar,
// Shift+Tab mode cycling, ⌘J terminal toggle, etc.) intentionally stay
// in their own files — this registry is only for *global* shortcuts
// that fire regardless of focus or current route.
//
// Each entry stores ONE canonical `defaultBinding` DSL string. The
// match predicate is auto-derived via `parseDsl` + `matchChord`, and
// the cheatsheet display chips are auto-derived via `formatChord`.
// User overrides flow through `getOverrideStore()`; the dispatcher
// resolves `effective = overrides.get(id) ?? defaultBinding`.

export type ShortcutGroup = "Navigation" | "View" | "Help";

export interface ShortcutCtx {
  navigate: (opts: NavigateArg) => void;
  /** sessionId of the thread the user is currently viewing, or null
   *  when on a non-chat route. */
  activeSessionId: string | null;
  /** Every thread the sidebar would show, ordered project-by-project
   *  (sidebar order), then thread-by-thread within each project. */
  projectSessions: SessionSummary[];
  /** Open the cheatsheet modal. */
  openShortcutsHelp: () => void;
  /** Open the project picker (then provider dropdown) used by ⌘⇧N. */
  openProjectPicker: () => void;
  /** Start a thread on the active session's project using the user's
   *  saved default provider/model. */
  startThreadOnCurrentProject: () => Promise<void>;
  /** Optional UI-feedback hook (toast in production, no-op in tests). */
  notify?: (message: string) => void;
}

// Structural NavigateArg keeps the registry decoupled from the route
// tree — same trick the dispatcher used previously.
type NavigateArg =
  | { to: "/chat/$sessionId"; params: { sessionId: string } }
  | {
      to: "/code/$sessionId";
      params: { sessionId: string };
      search?: { mode?: "files" | "content" };
    };

export interface Shortcut {
  /** Stable identifier — also the override storage key. NEVER rename
   *  an existing id; doing so would orphan any saved override. */
  id: string;
  /** Human-readable label shown in the cheatsheet. */
  label: string;
  /** Display group for the cheatsheet. */
  group: ShortcutGroup;
  /**
   * Canonical binding DSL — e.g. `"mod+shift+d"`, `"mod+["`, `"escape"`.
   * The dispatcher parses this once and matches via `matchChord`; the
   * cheatsheet renders chips via `formatChord`. NEVER change this for
   * an existing entry — bump the override path instead. The string
   * must round-trip cleanly through `parseDsl` + `chordToDsl`.
   */
  defaultBinding: string;
  /** Whether the shortcut should also fire when focus is inside an
   *  input/textarea/contenteditable. */
  fireInTextInputs: boolean;
  /** When true, the shortcut only fires inside a popout window. */
  popoutOnly?: boolean;
  /** When true, the shortcut only fires in the main window. */
  mainWindowOnly?: boolean;
  /** Handler — receives the per-press context and runs the action. */
  run: (ctx: ShortcutCtx) => void;
}

// ─── Custom events for cross-component bridges ────────────────────
//
// Several shortcuts need to reach into a specific component's local
// state (chat-view's diff/context flags, header-actions's editor
// dropdown, the chat-toolbar selectors). Using window CustomEvents
// keeps the components in charge of their own state without lifting
// it into a context every shortcut would have to read.

export const TOGGLE_DIFF_EVENT = "flowstate:toggle-diff";
export const TOGGLE_CONTEXT_EVENT = "flowstate:toggle-context";
export const TOGGLE_CODE_VIEW_EVENT = "flowstate:toggle-code-view";
export const OPEN_EDITOR_PICKER_EVENT = "flowstate:open-editor-picker";
export const LAUNCH_DEFAULT_EDITOR_EVENT = "flowstate:launch-default-editor";
export const OPEN_MODEL_PICKER_EVENT = "flowstate:open-model-picker";
export const OPEN_EFFORT_PICKER_EVENT = "flowstate:open-effort-picker";
export const ADD_PROJECT_EVENT = "flowstate:add-project";
// Dispatched by toolbar pickers (model / effort) when they close so the
// chat composer can reclaim focus. Without this, Radix's default
// `onCloseAutoFocus` returns focus to the trigger button, leaving the
// user reaching for the mouse after a ⌘⇧M / ⌘⇧E + Enter selection.
// The chat-input component listens and refocuses its textarea.
export const FOCUS_CHAT_INPUT_EVENT = "flowstate:focus-chat-input";

function cycleThread(ctx: ShortcutCtx, direction: 1 | -1): void {
  const list = ctx.projectSessions;
  if (list.length === 0) return;
  if (list.length === 1) {
    ctx.notify?.("No other threads in this project");
    return;
  }
  const currentIdx = ctx.activeSessionId
    ? list.findIndex((s) => s.sessionId === ctx.activeSessionId)
    : -1;
  const len = list.length;
  const nextIdx =
    currentIdx === -1
      ? direction === 1
        ? 0
        : len - 1
      : (currentIdx + direction + len) % len;
  const next = list[nextIdx];
  if (!next) return;
  ctx.navigate({
    to: "/chat/$sessionId",
    params: { sessionId: next.sessionId },
  });
}

export const SHORTCUTS: Shortcut[] = [
  {
    id: "toggle-diff",
    label: "Toggle git diff panel",
    defaultBinding: "mod+shift+d",
    group: "View",
    fireInTextInputs: true,
    run: () => window.dispatchEvent(new CustomEvent(TOGGLE_DIFF_EVENT)),
  },
  {
    id: "toggle-context",
    label: "Toggle agent context panel",
    defaultBinding: "mod+shift+k",
    group: "View",
    fireInTextInputs: true,
    run: () => window.dispatchEvent(new CustomEvent(TOGGLE_CONTEXT_EVENT)),
  },
  {
    id: "toggle-code-view",
    label: "Toggle code view panel",
    defaultBinding: "mod+alt+e",
    group: "View",
    fireInTextInputs: true,
    run: () => window.dispatchEvent(new CustomEvent(TOGGLE_CODE_VIEW_EVENT)),
  },
  {
    id: "popout-thread",
    label: "Pop out current thread",
    defaultBinding: "mod+t",
    group: "View",
    fireInTextInputs: true,
    mainWindowOnly: true,
    run: (ctx) => {
      // Defensive guard — the mainWindowOnly filter in the dispatcher
      // already prevents this from firing inside a popout, but a
      // shared shell that mounts the dispatcher in main mode from
      // inside a popout would still reach here without it.
      if (isPopoutWindow()) return;
      if (!ctx.activeSessionId) {
        ctx.notify?.("Open a thread first to pop it out");
        return;
      }
      void popoutThread(ctx.activeSessionId).catch((err) => {
        ctx.notify?.(`Pop-out failed: ${String(err)}`);
      });
    },
  },
  {
    id: "popout-pin-toggle",
    label: "Toggle always-on-top",
    defaultBinding: "mod+shift+t",
    group: "View",
    fireInTextInputs: true,
    popoutOnly: true,
    run: (ctx) => {
      const next = !readPopoutPinPref();
      void setPopoutPinned(next).catch((err) => {
        ctx.notify?.(`Pin toggle failed: ${String(err)}`);
      });
    },
  },
  {
    id: "launch-default-editor",
    label: "Open in default editor",
    defaultBinding: "mod+o",
    group: "View",
    // Fires even when the chat composer (or any other text input) is
    // focused — see useGlobalShortcuts. Without this the browser's
    // built-in "Open File…" dialog would hijack ⌘O whenever an input
    // had focus, since the dispatcher only calls preventDefault for
    // shortcuts it actually runs.
    fireInTextInputs: true,
    run: () =>
      window.dispatchEvent(new CustomEvent(LAUNCH_DEFAULT_EDITOR_EVENT)),
  },
  {
    id: "open-editor-picker",
    label: "Open editor picker",
    defaultBinding: "mod+shift+o",
    group: "View",
    fireInTextInputs: true,
    run: () => window.dispatchEvent(new CustomEvent(OPEN_EDITOR_PICKER_EVENT)),
  },
  {
    // Documentation entry. The actual binding is owned by the CM6
    // `commentExtension` keymap (Prec.high) — when a file is focused
    // its `.cm-content` is the active text input, so this global
    // entry exits early via `!fireInTextInputs && isInTextInput(...)`
    // and the editor's keymap handles the keystroke. Outside the
    // editor (e.g. focus on a button or document.body) the run
    // handler hints at where the shortcut belongs instead of
    // silently consuming the keystroke.
    id: "comment-on-line",
    label: "Comment on current line",
    defaultBinding: "mod+alt+c",
    group: "View",
    fireInTextInputs: false,
    run: (ctx) => {
      ctx.notify?.(
        "Open a file in the code view to add a line comment",
      );
    },
  },
  {
    id: "open-model-picker",
    label: "Open model picker",
    defaultBinding: "mod+shift+m",
    group: "View",
    fireInTextInputs: true,
    run: () => window.dispatchEvent(new CustomEvent(OPEN_MODEL_PICKER_EVENT)),
  },
  {
    id: "open-effort-picker",
    label: "Open effort picker",
    defaultBinding: "mod+shift+e",
    group: "View",
    fireInTextInputs: true,
    run: () => window.dispatchEvent(new CustomEvent(OPEN_EFFORT_PICKER_EVENT)),
  },
  {
    id: "next-thread",
    label: "Next thread",
    defaultBinding: "mod+]",
    group: "Navigation",
    fireInTextInputs: true,
    run: (ctx) => cycleThread(ctx, 1),
  },
  {
    id: "prev-thread",
    label: "Previous thread",
    defaultBinding: "mod+[",
    group: "Navigation",
    fireInTextInputs: true,
    run: (ctx) => cycleThread(ctx, -1),
  },
  {
    id: "open-file-search",
    label: "Search files",
    defaultBinding: "mod+p",
    group: "Navigation",
    fireInTextInputs: true,
    run: (ctx) => {
      if (!ctx.activeSessionId) {
        ctx.notify?.("Open a thread first to search its project files");
        return;
      }
      ctx.navigate({
        to: "/code/$sessionId",
        params: { sessionId: ctx.activeSessionId },
        search: { mode: "files" },
      });
    },
  },
  {
    id: "open-content-search",
    label: "Search file contents",
    defaultBinding: "mod+shift+f",
    group: "Navigation",
    fireInTextInputs: true,
    run: (ctx) => {
      if (!ctx.activeSessionId) {
        ctx.notify?.("Open a thread first to search its project contents");
        return;
      }
      ctx.navigate({
        to: "/code/$sessionId",
        params: { sessionId: ctx.activeSessionId },
        search: { mode: "content" },
      });
    },
  },
  {
    id: "new-thread-current-project",
    label: "New thread (current project)",
    defaultBinding: "mod+n",
    group: "Navigation",
    fireInTextInputs: true,
    run: (ctx) => {
      void ctx.startThreadOnCurrentProject();
    },
  },
  {
    id: "new-thread-pick-project",
    label: "New thread (pick project)",
    defaultBinding: "mod+shift+n",
    group: "Navigation",
    fireInTextInputs: true,
    run: (ctx) => ctx.openProjectPicker(),
  },
  {
    id: "add-project",
    label: "Add project (pick folder)",
    defaultBinding: "mod+alt+n",
    group: "Navigation",
    fireInTextInputs: true,
    run: () => window.dispatchEvent(new CustomEvent(ADD_PROJECT_EVENT)),
  },
  {
    id: "show-shortcuts",
    label: "Show keyboard shortcuts",
    // `?` is what Shift+/ delivers on most US layouts; the matcher
    // also accepts `/` with shiftKey via the dsl.ts fallback so other
    // layouts still reach this row.
    defaultBinding: "mod+shift+/",
    group: "Help",
    fireInTextInputs: true,
    run: (ctx) => ctx.openShortcutsHelp(),
  },
];

/**
 * Resolve a shortcut's effective binding by overlaying the override
 * store on top of its `defaultBinding`. Returns `null` if the user
 * cleared the binding entirely (rebound to nothing — no shortcut)
 * which is handled later as "this row never fires".
 *
 * The `null` case is intentional: users sometimes want to disarm a
 * shortcut without rebinding it (e.g. an accidental ⌘N during a
 * presentation). Storing the empty string as the override is the
 * sentinel.
 */
export function effectiveBinding(
  s: Shortcut,
  overrides: ShortcutOverrideStore,
): string | null {
  const override = overrides.get(s.id);
  if (override === null) return s.defaultBinding;
  if (override === "") return null;
  return override;
}

/**
 * Detect chord collisions across the active set in a given window
 * scope. Bucketizes by `${chord-canonical-form}::${scope-tag}` and
 * returns a list of `[chord, ids[]]` pairs that have more than one
 * occupant. Surface in dev as a console warning; the rebinding UI
 * (when it exists) will surface them inline next to the offending
 * row.
 */
export function detectConflicts(
  shortcuts: readonly Shortcut[],
  overrides: ShortcutOverrideStore,
  inPopout: boolean,
): { binding: string; ids: string[] }[] {
  const buckets = new Map<string, string[]>();
  for (const s of shortcuts) {
    if (s.popoutOnly && !inPopout) continue;
    if (s.mainWindowOnly && inPopout) continue;
    const dsl = effectiveBinding(s, overrides);
    if (dsl === null) continue;
    let canonical: string;
    try {
      const parsed = parseDsl(dsl);
      // Canonical key for collision: every modifier flag in a fixed
      // order so "mod+shift+d" and "shift+mod+d" bucket together.
      canonical = `${parsed.mod ? "M" : "_"}${parsed.shift ? "S" : "_"}${
        parsed.alt ? "A" : "_"
      }${parsed.ctrl ? "C" : "_"}${parsed.meta ? "X" : "_"}+${parsed.key}`;
    } catch {
      // Malformed override — skip the row. The dispatcher handles the
      // same case; we don't want a bad override to hide a real
      // collision elsewhere.
      continue;
    }
    const list = buckets.get(canonical) ?? [];
    list.push(s.id);
    buckets.set(canonical, list);
  }
  const conflicts: { binding: string; ids: string[] }[] = [];
  for (const [chord, ids] of buckets) {
    if (ids.length > 1) conflicts.push({ binding: chord, ids });
  }
  return conflicts;
}

// ─── help cheatsheet ───────────────────────────────────────────────

/** Read-only command-palette-style modal listing every registered
 *  shortcut, with a fuzzy filter at the top. Rendered key chips come
 *  from `formatChord(parseDsl(effectiveBinding(...)))` so the cheatsheet
 *  always reflects the user's overrides AND the platform's glyphs
 *  without per-entry hand-coding. */
export function ShortcutsDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const overrides = getOverrideStore();
  // Subscribe so a rebinding write (from devtools today, the future
  // Settings UI tomorrow) re-renders the dialog with the new chips
  // even if it was already open.
  const overrideTick = useOverrideTick(overrides);
  void overrideTick;

  const groups = React.useMemo(() => {
    const inPopout = isPopoutWindow();
    const order: ShortcutGroup[] = ["Navigation", "View", "Help"];
    const byGroup = new Map<ShortcutGroup, Shortcut[]>();
    for (const s of SHORTCUTS) {
      if (s.popoutOnly && !inPopout) continue;
      if (s.mainWindowOnly && inPopout) continue;
      const list = byGroup.get(s.group) ?? [];
      list.push(s);
      byGroup.set(s.group, list);
    }
    return order
      .map((name) => ({ name, items: byGroup.get(name) ?? [] }))
      .filter((g) => g.items.length > 0);
    // overrideTick included in deps below to re-bucket when an
    // override changes (cheap; the registry is single-digit entries).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [overrideTick]);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-lg overflow-hidden p-0">
        <DialogHeader className="sr-only">
          <DialogTitle>Keyboard shortcuts</DialogTitle>
          <DialogDescription>
            Search and review the keyboard shortcuts available in
            Flowstate.
          </DialogDescription>
        </DialogHeader>
        <Command className="rounded-xl">
          <CommandInput placeholder="Search shortcuts…" autoFocus />
          <CommandList className="max-h-[60vh]">
            <CommandEmpty>No matching shortcut.</CommandEmpty>
            {groups.map((g) => (
              <CommandGroup key={g.name} heading={g.name}>
                {g.items.map((s) => (
                  <ShortcutRow
                    key={s.id}
                    shortcut={s}
                    overrides={overrides}
                    onSelect={() => onOpenChange(false)}
                  />
                ))}
              </CommandGroup>
            ))}
          </CommandList>
        </Command>
      </DialogContent>
    </Dialog>
  );
}

function ShortcutRow({
  shortcut,
  overrides,
  onSelect,
}: {
  shortcut: Shortcut;
  overrides: ShortcutOverrideStore;
  onSelect: () => void;
}) {
  const dsl = effectiveBinding(shortcut, overrides);
  const chips = React.useMemo(() => {
    if (dsl === null) return [] as string[];
    try {
      return formatChord(parseDsl(dsl));
    } catch {
      // A bad override (someone dropped an invalid string into
      // localStorage) shouldn't crash the cheatsheet — render the raw
      // DSL as a single chip so the user can see what's stored.
      return [dsl];
    }
  }, [dsl]);
  return (
    <CommandItem
      // cmdk substring-filters on `value`; combine label + DSL so
      // typing either the action name OR the key sequence finds the
      // row.
      value={`${shortcut.label} ${dsl ?? ""}`}
      onSelect={onSelect}
    >
      <span className="flex-1 truncate">{shortcut.label}</span>
      {dsl === null ? (
        <span className="ml-auto text-[10px] italic text-muted-foreground">
          unbound
        </span>
      ) : (
        <ShortcutKeys keys={chips} />
      )}
    </CommandItem>
  );
}

function ShortcutKeys({ keys }: { keys: string[] }) {
  return (
    <span className="ml-auto flex items-center gap-1">
      {keys.map((k, i) => (
        <kbd
          key={`${k}-${i}`}
          className="inline-flex h-5 min-w-5 items-center justify-center rounded border border-border bg-muted px-1 font-mono text-[10px] font-medium text-muted-foreground"
        >
          {k}
        </kbd>
      ))}
    </span>
  );
}

/**
 * Subscribe-and-tick hook for the override store. Returns a counter
 * that bumps every time the store fires `subscribe`; callers add it
 * to their useMemo dep list so they re-derive on override changes.
 *
 * Lives here (not in overrides.ts) because it's the only React glue
 * the store needs — keeping the store framework-agnostic lets a
 * future SQLite-backed swap stay React-free.
 */
function useOverrideTick(overrides: ShortcutOverrideStore): number {
  const [tick, setTick] = React.useState(0);
  React.useEffect(() => {
    return overrides.subscribe(() => setTick((t) => t + 1));
  }, [overrides]);
  return tick;
}
