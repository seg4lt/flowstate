import * as React from "react";

// Integrated-terminal state. Scope: the dock's OPEN flag is now
// per-session (each thread independently remembers whether its
// dock is up), with a global fallback (`defaultDockOpen`) used by
// screens that have no active session (home, project view). Height
// stays global (persists to localStorage). The tab set is keyed by
// project id so switching to a different project swaps which tabs
// the dock shows. A folder-less session (no project) gets its own
// pool under NO_PROJECT_KEY.
//
// The store owns tab METADATA only — the live xterm.js instances
// and their PTYs are owned by the TerminalTab component itself.
// Background tabs (same project, not the active index) stay
// mounted but `display:none`, which lets xterm.js's built-in
// IntersectionObserver skip painting them. The PTY reader keeps
// running on the Rust side regardless, so a long `cargo build`
// doesn't freeze when the user switches to a sibling tab.
//
// All projects' tabs stay mounted (hidden via display:none) so
// switching threads/projects doesn't kill PTYs. Tabs are pruned
// when a project is deleted or its last active session is
// archived/deleted (see TerminalDock's prune_projects effect).

export const NO_PROJECT_KEY = "__none__";

export interface TerminalTab {
  /** Stable local id for React keys and store lookups. */
  id: string;
  /** Absolute path the shell was started in; also the dock title. */
  cwd: string;
  /** Display title — starts from cwd basename, shell may overwrite
   *  via OSC 2 (TerminalTab dispatches `set_tab_title` on change). */
  title: string;
}

interface ProjectTerminalState {
  tabs: TerminalTab[];
  activeTabIndex: number;
}

interface TerminalState {
  /** Global fallback for screens without an active session (home,
   *  /browse, project view). Persisted under DOCK_OPEN_KEY, so
   *  "dock was up when I last quit" still round-trips across app
   *  restarts on the home screen. */
  defaultDockOpen: boolean;
  /** Per-session dock open flag. When an entry exists it is the
   *  authoritative answer for that session (explicit user choice);
   *  when absent, callers fall back to `defaultDockOpen` via
   *  `selectDockOpen`. In-memory only — restart loses per-session
   *  flags, which matches the "transient UI" scope. */
  dockOpenBySession: Map<string, boolean>;
  dockHeight: number;
  projects: Map<string, ProjectTerminalState>;
}

type TerminalAction =
  | { type: "toggle_dock"; sessionId: string | null }
  | { type: "set_dock_open"; open: boolean; sessionId: string | null }
  | { type: "set_dock_height"; height: number }
  | { type: "open_tab"; projectKey: string; cwd: string }
  | { type: "close_tab"; projectKey: string; tabId: string }
  | { type: "set_active_tab"; projectKey: string; tabId: string }
  | { type: "set_tab_title"; projectKey: string; tabId: string; title: string }
  | { type: "prune_projects"; keep: Set<string> }
  | { type: "prune_sessions"; keep: Set<string> };

const DOCK_OPEN_KEY = "flowstate:terminal-dock-open";
const DOCK_HEIGHT_KEY = "flowstate:terminal-dock-height";
const DOCK_DEFAULT_HEIGHT = 320;
const DOCK_MIN_HEIGHT = 120;
const DOCK_MAX_HEIGHT = 900;

function readInitial(): TerminalState {
  let defaultDockOpen = false;
  let dockHeight = DOCK_DEFAULT_HEIGHT;
  try {
    defaultDockOpen = window.localStorage.getItem(DOCK_OPEN_KEY) === "1";
    const saved = window.localStorage.getItem(DOCK_HEIGHT_KEY);
    if (saved) {
      const parsed = Number.parseInt(saved, 10);
      if (!Number.isNaN(parsed)) {
        dockHeight = Math.max(
          DOCK_MIN_HEIGHT,
          Math.min(DOCK_MAX_HEIGHT, parsed),
        );
      }
    }
  } catch {
    // localStorage can throw in private mode; stick with defaults.
  }
  return {
    defaultDockOpen,
    dockOpenBySession: new Map(),
    dockHeight,
    projects: new Map(),
  };
}

/**
 * Resolve the effective dock-open flag for a given session. When
 * sessionId is null (home / project page / any route without an
 * active session) callers get the global default. When an entry
 * exists in the per-session map it's the authoritative answer —
 * an explicit user choice overrides the global default, so
 * "closed on thread A" sticks even if the user opens the dock
 * from the home screen later.
 */
export function selectDockOpen(
  state: TerminalState,
  sessionId: string | null,
): boolean {
  if (sessionId === null) return state.defaultDockOpen;
  const sessionValue = state.dockOpenBySession.get(sessionId);
  return sessionValue ?? state.defaultDockOpen;
}

function basename(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  const idx = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return idx >= 0 ? trimmed.slice(idx + 1) : trimmed;
}

