import * as React from "react";
import { useMatches } from "@tanstack/react-router";
import { Plus, X } from "lucide-react";
import { useApp } from "@/stores/app-store";
import {
  NO_PROJECT_KEY,
  selectDockOpen,
  useTerminal,
} from "@/stores/terminal-store";
import { TerminalTab } from "./TerminalTab";
import "@xterm/xterm/css/xterm.css";

const DOCK_MIN_HEIGHT = 120;
const DOCK_MAX_HEIGHT = 900;

interface ActiveProject {
  projectKey: string;
  cwd: string | null;
  /** True once we can trust the answer. If there's a sessionId in
   *  the URL but the daemon hasn't sent its snapshot yet, we keep
   *  this `false` so the auto-open effect waits rather than
   *  spawning a shell in $HOME under NO_PROJECT_KEY. */
  resolved: boolean;
  /** Current thread, if any. Used to look up per-session dock open
   *  state via `selectDockOpen`. Null when the user is on a route
   *  without a session (home / /browse / /project/$projectId). */
  sessionId: string | null;
}

function useActiveProject(): ActiveProject {
  const { state } = useApp();
  const matches = useMatches();

  let sessionId: string | null = null;
  let projectId: string | null = null;
  for (const m of matches) {
    const params = m.params as Record<string, string> | undefined;
    if (params?.sessionId) {
      sessionId = params.sessionId;
      break;
    }
    if (params?.projectId) {
      projectId = params.projectId;
      break;
    }
  }

  // /project/$projectId — route is keyed directly by the project.
  // Resolve the cwd from state.projects so the dock reuses the
  // project's existing tab pool instead of falling back to
  // NO_PROJECT_KEY and spawning a throwaway $HOME shell.
  if (projectId) {
    if (!state.ready) {
      return {
        projectKey: NO_PROJECT_KEY,
        cwd: null,
        resolved: false,
        sessionId: null,
      };
    }
    const project = state.projects.find((p) => p.projectId === projectId);
    if (!project) {
      return {
        projectKey: NO_PROJECT_KEY,
        cwd: null,
        resolved: true,
        sessionId: null,
      };
    }
    return {
      projectKey: project.projectId,
      cwd: project.path ?? null,
      resolved: true,
      sessionId: null,
    };
  }

  // No session in the URL → we know this is NO_PROJECT_KEY. That
  // answer is stable, so resolved is true.
  if (!sessionId) {
    return {
      projectKey: NO_PROJECT_KEY,
      cwd: null,
      resolved: true,
      sessionId: null,
    };
  }

  // sessionId is in the URL but the daemon snapshot hasn't arrived
  // yet. Report unresolved so the dock holds off on auto-opening.
  if (!state.ready) {
    return {
      projectKey: NO_PROJECT_KEY,
      cwd: null,
      resolved: false,
      sessionId,
    };
  }

  const session =
    state.sessions.get(sessionId) ??
    state.archivedSessions.find((s) => s.sessionId === sessionId);
  // Snapshot is loaded but the session still isn't there — it may
  // arrive via a later event. Hold off.
  if (!session) {
    return {
      projectKey: NO_PROJECT_KEY,
      cwd: null,
      resolved: false,
      sessionId,
    };
  }
  if (!session.projectId) {
    return {
      projectKey: NO_PROJECT_KEY,
      cwd: null,
      resolved: true,
      sessionId,
    };
  }

  const project = state.projects.find((p) => p.projectId === session.projectId);
  if (!project) {
    return {
      projectKey: NO_PROJECT_KEY,
      cwd: null,
      resolved: true,
      sessionId,
    };
  }

  return {
    projectKey: project.projectId,
    cwd: project.path ?? null,
    resolved: true,
    sessionId,
  };
}

