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
import type { SessionSummary } from "@/lib/types";

// ─── shortcut registry ─────────────────────────────────────────────
//
// Single source of truth for app-wide keyboard shortcuts. Both the
// global keydown handler (`useGlobalShortcuts`) and the help cheatsheet
// (`ShortcutsDialog`) read from this same array so a new shortcut only
// has to be defined once. Per-view shortcuts (CodeView's tab bar,
// Shift+Tab mode cycling, ⌘J terminal toggle, etc.) intentionally stay
// in their own files — this registry is only for *global* shortcuts
// that fire regardless of focus or current route.

export type ShortcutGroup = "Navigation" | "View" | "Help";

export interface ShortcutCtx {
  navigate: (opts: NavigateArg) => void;
  /** sessionId of the thread the user is currently viewing, or null
   *  when on a non-chat route. Used by toggle-diff (which targets
   *  whichever ChatView is mounted via custom event), ⌘P / ⌘⇧F (which
   *  jump to /code/$sessionId), and prev/next thread. */
  activeSessionId: string | null;
  /**
   * EVERY thread the sidebar would show, ordered project-by-project
   * in sidebar order, then thread-by-thread within each project. This
   * is what Cmd+] / Cmd+[ walks — at the end of project A's threads,
   * the next press lands on the first thread of project B (matching
   * how the user reads the sidebar top-to-bottom). Field name kept
   * as `projectSessions` for back-compat with existing callers /
   * tests; semantic was widened from "current project only" to
   * "all sidebar threads".
   */
  projectSessions: SessionSummary[];
  /** Open the cheatsheet modal. */
  openShortcutsHelp: () => void;
  /** Optional UI-feedback hook (toast in production, no-op in tests). */
  notify?: (message: string) => void;
}

// We use a structural arg type rather than importing the TanStack
// `NavigateOptions` directly to keep this file decoupled from the
// route tree — that way the registry can be unit-tested without
// pulling the router in.
type NavigateArg =
  | { to: "/chat/$sessionId"; params: { sessionId: string } }
  | {
      to: "/code/$sessionId";
      params: { sessionId: string };
      search?: { mode?: "files" | "content" };
    };

export interface Shortcut {
  id: string;
  /** Human-readable label shown in the cheatsheet. */
  label: string;
  /** Display-only key chips, e.g. ["⌘", "⇧", "D"]. Order matters for
   *  rendering. */
  keys: string[];
  group: ShortcutGroup;
  /** Whether the shortcut should also fire when focus is inside an
   *  input/textarea/contenteditable. The user's brief explicitly
   *  required this for *every* registered shortcut, but the flag is
   *  here so future additions can opt out without forking the hook. */
  fireInTextInputs: boolean;
  /** Pure predicate over the raw event. Returning true means "this
   *  shortcut owns this keydown" — the handler then preventDefaults
   *  and calls `run`. */
  match: (e: KeyboardEvent) => boolean;
  run: (ctx: ShortcutCtx) => void;
}

/** Custom event the diff toggle dispatches. Listened to by chat-view.tsx.
 *  Module-level constant so both sides import the same string. */
export const TOGGLE_DIFF_EVENT = "flowstate:toggle-diff";

function hasMod(e: KeyboardEvent): boolean {
  return e.metaKey || e.ctrlKey;
}

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
  // Wrap cleanly in either direction; if the active thread isn't in
  // the list (e.g. user is on /settings), jump to the first/last.
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
    keys: ["⌘", "⇧", "D"],
    group: "View",
    fireInTextInputs: true,
    match: (e) =>
      hasMod(e) && e.shiftKey && !e.altKey && e.key.toLowerCase() === "d",
    run: () => {
      // Fire-and-forget: chat-view.tsx subscribes to this event and
      // flips its local `diffOpen` state. Lives outside the React
      // tree intentionally — keeps state ownership in chat-view
      // without lifting it into a context.
      window.dispatchEvent(new CustomEvent(TOGGLE_DIFF_EVENT));
    },
  },
  {
    id: "next-thread",
    label: "Next thread",
    keys: ["⌘", "]"],
    group: "Navigation",
    fireInTextInputs: true,
    match: (e) => hasMod(e) && !e.shiftKey && !e.altKey && e.key === "]",
    run: (ctx) => cycleThread(ctx, 1),
  },
  {
    id: "prev-thread",
    label: "Previous thread",
    keys: ["⌘", "["],
    group: "Navigation",
    fireInTextInputs: true,
    match: (e) => hasMod(e) && !e.shiftKey && !e.altKey && e.key === "[",
    run: (ctx) => cycleThread(ctx, -1),
  },
  {
    id: "open-file-search",
    label: "Search files",
    keys: ["⌘", "P"],
    group: "Navigation",
    fireInTextInputs: true,
    match: (e) =>
      hasMod(e) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "p",
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
    keys: ["⌘", "⇧", "F"],
    group: "Navigation",
    fireInTextInputs: true,
    match: (e) =>
      hasMod(e) && e.shiftKey && !e.altKey && e.key.toLowerCase() === "f",
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
    id: "show-shortcuts",
    label: "Show keyboard shortcuts",
    keys: ["⌘", "⇧", "?"],
    group: "Help",
    fireInTextInputs: true,
    // On most US keyboards Shift+/ delivers `event.key === "?"`, but
    // some layouts/IMEs deliver `"/"` plus shiftKey instead. Accept
    // both so the cheatsheet stays reachable across layouts.
    match: (e) =>
      hasMod(e) &&
      e.shiftKey &&
      !e.altKey &&
      (e.key === "?" || e.key === "/"),
    run: (ctx) => ctx.openShortcutsHelp(),
  },
];

// ─── help cheatsheet ───────────────────────────────────────────────

/** Read-only command-palette-style modal listing every registered
 *  shortcut, with a fuzzy filter at the top. Items are not actionable
 *  — selecting one is a no-op. The dialog itself is reachable via
 *  ⌘⇧? (the last entry in `SHORTCUTS`). */
export function ShortcutsDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  // Pre-group once per render. The list is static, so this is cheap.
  const groups = React.useMemo(() => {
    const order: ShortcutGroup[] = ["Navigation", "View", "Help"];
    const byGroup = new Map<ShortcutGroup, Shortcut[]>();
    for (const s of SHORTCUTS) {
      const list = byGroup.get(s.group) ?? [];
      list.push(s);
      byGroup.set(s.group, list);
    }
    return order
      .map((name) => ({ name, items: byGroup.get(name) ?? [] }))
      .filter((g) => g.items.length > 0);
  }, []);

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
        <Command
          // `cmdk` filters by item value substrings; we set a value
          // string per-item below combining label + keys so users can
          // type either "diff" or "shift+d" and find the same row.
          className="rounded-xl"
        >
          <CommandInput
            placeholder="Search shortcuts…"
            autoFocus
          />
          <CommandList className="max-h-[60vh]">
            <CommandEmpty>No matching shortcut.</CommandEmpty>
            {groups.map((g) => (
              <CommandGroup key={g.name} heading={g.name}>
                {g.items.map((s) => (
                  <CommandItem
                    key={s.id}
                    value={`${s.label} ${s.keys.join(" ")}`}
                    // Decorative — selecting an item just closes the
                    // dialog. Nothing here mutates app state.
                    onSelect={() => onOpenChange(false)}
                  >
                    <span className="flex-1 truncate">{s.label}</span>
                    <ShortcutKeys keys={s.keys} />
                  </CommandItem>
                ))}
              </CommandGroup>
            ))}
          </CommandList>
        </Command>
      </DialogContent>
    </Dialog>
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

