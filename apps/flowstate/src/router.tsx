import * as React from "react";
import {
  createRootRoute,
  createRoute,
  createRouter,
  Outlet,
  useParams,
} from "@tanstack/react-router";
import { AppSidebar } from "@/components/app-sidebar";
import {
  SidebarInset,
  SidebarProvider,
  useSidebar,
} from "@/components/ui/sidebar";
import { TooltipProvider } from "@/components/ui/tooltip";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { AppProvider, useApp } from "@/stores/app-store";
import { ThemeProvider } from "@/hooks/use-theme";
import { ContextDisplaySettingProvider } from "@/hooks/use-context-display-setting";
import { ProviderEnabledProvider } from "@/hooks/use-provider-enabled";
import { TerminalProvider, useTerminal } from "@/stores/terminal-store";
import { TerminalDock } from "@/components/terminal/TerminalDock";
import { ChatView } from "@/components/chat/chat-view";
import { CodeView } from "@/components/code/code-view";
import { ProjectHomeView } from "@/components/project/project-home-view";
import { SettingsView } from "@/components/settings/settings-view";
import { UsageView } from "@/components/usage/usage-view";
import { Toaster } from "@/components/ui/toaster";
import { UpdateBanner } from "@/components/update-banner";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";

const SIDEBAR_WIDTH_KEY = "flowstate:sidebar-width";
const SIDEBAR_MIN_WIDTH = 200;
const SIDEBAR_MAX_WIDTH = 500;
const SIDEBAR_DEFAULT_WIDTH = 256;

const ZOOM_KEY = "flowstate:webview-zoom";
const ZOOM_MIN = 0.5;
const ZOOM_MAX = 3.0;
const ZOOM_STEP = 0.1;
const ZOOM_DEFAULT = 1.0;

function clampZoom(v: number) {
  return Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, Math.round(v * 100) / 100));
}

function DragHandle({
  width,
  onResize,
}: {
  width: number;
  onResize: (w: number) => void;
}) {
  const { state, isMobile } = useSidebar();
  const handleRef = React.useRef<HTMLDivElement>(null);
  const draggingRef = React.useRef(false);
  const latestWidthRef = React.useRef(width);
  const wrapperRef = React.useRef<HTMLElement | null>(null);

  React.useEffect(() => {
    latestWidthRef.current = width;
  }, [width]);

  React.useEffect(() => {
    function onMove(e: MouseEvent) {
      if (!draggingRef.current || !wrapperRef.current) return;
      const next = Math.max(
        SIDEBAR_MIN_WIDTH,
        Math.min(SIDEBAR_MAX_WIDTH, e.clientX),
      );
      latestWidthRef.current = next;
      wrapperRef.current.style.setProperty("--sidebar-width", `${next}px`);
      if (handleRef.current) {
        handleRef.current.style.left = `${next - 1}px`;
      }
    }
    function onUp() {
      if (!draggingRef.current) return;
      draggingRef.current = false;
      if (wrapperRef.current) {
        wrapperRef.current.removeAttribute("data-sidebar-resizing");
      }
      wrapperRef.current = null;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      onResize(latestWidthRef.current);
    }
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
  }, [onResize]);

  // Hide on mobile: the sidebar renders as a full-width Sheet overlay
  // whose width is hardcoded (`SIDEBAR_WIDTH_MOBILE`) and isn't driven
  // by `--sidebar-width`, so there's nothing to drag. Leaving the
  // handle rendered puts a fixed `z-30` 1px resize bar in the middle
  // of the narrow viewport that catches taps and, if the user drags
  // it, persists a bogus width to localStorage that bites them the
  // next time they return to a wider window.
  if (isMobile || state === "collapsed") return null;

  return (
    <div
      ref={handleRef}
      role="separator"
      aria-label="Resize sidebar"
      className="fixed inset-y-0 z-30 w-1 cursor-col-resize hover:bg-sidebar-border/80"
      style={{ left: width - 1 }}
      onMouseDown={(e) => {
        e.preventDefault();
        const wrapper = (e.currentTarget as HTMLElement).closest<HTMLElement>(
          '[data-slot="sidebar-wrapper"]',
        );
        if (!wrapper) return;
        wrapperRef.current = wrapper;
        wrapper.setAttribute("data-sidebar-resizing", "true");
        draggingRef.current = true;
        document.body.style.cursor = "col-resize";
        document.body.style.userSelect = "none";
      }}
    />
  );
}