export function TerminalDock() {
  const { state, dispatch } = useTerminal();
  const { state: appState } = useApp();
  const { projectKey, cwd, resolved, sessionId } = useActiveProject();
  // Effective open flag for the current route: per-session override
  // if the user explicitly set one, otherwise the global default
  // (`defaultDockOpen`). Computed once so downstream reads stay
  // consistent across a single render.
  const dockOpen = selectDockOpen(state, sessionId);

  // Prune terminals for projects that no longer exist in app state
  // (user deleted the folder) OR whose last active session was
  // archived / deleted. Keeps NO_PROJECT_KEY alive. Also prunes
  // per-session dock-open entries so we don't leak booleans for
  // sessions the user has deleted or archived.
  React.useEffect(() => {
    const existingProjects = new Set(appState.projects.map((p) => p.projectId));

    // Collect projects that still have at least one active (non-archived) session.
    const activeProjectIds = new Set<string>();
    for (const session of appState.sessions.values()) {
      if (session.projectId) {
        activeProjectIds.add(session.projectId);
      }
    }

    // Keep only projects that exist AND have active sessions.
    const keep = new Set<string>();
    for (const pid of existingProjects) {
      if (activeProjectIds.has(pid)) {
        keep.add(pid);
      }
    }

    dispatch({ type: "prune_projects", keep });
    dispatch({
      type: "prune_sessions",
      keep: new Set(appState.sessions.keys()),
    });
  }, [appState.projects, appState.sessions, dispatch]);

  const projectState = state.projects.get(projectKey);

  // Auto-open a first tab when the dock is shown for a project
  // that has none yet. Gated on `resolved` so we don't create a
  // throwaway $HOME shell during the brief window between mount
  // and the daemon snapshot arriving. Project routes additionally
  // wait until `cwd` is a real path: we freeze `tab.cwd` at creation
  // time and pass it straight into TerminalTab's effect, so a tab
  // born with an empty cwd would later see its prop change and
  // rebuild the xterm/PTY from scratch.
  React.useEffect(() => {
    if (!dockOpen) return;
    if (!resolved) return;
    if (projectKey !== NO_PROJECT_KEY && !cwd) return;
    const current = state.projects.get(projectKey);
    if (!current || current.tabs.length === 0) {
      dispatch({
        type: "open_tab",
        projectKey,
        cwd: cwd ?? "",
      });
    }
  }, [dockOpen, resolved, projectKey, cwd, dispatch, state.projects]);

  const handleResize = React.useCallback(
    (startY: number, startHeight: number) => {
      function onMove(e: MouseEvent) {
        const delta = startY - e.clientY;
        const next = Math.max(
          DOCK_MIN_HEIGHT,
          Math.min(DOCK_MAX_HEIGHT, startHeight + delta),
        );
        dispatch({ type: "set_dock_height", height: next });
      }
      function onUp() {
        window.removeEventListener("mousemove", onMove);
        window.removeEventListener("mouseup", onUp);
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
      }
      document.body.style.cursor = "row-resize";
      document.body.style.userSelect = "none";
      window.addEventListener("mousemove", onMove);
      window.addEventListener("mouseup", onUp);
    },
    [dispatch],
  );

  const activeTab = projectState?.tabs[projectState.activeTabIndex];

  // NB: we do NOT early-return on !dockOpen. Unmounting the subtree
  // would dispose every xterm instance and kill every PTY, which
  // violates the "background instances stay alive" requirement.
  // Instead we toggle `display` so React keeps the tab components
  // mounted; the WebGL addon is released via the isVisible prop
  // below so hidden tabs don't hold a GPU context.
  return (
    <div
      className="absolute bottom-0 left-0 right-0 z-20 flex flex-col border-t border-border bg-[#0f0f10] text-xs"
      style={{
        height: state.dockHeight,
        display: dockOpen ? "flex" : "none",
      }}
    >
      {/* Drag handle */}
      <div
        role="separator"
        aria-label="Resize terminal"
        className="absolute -top-[3px] left-0 right-0 h-[6px] cursor-row-resize hover:bg-primary/30"
        onMouseDown={(e) => {
          e.preventDefault();
          handleResize(e.clientY, state.dockHeight);
        }}
      />

      {/* Tab strip */}
      <div className="flex h-8 shrink-0 items-center gap-0.5 border-b border-border px-1 text-muted-foreground">
        {projectState?.tabs.map((tab, idx) => {
          const active = idx === projectState.activeTabIndex;
          return (
            <div
              key={tab.id}
              className={`group/tab flex h-6 items-center gap-1 rounded-sm px-2 text-[11px] ${
                active
                  ? "bg-background text-foreground"
                  : "hover:bg-background/50"
              }`}
            >
              <button
                type="button"
                className="max-w-[160px] truncate"
                onClick={() =>
                  dispatch({
                    type: "set_active_tab",
                    projectKey,
                    tabId: tab.id,
                  })
                }
              >
                {tab.title}
              </button>
              <button
                type="button"
                aria-label="Close tab"
                className="opacity-0 hover:text-foreground group-hover/tab:opacity-100"
                onClick={() =>
                  dispatch({
                    type: "close_tab",
                    projectKey,
                    tabId: tab.id,
                  })
                }
              >
                <X className="h-3 w-3" />
              </button>
            </div>
          );
        })}
        <button
          type="button"
          aria-label="New terminal"
          className="flex h-6 w-6 items-center justify-center rounded-sm hover:bg-background/50"
          onClick={() =>
            dispatch({
              type: "open_tab",
              projectKey,
              cwd: cwd ?? "",
            })
          }
        >
          <Plus className="h-3.5 w-3.5" />
        </button>
        <div className="flex-1" />
        <button
          type="button"
          aria-label="Hide terminal"
          className="flex h-6 w-6 items-center justify-center rounded-sm hover:bg-background/50"
          onClick={() =>
            dispatch({ type: "set_dock_open", open: false, sessionId })
          }
        >
          <X className="h-3.5 w-3.5" />
        </button>
      </div>

      {/* Terminal grid area — ALL projects' tabs stay mounted so
          switching threads doesn't kill PTYs. Only the active
          project's active tab is visible; the rest use display:none
          (same pattern as inactive tabs within a single project). */}
      <div className="relative min-h-0 flex-1">
        {Array.from(state.projects.entries()).map(([pKey, pState]) => {
          const isActiveProject = pKey === projectKey;
          return pState.tabs.map((tab) => {
            const isActive = isActiveProject && tab.id === activeTab?.id;
            return (
              <div
                key={tab.id}
                className="absolute inset-0 p-1"
                style={{ display: isActive ? "block" : "none" }}
              >
                <TerminalTab
                  tabId={tab.id}
                  cwd={tab.cwd}
                  isVisible={dockOpen && isActive}
                  onTitleChange={(title) =>
                    dispatch({
                      type: "set_tab_title",
                      projectKey: pKey,
                      tabId: tab.id,
                      title,
                    })
                  }
                  onExit={() =>
                    dispatch({
                      type: "close_tab",
                      projectKey: pKey,
                      tabId: tab.id,
                    })
                  }
                />
              </div>
            );
          });
        })}
      </div>
    </div>
  );
}
