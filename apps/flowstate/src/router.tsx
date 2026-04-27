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
import { RoutePromptOverlay } from "@/components/chat/route-prompt-overlay";
import { CodeView } from "@/components/code/code-view";
import { ProjectHomeView } from "@/components/project/project-home-view";
import { SettingsView } from "@/components/settings/settings-view";
import { UsageView } from "@/components/usage/usage-view";
import { Toaster } from "@/components/ui/toaster";
import { UpdateBanner } from "@/components/update-banner";
import { ProvisioningSplash } from "@/components/provisioning-splash";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { isPopoutWindow } from "@/lib/popout";
import { useGlobalShortcuts } from "@/hooks/useGlobalShortcuts";
import { ShortcutsDialog } from "@/lib/keyboard-shortcuts";
import { ProjectProviderPicker } from "@/components/project/project-provider-picker";

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

// Cmd+W: app-wide no-op safety net.
//
// CodeView has its own ⌘W handler (close active editor tab) that
// runs at the same `window` keydown phase and calls `preventDefault`
// before returning. Here we install a global handler that *also*
// preventDefaults ⌘W so on every other route — chat, project home,
// settings, etc. — the press is swallowed instead of falling through
// to the macOS Window menu's "Close Window" item (which fires
// `WindowEvent::CloseRequested` and hides the main window). Both
// listeners are attached to `window` and React effects mount in
// child-before-parent order, so when CodeView is mounted its
// listener runs first and the global no-op below never sees the
// event (return-without-preventDefault here would still be safe,
// but CodeView already prevents). When CodeView isn't mounted,
// only this global runs and the keystroke quietly dies.
//
// Cmd+Shift+W is left alone — it's not bound by Tauri's default
// macOS menu and a few existing shortcuts (e.g. registry entries)
// could grow into that namespace later. Cmd+Ctrl+W and Cmd+Alt+W
// are also left alone for the same reason.
function useCmdWNoop() {
  React.useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      const mod = e.metaKey || e.ctrlKey;
      if (!mod || e.altKey || e.shiftKey) return;
      if (e.key.toLowerCase() !== "w") return;
      e.preventDefault();
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);
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
    // The WKWebView default is 1.0. Calling setZoom(1.0) still triggers a
    // full document reflow — and because Tauri's IPC queue is blocked behind
    // provision_runtimes / connectStream backoff on cold launch, that reflow
    // can land 5–15 s after first paint, disrupting whatever the user is
    // doing (cursor jumps, Virtuoso scroll loss, sidebar/title flicker).
    // Skip the IPC entirely when there's nothing to restore.
    if (initial === ZOOM_DEFAULT) return;
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

// Stripped shell used by thread popouts (the Rust `popout_thread`
// command opens a webview at `/chat/<id>?popout=1`). No sidebar,
// no terminal dock, no update banner, no provisioning splash —
// those belong only to the main window. The popout still needs
// TooltipProvider + Toaster + zoom shortcuts, and sits inside the
// same provider stack (see AppLayout) so its AppProvider opens
// its own `connectStream` subscription and hydrates from the
// broadcast.
//
// `SidebarProvider` is mounted as a safety net even though no
// `AppSidebar` is rendered here. Reason: several routes the user
// can navigate into while inside the popout (CodeView, SettingsView,
// ProjectHomeView, UsageView, the index route) render
// `<SidebarTrigger />` unconditionally in their header. A client-side
// navigation inside the popout (e.g. the "Search" button in the chat
// header -> /code/<id>) drops the `?popout=1` query string, but the
// shell choice in `AppLayout` is memoized at mount, so we stay in
// `PopoutShell` — and those unguarded SidebarTriggers would throw
// `useSidebar must be used within a SidebarProvider`. With the
// provider in scope the triggers degrade to harmless no-ops (there's
// no AppSidebar to show/hide) instead of crashing the whole popout
// into the TanStack Router error boundary.
function PopoutShell() {
  useZoomShortcuts();
  // Run the global shortcuts hook in "popout" mode so the only entry
  // that fires here is the always-on-top toggle (⌘⇧T). Everything
  // else in SHORTCUTS depends on UI mounted in the main AppShell
  // (project picker, help dialog) or has semantics that don't transfer
  // to a single-thread popout (⌘N spawning a thread the user can't
  // see, ⌘[/⌘] cycling threads inside a window pinned to one). The
  // help / picker callbacks are no-ops here for the same reason.
  useGlobalShortcuts({
    openShortcutsHelp: noopOpen,
    openProjectPicker: noopOpen,
    mode: "popout",
  });
  // Same `state.ready` gate as AppShell — keep the popout's Outlet
  // unmounted until `welcome` lands so ChatView doesn't mount with
  // an empty `state.sessions` (popouts open at /chat/<id>?popout=1
  // and would otherwise render the same boot-race "no session"
  // shell that AppShell does). Popouts don't host ProvisioningSplash,
  // so render a small placeholder while we wait — boot is bounded
  // by `connectStream`'s retry budget either way.
  const { state } = useApp();
  return (
    <TooltipProvider>
      <SidebarProvider>
        <div className="h-svh w-svw">
          {state.ready ? (
            <Outlet />
          ) : (
            <div
              role="status"
              aria-live="polite"
              className="flex h-full items-center justify-center text-xs text-muted-foreground"
            >
              Loading…
            </div>
          )}
        </div>
      </SidebarProvider>
      <Toaster />
    </TooltipProvider>
  );
}

