import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { useApp } from "@/stores/app-store";
import { toast } from "@/hooks/use-toast";
import {
  SHORTCUTS,
  type ShortcutCtx,
} from "@/lib/keyboard-shortcuts";
import type { SessionSummary } from "@/lib/types";

// Single global keydown listener that walks the SHORTCUTS registry
// and runs the first matching entry. Mounted exactly once in
// `AppShell` (router.tsx) — never in PopoutShell, since popouts only
// host a single thread and don't need cross-thread navigation. The
// help dialog's open state is owned by the caller so the same hook
// can be tested in isolation.
//
// Pattern follows useModeCycleShortcut.ts: one useEffect, attaches
// to `window`, returns a cleanup. Unlike that hook, we do NOT skip
// when focus is on input/textarea — the user explicitly required
// every registered shortcut to fire even from inside the composer.
// Each shortcut still calls preventDefault to suppress browser
// defaults like ⌘P (print), ⌘[ / ⌘] (history nav), ⌘⇧F (fullscreen).
export function useGlobalShortcuts(params: {
  openShortcutsHelp: () => void;
}): void {
  const { openShortcutsHelp } = params;
  const { state } = useApp();
  const navigate = useNavigate();

  // Build the per-press context lazily inside the listener so the
  // event always sees the freshest state — if we closed over `state`
  // directly, the listener would only see the snapshot at mount and
  // miss any session/project added later.
  const stateRef = React.useRef(state);
  React.useEffect(() => {
    stateRef.current = state;
  }, [state]);

  React.useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      // First match wins. The registry is small (single-digit
      // entries) so a linear scan per keydown is fine — no need for
      // a key-indexed map.
      for (const shortcut of SHORTCUTS) {
        if (!shortcut.match(e)) continue;

        if (!shortcut.fireInTextInputs && isInTextInput(e.target)) {
          return;
        }

        e.preventDefault();
        const ctx = buildCtx(stateRef.current, navigate, openShortcutsHelp);
        shortcut.run(ctx);
        return;
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [navigate, openShortcutsHelp]);
}

function isInTextInput(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName;
  return (
    tag === "INPUT" ||
    tag === "TEXTAREA" ||
    target.isContentEditable === true
  );
}

// Build the full prev/next-thread cycle list: every thread in the
// sidebar, ordered project-by-project (sidebar's `sortOrder`, then
// alphabetical), then thread-by-thread within each project (newest
// first to mirror the sidebar's per-project ordering). At the end of
// project A's threads, Cmd+] lands on the first thread of project B
// — keys stay simple, the cycle just spans more.
//
// Worktree-child projects roll up under their parent (matching
// app-sidebar.tsx's `sortedActiveProjects` filter), so a worktree's
// threads appear as part of the parent project's group rather than
// in their own bucket. Threads attached to projects flowstate
// doesn't know about (deleted/tombstoned) are filtered out — they
// have no sidebar row, so cycling onto them would be invisible.
function getAllSessionsInSidebarOrder(
  state: ReturnType<typeof useApp>["state"],
): SessionSummary[] {
  // 1. Resolve the project ordering the sidebar uses. Mirrors
  //    `sortedActiveProjects` in app-sidebar.tsx so the cycle order
  //    matches what the user reads top-to-bottom.
  const worktreeIds = new Set(state.projectWorktrees.keys());
  const nameFor = (projectId: string) =>
    state.projectDisplay.get(projectId)?.name ?? "Untitled project";
  const sortedProjects = state.projects
    .filter((p) => !worktreeIds.has(p.projectId))
    .slice()
    .sort((a, b) => {
      const oa = state.projectDisplay.get(a.projectId)?.sortOrder;
      const ob = state.projectDisplay.get(b.projectId)?.sortOrder;
      if (oa == null && ob == null) {
        return nameFor(a.projectId).localeCompare(nameFor(b.projectId));
      }
      if (oa == null) return 1;
      if (ob == null) return -1;
      return oa - ob;
    });

  // 2. Bucket every session by its effective project id (parent of a
  //    worktree, or the project itself).
  const knownProjectIds = new Set(state.projects.map((p) => p.projectId));
  const buckets = new Map<string, SessionSummary[]>();
  for (const session of state.sessions.values()) {
    if (!session.projectId) continue;
    if (!knownProjectIds.has(session.projectId)) continue;
    const effective =
      state.projectWorktrees.get(session.projectId)?.parentProjectId ??
      session.projectId;
    const list = buckets.get(effective) ?? [];
    list.push(session);
    buckets.set(effective, list);
  }
  for (const list of buckets.values()) {
    list.sort(
      (a, b) =>
        new Date(b.createdAt).getTime() - new Date(a.createdAt).getTime(),
    );
  }

  // 3. Flatten in sidebar project order so the cycle spans every
  //    thread without hopping back and forth.
  const flat: SessionSummary[] = [];
  for (const project of sortedProjects) {
    const list = buckets.get(project.projectId);
    if (list) flat.push(...list);
  }
  return flat;
}

function buildCtx(
  state: ReturnType<typeof useApp>["state"],
  navigate: ReturnType<typeof useNavigate>,
  openShortcutsHelp: () => void,
): ShortcutCtx {
  return {
    activeSessionId: state.activeSessionId,
    projectSessions: getAllSessionsInSidebarOrder(state),
    openShortcutsHelp,
    notify: (message) => {
      toast({ description: message, duration: 2000 });
    },
    // The registry's NavigateArg is structurally a subset of TanStack's
    // `NavigateOptions`, but TanStack's type is a deeply-generic union
    // over the route tree that won't accept a literal-typed arg
    // directly. The double cast (`unknown` first) keeps the registry
    // decoupled from the route tree without weakening the call site
    // — both routes used here are registered in `routeTree`, so this
    // is sound at runtime.
    navigate: (opts) =>
      navigate(opts as unknown as Parameters<typeof navigate>[0]),
  };
}
