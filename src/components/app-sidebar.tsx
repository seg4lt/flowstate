import {
  ChevronRight,
  FolderIcon,
  Plus,
  Settings,
  ArrowUpDown,
} from "lucide-react";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupAction,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarMenuSub,
  SidebarMenuSubButton,
  SidebarMenuSubItem,
  SidebarRail,
} from "@/components/ui/sidebar";

type Thread = {
  id: string;
  title: string;
  updatedAt: string;
};

type Project = {
  id: string;
  name: string;
  threads: Thread[];
};

const projects: Project[] = [
  {
    id: "acme-web",
    name: "acme-web",
    threads: [
      { id: "t1", title: "Wire up checkout flow", updatedAt: "2m ago" },
      { id: "t2", title: "Refactor auth middleware", updatedAt: "1h ago" },
    ],
  },
  {
    id: "internal-api",
    name: "internal-api",
    threads: [
      { id: "t3", title: "Add pagination to list endpoints", updatedAt: "14m ago" },
    ],
  },
  {
    id: "design-system",
    name: "design-system",
    threads: [
      { id: "t4", title: "Draft v2 token spec", updatedAt: "yesterday" },
    ],
  },
];

export function AppSidebar() {
  return (
    <Sidebar collapsible="icon">
      <SidebarHeader className="h-12 flex-row items-center justify-start border-b border-sidebar-border px-4 py-0">
        <span className="text-sm font-semibold tracking-tight [[data-collapsible=icon]_&]:hidden">
          flowzen
        </span>
      </SidebarHeader>

      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupLabel>Projects</SidebarGroupLabel>
          <SidebarGroupAction title="Sort projects">
            <ArrowUpDown />
            <span className="sr-only">Sort projects</span>
          </SidebarGroupAction>
          <SidebarGroupAction title="New project" className="right-7">
            <Plus />
            <span className="sr-only">New project</span>
          </SidebarGroupAction>
          <SidebarGroupContent>
            <SidebarMenu>
              {projects.map((project) => (
                <Collapsible
                  key={project.id}
                  defaultOpen
                  className="group/collapsible"
                >
                  <SidebarMenuItem>
                    <CollapsibleTrigger asChild>
                      <SidebarMenuButton tooltip={project.name}>
                        <ChevronRight className="transition-transform duration-200 group-data-[state=open]/collapsible:rotate-90" />
                        <FolderIcon />
                        <span>{project.name}</span>
                      </SidebarMenuButton>
                    </CollapsibleTrigger>
                    <CollapsibleContent>
                      <SidebarMenuSub>
                        {project.threads.map((thread) => (
                          <SidebarMenuSubItem key={thread.id}>
                            <SidebarMenuSubButton className="h-auto flex-col items-start gap-0.5 py-1.5">
                              <span className="truncate text-sm">
                                {thread.title}
                              </span>
                              <span className="text-xs text-muted-foreground">
                                {thread.updatedAt}
                              </span>
                            </SidebarMenuSubButton>
                          </SidebarMenuSubItem>
                        ))}
                      </SidebarMenuSub>
                    </CollapsibleContent>
                  </SidebarMenuItem>
                </Collapsible>
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>

      <SidebarFooter className="border-t border-sidebar-border">
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton tooltip="Settings">
              <Settings />
              <span>Settings</span>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarFooter>
      <SidebarRail />
    </Sidebar>
  );
}
