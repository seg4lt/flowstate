import { useState } from "react";
import { ChevronRight, Folder, FolderOpen, Pencil, Plus, Trash2 } from "lucide-react";
import {
  SidebarGroup,
  SidebarGroupLabel,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
} from "../ui/sidebar";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuTrigger,
} from "../ui/context-menu";
import { Input } from "../ui/input";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "../ui/dropdown-menu";
import type { ProjectGroup, SendClientMessage } from "../../state/appStore";
import { actions, useAppStore } from "../../state/appStore";
import { ThreadMenuItem } from "./ThreadMenuItem";
import type { ProviderKind, ProviderStatus } from "../../types";
import { PROVIDER_COLORS, PROVIDER_LABELS } from "../../types";

interface Props {
  group: ProjectGroup;
  providers: ProviderStatus[];
  sendClientMessage: SendClientMessage;
}

export function ProjectGroupNode({ group, providers, sendClientMessage }: Props) {
  const { project } = group;
  const key = project?.projectId ?? "unassigned";
  const expanded = useAppStore(
    (s) => s.expandedProjectIds[key] ?? true,
  );
  const [renaming, setRenaming] = useState(false);
  const [draftName, setDraftName] = useState(project?.name ?? "");
  const label = project?.name ?? "Unassigned";

  const submitRename = () => {
    const trimmed = draftName.trim();
    if (project && trimmed && trimmed !== project.name) {
      sendClientMessage({
        type: "rename_project",
        project_id: project.projectId,
        name: trimmed,
      });
    }
    setRenaming(false);
  };

  const handleDelete = () => {
    if (!project) return;
    sendClientMessage({ type: "delete_project", project_id: project.projectId });
  };

  const startSessionInProject = (providerKind: ProviderKind, model?: string) => {
    sendClientMessage({
      type: "start_session",
      provider: providerKind,
      title: null,
      model: model ?? null,
      project_id: project?.projectId ?? null,
    });
    actions.setLastAction(`Creating ${PROVIDER_LABELS[providerKind]} session...`);
  };

  return (
    <SidebarGroup className="py-1">
      <div className="flex items-center justify-between gap-1 px-2">
        <button
          type="button"
          onClick={() => actions.toggleProjectExpanded(key)}
          className="flex items-center gap-1.5 flex-1 min-w-0 text-left"
        >
          <ChevronRight
            className={`h-3 w-3 shrink-0 text-muted-foreground transition-transform ${
              expanded ? "rotate-90" : ""
            }`}
          />
          {expanded ? (
            <FolderOpen className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
          ) : (
            <Folder className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
          )}
          {renaming ? (
            <Input
              autoFocus
              value={draftName}
              onChange={(e) => setDraftName(e.target.value)}
              onClick={(e) => e.stopPropagation()}
              onKeyDown={(e) => {
                e.stopPropagation();
                if (e.key === "Enter") {
                  e.preventDefault();
                  submitRename();
                } else if (e.key === "Escape") {
                  setRenaming(false);
                  setDraftName(project?.name ?? "");
                }
              }}
              onBlur={submitRename}
              className="h-6 text-xs px-1"
            />
          ) : (
            <SidebarGroupLabel className="flex-1 min-w-0 truncate px-0 cursor-pointer">
              {label}
            </SidebarGroupLabel>
          )}
        </button>

        <DropdownMenu>
          <DropdownMenuTrigger>
            <button
              type="button"
              className="h-5 w-5 flex items-center justify-center rounded hover:bg-muted"
              aria-label="New thread in project"
            >
              <Plus className="h-3.5 w-3.5 text-muted-foreground" />
            </button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="start" className="w-56">
            {providers.map((provider) => (
              <DropdownMenuSub key={provider.kind}>
                <DropdownMenuSubTrigger className="gap-2">
                  <div className={`w-2 h-2 rounded-full ${PROVIDER_COLORS[provider.kind]}`} />
                  New {provider.label} thread
                </DropdownMenuSubTrigger>
                <DropdownMenuSubContent>
                  {provider.models.length === 0 ? (
                    <DropdownMenuItem
                      onClick={() => startSessionInProject(provider.kind)}
                    >
                      Default
                    </DropdownMenuItem>
                  ) : (
                    provider.models.map((model) => (
                      <DropdownMenuItem
                        key={model.value}
                        onClick={() => startSessionInProject(provider.kind, model.value)}
                      >
                        {model.label}
                      </DropdownMenuItem>
                    ))
                  )}
                </DropdownMenuSubContent>
              </DropdownMenuSub>
            ))}
          </DropdownMenuContent>
        </DropdownMenu>

        {project && (
          <ContextMenu>
            <ContextMenuTrigger>
              <div className="h-0 w-0" />
            </ContextMenuTrigger>
            <ContextMenuContent>
              <ContextMenuItem
                onClick={() => {
                  setRenaming(true);
                  setDraftName(project.name);
                }}
              >
                <Pencil className="mr-2 h-3.5 w-3.5" /> Rename
              </ContextMenuItem>
              <ContextMenuItem onClick={handleDelete}>
                <Trash2 className="mr-2 h-3.5 w-3.5" /> Delete
              </ContextMenuItem>
            </ContextMenuContent>
          </ContextMenu>
        )}
      </div>

      {expanded && (
        <SidebarMenu className="pl-4">
          {group.sessions.length === 0 ? (
            <SidebarMenuItem>
              <SidebarMenuButton disabled className="text-xs text-muted-foreground italic">
                No threads
              </SidebarMenuButton>
            </SidebarMenuItem>
          ) : (
            group.sessions.map((session) => (
              <ThreadMenuItem
                key={session.summary.sessionId}
                session={session}
                sendClientMessage={sendClientMessage}
              />
            ))
          )}
        </SidebarMenu>
      )}
    </SidebarGroup>
  );
}
