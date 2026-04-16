import * as React from "react";

// Integrated-terminal state. Scope: the dock is global
// (Cmd+J toggles one dock across the whole app, height persists
// to localStorage), but the tab set is keyed by project id so
// switching to a different project swaps which tabs the dock
// shows. A folder-less session (no project) gets its own pool
// under NO_PROJECT_KEY.
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
  dockOpen: boolean;
  dockHeight: number;
  projects: Map<string, ProjectTerminalState>;
}

type TerminalAction =
  | { type: "toggle_dock" }
  | { type: "set_dock_open"; open: boolean }
  | { type: "set_dock_height"; height: number }
  | { type: "open_tab"; projectKey: string; cwd: string }
  | { type: "close_tab"; projectKey: string; tabId: string }
  | { type: "set_active_tab"; projectKey: string; tabId: string }
  | { type: "set_tab_title"; projectKey: string; tabId: string; title: string }
  | { type: "prune_projects"; keep: Set<string> };

const DOCK_HEIGHT_KEY = "flowstate:terminal-dock-height";
const DOCK_DEFAULT_HEIGHT = 320;
const DOCK_MIN_HEIGHT = 120;
const DOCK_MAX_HEIGHT = 900;

function readInitial(): TerminalState {
  // Always start with the dock closed. We intentionally do not
  // persist dockOpen: tab metadata isn't persisted either, so
  // restoring "open" on launch would just trigger the auto-spawn
  // effect and resurrect a throwaway shell the user never asked for.
  const dockOpen = false;
  let dockHeight = DOCK_DEFAULT_HEIGHT;
  try {
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
  return { dockOpen, dockHeight, projects: new Map() };
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
    case "toggle_dock":
      return { ...state, dockOpen: !state.dockOpen };
    case "set_dock_open":
      return { ...state, dockOpen: action.open };
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
      let dockOpen = state.dockOpen;
      if (tabs.length === 0) {
        projects.delete(action.projectKey);
        // If no project has any tabs left, fold the dock away so the
        // auto-spawn effect in TerminalDock doesn't resurrect a shell.
        if (projects.size === 0) {
          dockOpen = false;
        }
      } else {
        const activeTabIndex = Math.min(
          current.activeTabIndex > idx
            ? current.activeTabIndex - 1
            : current.activeTabIndex,
          tabs.length - 1,
        );
        projects.set(action.projectKey, { tabs, activeTabIndex });
      }
      return { ...state, projects, dockOpen };
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
