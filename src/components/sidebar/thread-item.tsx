import { Archive, Ellipsis, Trash2 } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  SidebarMenuSubButton,
  SidebarMenuSubItem,
} from "@/components/ui/sidebar";
import { useApp } from "@/stores/app-store";

function formatTimeAgo(iso: string): string {
  const diff = Date.now() - new Date(iso).getTime();
  const minutes = Math.floor(diff / 60000);
  if (minutes < 1) return "now";
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.floor(hours / 24);
  return `${days}d`;
}

interface ThreadItemProps {
  sessionId: string;
  title: string;
  updatedAt: string;
  isActive: boolean;
  onClick: () => void;
}

export function ThreadItem({
  sessionId,
  title,
  updatedAt,
  isActive,
  onClick,
}: ThreadItemProps) {
  const { send } = useApp();

  return (
    <SidebarMenuSubItem className="group/thread -mr-6">
      <SidebarMenuSubButton
        className="h-7 w-full min-w-0 rounded-r-none pr-8"
        isActive={isActive}
        onClick={onClick}
      >
        <span className="flex-1 truncate text-xs">{title || "New thread"}</span>
      </SidebarMenuSubButton>

      {/* Timestamp — fades out on hover */}
      <span className="pointer-events-none absolute right-2 top-1/2 -translate-y-1/2 text-[10px] text-muted-foreground transition-opacity group-hover/thread:opacity-0">
        {formatTimeAgo(updatedAt)}
      </span>

      {/* Actions — fades in on hover, stays visible when dropdown is open */}
      <div
        className="absolute right-1 top-1/2 -translate-y-1/2 opacity-0 transition-opacity group-hover/thread:opacity-100 has-[[data-state=open]]:opacity-100"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={(e) => e.stopPropagation()}
      >
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <button
              type="button"
              className="inline-flex h-5 w-5 items-center justify-center rounded-md bg-sidebar-accent text-sidebar-foreground outline-none hover:bg-sidebar-border"
            >
              <Ellipsis className="h-3 w-3" />
            </button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="start" className="min-w-32">
            <DropdownMenuItem
              onClick={() =>
                send({ type: "archive_session", session_id: sessionId })
              }
            >
              <Archive className="mr-2 h-3.5 w-3.5" />
              Archive
            </DropdownMenuItem>
            <DropdownMenuSeparator />
            <DropdownMenuItem
              variant="destructive"
              onClick={() =>
                send({ type: "delete_session", session_id: sessionId })
              }
            >
              <Trash2 className="mr-2 h-3.5 w-3.5" />
              Delete
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
    </SidebarMenuSubItem>
  );
}
