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
import { AppProvider } from "@/stores/app-store";
import { ChatView } from "@/components/chat/chat-view";
import { NewThreadPage } from "@/components/new-thread-page";

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
          <AppSidebar />
          <DragHandle width={width} onResize={setWidth} />
          <SidebarInset>
            <Outlet />
          </SidebarInset>
        </SidebarProvider>
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
    return <NewThreadPage />;
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

const routeTree = rootRoute.addChildren([indexRoute, chatRoute]);

export const router = createRouter({ routeTree });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