// Cmd+J (Ctrl+J on non-mac) toggles the integrated terminal dock.
// Intentionally no isInTextInput guard — the dock is chrome and the
// user expects the shortcut to fire even while typing in the
// composer, matching VS Code behavior.
function useTerminalShortcut() {
  const { dispatch } = useTerminal();
  const { state: appState } = useApp();
  // Ref-shadow activeSessionId so the listener doesn't rebind on
  // every thread switch. The Cmd+J handler reads the ref at press
  // time to route the toggle to either the global default (no
  // session → null) or the per-session override (on a thread).
  const activeSessionIdRef = React.useRef(appState.activeSessionId);
  React.useEffect(() => {
    activeSessionIdRef.current = appState.activeSessionId;
  }, [appState.activeSessionId]);
  React.useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      const mod = e.metaKey || e.ctrlKey;
      if (!mod || e.altKey || e.shiftKey) return;
      if (e.key.toLowerCase() !== "j") return;
      e.preventDefault();
      dispatch({
        type: "toggle_dock",
        sessionId: activeSessionIdRef.current,
      });
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [dispatch]);
}

// Cmd/Ctrl +=, +-, +0 control the webview zoom. Same no-text-input-guard
// rationale as useTerminalShortcut: these are chrome-level shortcuts.
// useIsMobile observes documentElement via ResizeObserver, so it picks
// up the viewport change automatically once the WKWebView reflows after
// setZoom() — no explicit cross-component notification needed here.
function useZoomShortcuts() {
  const zoomRef = React.useRef<number>(ZOOM_DEFAULT);

  React.useEffect(() => {
    const saved = Number.parseFloat(
      window.localStorage.getItem(ZOOM_KEY) ?? "",
    );
    const initial = Number.isFinite(saved) ? clampZoom(saved) : ZOOM_DEFAULT;
    zoomRef.current = initial;
    getCurrentWebviewWindow()
      .setZoom(initial)
      .catch(() => {});
  }, []);

  React.useEffect(() => {
    function apply(next: number) {
      const clamped = clampZoom(next);
      zoomRef.current = clamped;
      window.localStorage.setItem(ZOOM_KEY, String(clamped));
      getCurrentWebviewWindow()
        .setZoom(clamped)
        .catch(() => {});
    }
    function onKeyDown(e: KeyboardEvent) {
      const mod = e.metaKey || e.ctrlKey;
      if (!mod || e.altKey) return;
      if (e.key === "0" && !e.shiftKey) {
        e.preventDefault();
        apply(ZOOM_DEFAULT);
        return;
      }
      if (e.key === "=" || e.key === "+") {
        e.preventDefault();
        apply(zoomRef.current + ZOOM_STEP);
        return;
      }
      if (e.key === "-" || e.key === "_") {
        e.preventDefault();
        apply(zoomRef.current - ZOOM_STEP);
        return;
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);
}

function AppShell() {
  useTerminalShortcut();
  useZoomShortcuts();
  const [width, setWidth] = React.useState<number>(() => {
    const saved = window.localStorage.getItem(SIDEBAR_WIDTH_KEY);
    if (!saved) return SIDEBAR_DEFAULT_WIDTH;
    const parsed = Number.parseInt(saved, 10);
    if (Number.isNaN(parsed)) return SIDEBAR_DEFAULT_WIDTH;
    return Math.max(SIDEBAR_MIN_WIDTH, Math.min(SIDEBAR_MAX_WIDTH, parsed));
  });

  React.useEffect(() => {
    window.localStorage.setItem(SIDEBAR_WIDTH_KEY, String(width));
  }, [width]);

  return (
    <TooltipProvider>
      <SidebarProvider
        style={{ "--sidebar-width": `${width}px` } as React.CSSProperties}
      >
        {import.meta.env.DEV && (
          <div className="pointer-events-none fixed top-1 left-1/2 z-50 -translate-x-1/2 rounded bg-amber-500/90 px-2 py-0.5 text-[10px] font-medium text-black shadow-sm">
            DEV BUILD
          </div>
        )}
        <AppSidebar />
        <DragHandle width={width} onResize={setWidth} />
        {/* min-w-0 + overflow-hidden are essential here. SidebarInset is a
            flex item in the SidebarProvider row, and without min-w-0 its
            default `min-width: auto` lets any wide child (code block,
            tool card with long path, working indicator on a narrow
            window) push the panel past the Tauri window edge, breaking
            the layout for every sibling. overflow-hidden makes any
            residual horizontal overflow clip cleanly inside the panel
            instead of producing a window-level scrollbar. The `relative`
            anchor is what the TerminalDock positions itself against
            (absolute bottom). */}
        <SidebarInset className="relative min-w-0 overflow-hidden">
          <Outlet />
          <TerminalDock />
        </SidebarInset>
      </SidebarProvider>
      <Toaster />
      <UpdateBanner />
    </TooltipProvider>
  );
}

function AppLayout() {
  return (
    <ThemeProvider>
      <ContextDisplaySettingProvider>
        <ProviderEnabledProvider>
          <AppProvider>
            <TerminalProvider>
              <AppShell />
            </TerminalProvider>
          </AppProvider>
        </ProviderEnabledProvider>
      </ContextDisplaySettingProvider>
    </ThemeProvider>
  );
}

const rootRoute = createRootRoute({
  component: AppLayout,
});

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: function IndexPage() {
    return (
      <div className="flex h-svh flex-col">
        <header className="flex h-12 items-center gap-2 border-b border-border px-2 text-sm text-muted-foreground">
          <SidebarTrigger />
          <span>No active thread</span>
        </header>
        <div className="flex flex-1 items-center justify-center p-8 text-sm text-muted-foreground">
          Select a thread or create a new one to get started.
        </div>
      </div>
    );
  },
});

const chatRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/chat/$sessionId",
  component: function ChatPage() {
    const { sessionId } = useParams({ from: "/chat/$sessionId" });
    return <ChatView sessionId={sessionId} />;
  },
});

const codeRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/code/$sessionId",
  component: function CodePage() {
    const { sessionId } = useParams({ from: "/code/$sessionId" });
    return <CodeView sessionId={sessionId} />;
  },
});

const browseRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/browse",
  validateSearch: (search: Record<string, unknown>): { path: string } => ({
    path: String(search.path ?? ""),
  }),
  component: function BrowsePage() {
    const { path } = browseRoute.useSearch();
    if (!path) {
      return (
        <div className="flex h-svh items-center justify-center text-sm text-muted-foreground">
          No path specified.
        </div>
      );
    }
    return <CodeView projectPath={path} />;
  },
});

const projectRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/project/$projectId",
  component: function ProjectPage() {
    const { projectId } = useParams({ from: "/project/$projectId" });
    return <ProjectHomeView projectId={projectId} />;
  },
});

const settingsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/settings",
  component: SettingsView,
});

const usageRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/usage",
  component: UsageView,
});

const routeTree = rootRoute.addChildren([
  indexRoute,
  chatRoute,
  codeRoute,
  browseRoute,
  projectRoute,
  settingsRoute,
  usageRoute,
]);

export const router = createRouter({ routeTree });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
