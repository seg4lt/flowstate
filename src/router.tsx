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
import { AppProvider } from "@/stores/app-store";
import { ChatView } from "@/components/chat/chat-view";
import { CodeView } from "@/components/code/code-view";
import { SettingsView } from "@/components/settings/settings-view";
import { Toaster } from "@/components/ui/toaster";

const SIDEBAR_WIDTH_KEY = "flowzen:sidebar-width";
const SIDEBAR_MIN_WIDTH = 200;
const SIDEBAR_MAX_WIDTH = 500;
const SIDEBAR_DEFAULT_WIDTH = 256;

function DragHandle({
  width,
  onResize,
}: {
  width: number;
  onResize: (w: number) => void;
}) {
  const { state } = useSidebar();
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

  if (state === "collapsed") return null;

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

function AppLayout() {
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
    <AppProvider>
      <TooltipProvider>
        <SidebarProvider
          style={
            { "--sidebar-width": `${width}px` } as React.CSSProperties
          }
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
              instead of producing a window-level scrollbar. */}
          <SidebarInset className="min-w-0 overflow-hidden">
            <Outlet />
          </SidebarInset>
        </SidebarProvider>
        <Toaster />
      </TooltipProvider>
    </AppProvider>
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

const settingsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/settings",
  component: SettingsView,
});

const routeTree = rootRoute.addChildren([
  indexRoute,
  chatRoute,
  codeRoute,
  settingsRoute,
]);

export const router = createRouter({ routeTree });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
