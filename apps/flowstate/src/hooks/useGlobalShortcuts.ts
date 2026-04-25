import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { useApp } from "@/stores/app-store";
import { toast } from "@/hooks/use-toast";
import { useDefaultProvider } from "@/hooks/use-default-provider";
import { readDefaultModel } from "@/lib/defaults-settings";
import { isPopoutWindow } from "@/lib/popout";
import {
  SHORTCUTS,
  detectConflicts,
  effectiveBinding,
  getOverrideStore,
  matchChord,
  parseDsl,
  type Shortcut,
  type ShortcutCtx,
} from "@/lib/keyboard";
import type { ProviderKind, SessionSummary } from "@/lib/types";

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
  /** Open the project picker dialog used by ⌘⇧N. Owned by AppShell so
   *  the picker can render alongside the cheatsheet dialog and reuse
   *  the same provider hooks the sidebar already uses. */
  openProjectPicker: () => void;
  /**
   * Which shell mounted the hook. "main" runs every non-`popoutOnly`
   * binding (the default — this is what AppShell uses). "popout"
   * restricts dispatch to `popoutOnly: true` entries only, so the
   * popout window can still respond to ⌘⇧T (always-on-top) without
   * also firing ⌘N / ⌘P / ⌘⇧? — those depend on UI mounted only in
   * the main window's AppShell (project picker dialog, help dialog,
   * sidebar-driven nav) and would silently no-op or feel broken.
   */
  mode?: "main" | "popout";
}): void {
  const { openShortcutsHelp, openProjectPicker, mode = "main" } = params;
  const { state, send } = useApp();
  const navigate = useNavigate();
  // The default-provider preference is async (SQLite-backed). Reading
  // it inside the listener directly would either need to be async on
  // the keypress path (breaks preventDefault flow) or block on a
  // ref. Pre-resolve at the hook layer and stash so the handler is a
  // synchronous read.
  const { defaultProvider, loaded: defaultProviderLoaded } =
    useDefaultProvider();

  // Build the per-press context lazily inside the listener so the
  // event always sees the freshest state — if we closed over `state`
  // directly, the listener would only see the snapshot at mount and
  // miss any session/project added later.
  const stateRef = React.useRef(state);
  React.useEffect(() => {
    stateRef.current = state;
  }, [state]);
  const sendRef = React.useRef(send);
  React.useEffect(() => {
    sendRef.current = send;
  }, [send]);
  const defaultProviderRef = React.useRef(defaultProvider);
  React.useEffect(() => {
    defaultProviderRef.current = defaultProvider;
  }, [defaultProvider]);
  const defaultProviderLoadedRef = React.useRef(defaultProviderLoaded);
  React.useEffect(() => {
    defaultProviderLoadedRef.current = defaultProviderLoaded;
  }, [defaultProviderLoaded]);

  // Active bindings — DSL strings resolved through the override store.
  // Recomputed when overrides change so a rebinding is live without a
  // page reload. The parsed chord is cached alongside so the keydown
  // path is a cheap field compare per entry.
  const overrides = React.useMemo(() => getOverrideStore(), []);
  const [overrideTick, setOverrideTick] = React.useState(0);
  React.useEffect(() => {
    return overrides.subscribe(() => setOverrideTick((t) => t + 1));
  }, [overrides]);

  type ResolvedShortcut = {
    shortcut: Shortcut;
    chord: ReturnType<typeof parseDsl>;
  };
  const resolved = React.useMemo<ResolvedShortcut[]>(() => {
    const out: ResolvedShortcut[] = [];
    for (const s of SHORTCUTS) {
      const dsl = effectiveBinding(s, overrides);
      if (dsl === null) continue; // user explicitly unbound this row
      try {
        out.push({ shortcut: s, chord: parseDsl(dsl) });
      } catch (err) {
        // Bad override silently dropped (the cheatsheet renders the
        // raw DSL chip so the user sees what's broken). Logging here
        // helps when debugging from a console.
        // eslint-disable-next-line no-console
        console.warn(
          `keyboard: failed to parse binding for "${s.id}" ("${dsl}"): ${String(err)}`,
        );
      }
    }
    return out;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [overrides, overrideTick]);

  // Conflict detection — runs whenever the resolved set changes.
  // Logs to console so the dev catches collisions early; the future
  // rebinding UI will surface them inline next to each row.
  React.useEffect(() => {
    const inPopout = isPopoutWindow();
    const conflicts = detectConflicts(SHORTCUTS, overrides, inPopout);
    if (conflicts.length === 0) return;
    for (const c of conflicts) {
      // eslint-disable-next-line no-console
      console.warn(
        `keyboard: conflict — multiple bindings match "${c.binding}": ${c.ids.join(", ")}`,
      );
    }
  }, [overrides, overrideTick]);

  React.useEffect(() => {
    const inPopout = isPopoutWindow();
    function onKeyDown(e: KeyboardEvent) {
      // First match wins. The registry is small so a linear scan per
      // keydown is fine — no need for a key-indexed map.
      for (const { shortcut, chord } of resolved) {
        if (!matchChord(chord, e)) continue;

        if (!shortcut.fireInTextInputs && isInTextInput(e.target)) {
          return;
        }
        // Mode filter. The popout shell mounts this hook in "popout"
        // mode and only runs `popoutOnly` bindings — every other
        // shortcut depends on UI mounted in the main AppShell
        // (project picker, help dialog, etc.) or has semantics that
        // don't transfer (⌘N would create a thread the popout can't
        // navigate to). The main shell uses default "main" and skips
        // popout-only entries so a popout pin keystroke can't
        // accidentally fire there.
        if (mode === "popout" && !shortcut.popoutOnly) continue;
        if (mode === "main" && shortcut.popoutOnly) continue;
        // Window-scope safety net. `mainWindowOnly` is redundant with
        // the mode check above when mode="popout" but stays for
        // belt-and-braces in case someone calls the hook in main mode
        // from inside a popout (e.g. a future shared shell).
        if (shortcut.mainWindowOnly && inPopout) continue;

        e.preventDefault();
        const ctx = buildCtx({
          state: stateRef.current,
          navigate,
          openShortcutsHelp,
          openProjectPicker,
          sendMsg: sendRef.current,
          defaultProvider: defaultProviderRef.current,
          defaultProviderLoaded: defaultProviderLoadedRef.current,
        });
        shortcut.run(ctx);
        return;
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [navigate, openShortcutsHelp, openProjectPicker, mode, resolved]);
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

/** Resolve the active session's "effective" project id — the parent
 *  of a worktree row, or the project itself. Mirrors the rollup the
 *  sidebar uses so ⌘N feels like "new thread in the row I'm reading
 *  the sidebar from". */
function effectiveActiveProjectId(
  state: ReturnType<typeof useApp>["state"],
): string | null {
  const activeId = state.activeSessionId;
  const active = activeId ? state.sessions.get(activeId) : undefined;
  if (!active?.projectId) return null;
  return (
    state.projectWorktrees.get(active.projectId)?.parentProjectId ??
    active.projectId
  );
}

interface BuildCtxArgs {
  state: ReturnType<typeof useApp>["state"];
  navigate: ReturnType<typeof useNavigate>;
  openShortcutsHelp: () => void;
  openProjectPicker: () => void;
  sendMsg: ReturnType<typeof useApp>["send"];
  defaultProvider: ProviderKind;
  defaultProviderLoaded: boolean;
}

function buildCtx(args: BuildCtxArgs): ShortcutCtx {
  const {
    state,
    navigate,
    openShortcutsHelp,
    openProjectPicker,
    sendMsg,
    defaultProvider,
    defaultProviderLoaded,
  } = args;
  const notify = (message: string) => {
    toast({ description: message, duration: 2000 });
  };
  return {
    activeSessionId: state.activeSessionId,
    projectSessions: getAllSessionsInSidebarOrder(state),
    openShortcutsHelp,
    openProjectPicker,
    notify,
    // The registry's NavigateArg is structurally a subset of TanStack's
    // `NavigateOptions`, but TanStack's type is a deeply-generic union
    // over the route tree that won't accept a literal-typed arg
    // directly. The double cast (`unknown` first) keeps the registry
    // decoupled from the route tree without weakening the call site
    // — both routes used here are registered in `routeTree`, so this
    // is sound at runtime.
    navigate: (opts) =>
      navigate(opts as unknown as Parameters<typeof navigate>[0]),
    startThreadOnCurrentProject: async () => {
      // Resolve "current project" the same way the sidebar groups
      // threads — walking through worktree links so a popout from a
      // worktree thread still lands the new thread on the parent
      // project (which is what the user sees as the active row).
      const projectId = effectiveActiveProjectId(state);
      if (!projectId) {
        notify("Open a thread first to start one in its project");
        return;
      }
      if (!defaultProviderLoaded) {
        // Keep the UX honest — falling through to the constant
        // DEFAULT_PROVIDER would silently disregard a saved choice
        // that just hadn't loaded yet. Same guard the sidebar's
        // worktree-new-thread dropdown uses for the same reason.
        notify("Default provider still loading… try again in a moment");
        return;
      }
      try {
        const model = await readDefaultModel(defaultProvider);
        const res = await sendMsg({
          type: "start_session",
          provider: defaultProvider,
          model: model ?? undefined,
          project_id: projectId,
        });
        if (res?.type === "session_created") {
          navigate({
            to: "/chat/$sessionId",
            params: { sessionId: res.session.sessionId },
          } as unknown as Parameters<typeof navigate>[0]);
        }
      } catch (err) {
        notify(`Failed to start thread: ${String(err)}`);
      }
    },
  };
}