// Module-level no-op so PopoutShell's useGlobalShortcuts call gets a
// stable callback identity each render — useGlobalShortcuts depends
// on the callback for its effect deps and a fresh function per
// render would needlessly re-bind the keydown listener.
function noopOpen(): void {
  /* PopoutShell never opens these dialogs — they aren't mounted here. */
}

function AppShell() {
  useTerminalShortcut();
  useZoomShortcuts();
  // Swallow ⌘W everywhere except CodeView (which has its own
  // tab-close handler). Without this the press falls through to the
  // macOS Window menu's "Close Window" item and hides the main
  // window — see useCmdWNoop's comment. Intentionally not mounted in
  // PopoutShell: popouts are window-shaped, and ⌘W closing the
  // popout matches platform expectations.
  useCmdWNoop();
  // Gate the route `<Outlet />` on `state.ready` so route components
  // (ChatView, CodeView, ProjectHomeView, …) don't mount with an
  // empty `state.sessions` / `state.projects` before the daemon's
  // `welcome` message lands. Without this gate, ChatView mounted with
  // a sessionId from the persisted URL but `state.sessions.get(id)`
  // returned undefined, so the chat shell rendered as a "no session"
  // view; then `welcome` arrived, state populated, and the entire
  // route subtree re-rendered through the populated path. The user-
  // visible result was a refresh blip ("all projects/sessions gone,
  // current thread goes to new thread, fixes itself"), Virtuoso
  // losing its scroll position (the rAF in MessageList's scroll-to-
  // latest effect got canceled mid-storm), and the composer cursor
  // jumping as `disabled` flipped during the re-render cascade. The
  // `<ProvisioningSplash />` overlay below still covers the screen
  // while we wait, so this just defers route mount to the moment we
  // actually have data to render.
  const { state } = useApp();
  const [shortcutsHelpOpen, setShortcutsHelpOpen] = React.useState(false);
  const [projectPickerOpen, setProjectPickerOpen] = React.useState(false);
  useGlobalShortcuts({
    openShortcutsHelp: React.useCallback(
      () => setShortcutsHelpOpen(true),
      [],
    ),
    openProjectPicker: React.useCallback(
      () => setProjectPickerOpen(true),
      [],
    ),
  });
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
          {/* `state.ready` flips true the moment the daemon's `welcome`
              message lands and `state.sessions` / `state.projects` are
              populated. Holding the route mount until then is what
              eliminates the boot-time "empty → populated" re-render
              storm; ProvisioningSplash covers the gap visually. The
              `RoutePromptOverlay` and `TerminalDock` consume per-
              session state too, so they sit inside the same gate. */}
          {state.ready ? (
            <>
              <Outlet />
              {/* Route-independent surface for the per-session
                  permission and clarifying-question prompts. Yields
                  to ChatView's inline rendering on /chat/$sessionId;
                  on every other route that carries a sessionId param
                  (notably /code/$sessionId) it surfaces the same
                  prompt at the bottom of the viewport so the daemon's
                  pause for input isn't invisible while the user is in
                  the code view. */}
              <RoutePromptOverlay />
              <TerminalDock />
            </>
          ) : null}
        </SidebarInset>
      </SidebarProvider>
      <Toaster />
      <UpdateBanner />
      {/*
        First-launch loading overlay. Rendered last so it sits above
        every sibling via its own z-[9999] — the app UI stays mounted
        behind it so any late-arriving state (welcome message,
        sessions) is already hydrated when the splash unmounts.
      */}
      <ProvisioningSplash />
      <ShortcutsDialog
        open={shortcutsHelpOpen}
        onOpenChange={setShortcutsHelpOpen}
      />
      <ProjectProviderPicker
        open={projectPickerOpen}
        onOpenChange={setProjectPickerOpen}
      />
    </TooltipProvider>
  );
}

function AppLayout() {
  // Decide once per window, not per render: the popout flag is
  // set by the Rust-side `popout_thread` command on URL creation
  // and never flips after mount. Computing it at module scope
  // would run before the test environment can stub `window`, so
  // lazy-initialize inside the component instead.
  const popout = React.useMemo(() => isPopoutWindow(), []);
  return (
    <ThemeProvider>
      <ContextDisplaySettingProvider>
        <ProviderEnabledProvider>
          <AppProvider>
            <TerminalProvider>
              {popout ? <PopoutShell /> : <AppShell />}
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
  // `mode` is the initial picker tab: "files" for file-name search,
  // "content" for ripgrep content search. Set by the global ⌘P / ⌘⇧F
  // shortcuts in keyboard-shortcuts.tsx so the user lands in the
  // right mode without an extra click. Optional and validated through
  // a string allowlist so unknown values silently fall back to the
  // route's default (files mode).
  validateSearch: (
    search: Record<string, unknown>,
  ): { mode?: "files" | "content" } => ({
    mode:
      search.mode === "content"
        ? "content"
        : search.mode === "files"
          ? "files"
          : undefined,
  }),
  component: function CodePage() {
    const { sessionId } = useParams({ from: "/code/$sessionId" });
    const { mode } = codeRoute.useSearch();
    return <CodeView sessionId={sessionId} initialSearchMode={mode} />;
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
