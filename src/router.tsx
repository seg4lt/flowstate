import {
  createRootRoute,
  createRoute,
  createRouter,
  Outlet,
} from "@tanstack/react-router";
import { AppSidebar } from "@/components/app-sidebar";
import { SidebarInset, SidebarProvider } from "@/components/ui/sidebar";
import { TooltipProvider } from "@/components/ui/tooltip";

const rootRoute = createRootRoute({
  component: () => (
    <TooltipProvider>
      <SidebarProvider>
        <AppSidebar />
        <SidebarInset>
          <Outlet />
        </SidebarInset>
      </SidebarProvider>
    </TooltipProvider>
  ),
});

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: function IndexPage() {
    return (
      <div className="flex h-full min-h-svh flex-col">
        <header className="flex h-10 items-center border-b border-border px-4 text-sm text-muted-foreground">
          No active thread
        </header>
        <div className="flex flex-1 items-center justify-center p-8 text-sm text-muted-foreground">
          Select a thread or create a new one to get started.
        </div>
      </div>
    );
  },
});

const routeTree = rootRoute.addChildren([indexRoute]);

export const router = createRouter({ routeTree });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