function terminalReducer(
  state: TerminalState,
  action: TerminalAction,
): TerminalState {
  switch (action.type) {
    case "toggle_dock": {
      if (action.sessionId === null) {
        return { ...state, defaultDockOpen: !state.defaultDockOpen };
      }
      // Toggle relative to the effective value — if the session
      // has no override, flip the global default for this session
      // (which is what the user sees, so it's what "toggle" must
      // act on).
      const current = selectDockOpen(state, action.sessionId);
      const dockOpenBySession = new Map(state.dockOpenBySession);
      dockOpenBySession.set(action.sessionId, !current);
      return { ...state, dockOpenBySession };
    }
    case "set_dock_open": {
      if (action.sessionId === null) {
        return { ...state, defaultDockOpen: action.open };
      }
      const dockOpenBySession = new Map(state.dockOpenBySession);
      dockOpenBySession.set(action.sessionId, action.open);
      return { ...state, dockOpenBySession };
    }
    case "set_dock_height":
      return {
        ...state,
        dockHeight: Math.max(
          DOCK_MIN_HEIGHT,
          Math.min(DOCK_MAX_HEIGHT, action.height),
        ),
      };
    case "open_tab": {
      // Refuse to create a project-scoped tab without a resolved
      // path. An empty cwd flows into TerminalTab as a prop, and
      // because the tab's stored cwd is frozen at creation, a later
      // resolution would make the prop change and re-spawn the pty.
      // NO_PROJECT_KEY intentionally stores "" — the Rust side
      // treats that as $HOME (see pty.rs open()).
      if (action.projectKey !== NO_PROJECT_KEY && !action.cwd) {
        return state;
      }
      const projects = new Map(state.projects);
      const current = projects.get(action.projectKey) ?? {
        tabs: [],
        activeTabIndex: 0,
      };
      const title = basename(action.cwd) || "shell";
      const newTab: TerminalTab = {
        id: crypto.randomUUID(),
        cwd: action.cwd,
        title,
      };
      projects.set(action.projectKey, {
        tabs: [...current.tabs, newTab],
        activeTabIndex: current.tabs.length,
      });
      return { ...state, projects };
    }
    case "close_tab": {
      const current = state.projects.get(action.projectKey);
      if (!current) return state;
      const idx = current.tabs.findIndex((t) => t.id === action.tabId);
      if (idx === -1) return state;
      const tabs = current.tabs.filter((_, i) => i !== idx);
      const projects = new Map(state.projects);
      if (tabs.length === 0) {
        projects.delete(action.projectKey);
      } else {
        const activeTabIndex = Math.min(
          current.activeTabIndex > idx
            ? current.activeTabIndex - 1
            : current.activeTabIndex,
          tabs.length - 1,
        );
        projects.set(action.projectKey, { tabs, activeTabIndex });
      }
      return { ...state, projects };
    }
    case "set_active_tab": {
      const current = state.projects.get(action.projectKey);
      if (!current) return state;
      const idx = current.tabs.findIndex((t) => t.id === action.tabId);
      if (idx === -1 || idx === current.activeTabIndex) return state;
      const projects = new Map(state.projects);
      projects.set(action.projectKey, { ...current, activeTabIndex: idx });
      return { ...state, projects };
    }
    case "set_tab_title": {
      const current = state.projects.get(action.projectKey);
      if (!current) return state;
      const idx = current.tabs.findIndex((t) => t.id === action.tabId);
      if (idx === -1) return state;
      const tabs = current.tabs.slice();
      tabs[idx] = { ...tabs[idx], title: action.title };
      const projects = new Map(state.projects);
      projects.set(action.projectKey, { ...current, tabs });
      return { ...state, projects };
    }
    case "prune_projects": {
      const projects = new Map<string, ProjectTerminalState>();
      for (const [key, val] of state.projects) {
        if (key === NO_PROJECT_KEY || action.keep.has(key)) {
          projects.set(key, val);
        }
      }
      if (projects.size === state.projects.size) return state;
      return { ...state, projects };
    }
    case "prune_sessions": {
      const dockOpenBySession = new Map<string, boolean>();
      for (const [sid, open] of state.dockOpenBySession) {
        if (action.keep.has(sid)) dockOpenBySession.set(sid, open);
      }
      if (dockOpenBySession.size === state.dockOpenBySession.size) return state;
      return { ...state, dockOpenBySession };
    }
    default:
      return state;
  }
}

interface TerminalContextValue {
  state: TerminalState;
  dispatch: React.Dispatch<TerminalAction>;
}

const TerminalContext = React.createContext<TerminalContextValue | null>(null);

export function TerminalProvider({ children }: { children: React.ReactNode }) {
  const [state, dispatch] = React.useReducer(terminalReducer, undefined, readInitial);

  // Only the global default is persisted. Per-session overrides
  // are intentionally in-memory (they clear on reload), so we
  // don't accumulate one localStorage key per session the user
  // has ever opened.
  React.useEffect(() => {
    try {
      window.localStorage.setItem(
        DOCK_OPEN_KEY,
        state.defaultDockOpen ? "1" : "0",
      );
    } catch {
      // ignore
    }
  }, [state.defaultDockOpen]);

  React.useEffect(() => {
    try {
      window.localStorage.setItem(DOCK_HEIGHT_KEY, String(state.dockHeight));
    } catch {
      // ignore
    }
  }, [state.dockHeight]);

  const value = React.useMemo(() => ({ state, dispatch }), [state]);
  return (
    <TerminalContext.Provider value={value}>{children}</TerminalContext.Provider>
  );
}

export function useTerminal(): TerminalContextValue {
  const ctx = React.useContext(TerminalContext);
  if (!ctx) throw new Error("useTerminal must be used within TerminalProvider");
  return ctx;
}
