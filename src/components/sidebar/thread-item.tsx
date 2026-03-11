import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Archive, EllipsisVertical, Loader2, Trash2 } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  SidebarMenuSubButton,
  SidebarMenuSubItem,
} from "@/components/ui/sidebar";
import { useApp } from "@/stores/app-store";
import { prefetchSession } from "@/lib/queries";

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
  /** True while the session has an in-flight turn. Renders a small
   *  spinner next to the title so the user can see at a glance which
   *  threads are working without having to open them. */
  running: boolean;
  onClick: () => void;
}

export function ThreadItem({
  sessionId,
  title,
  updatedAt,
  isActive,
  running,
  onClick,
}: ThreadItemProps) {
  const { send } = useApp();
  const queryClient = useQueryClient();
  const [editing, setEditing] = React.useState(false);
  const [draft, setDraft] = React.useState(title);
  const inputRef = React.useRef<HTMLInputElement>(null);
  // Hover prefetch: warm the session cache the moment the pointer
  // enters this row so the click itself only has to consume cached
  // data. A few hundred ms of hover — normal human targeting time —
  // is usually enough to cover the full `load_session` round-trip.
  // tanstack query dedupes repeated prefetches and skips entirely
  // when the cache is already warm, so this is a no-op on
  // re-entry or on the active thread.
  const handleMouseEnter = React.useCallback(() => {
    prefetchSession(queryClient, sessionId);
  }, [queryClient, sessionId]);

  React.useEffect(() => {
    setDraft(title);
  }, [title]);

  React.useEffect(() => {
    if (editing) {
      inputRef.current?.focus();
      inputRef.current?.select();
    }
  }, [editing]);

  function commitRename() {
    const trimmed = draft.trim();
    setEditing(false);
    if (trimmed && trimmed !== title) {
      send({ type: "rename_session", session_id: sessionId, title: trimmed });
    }
  }

  return (
    <SidebarMenuSubItem
      className="group/thread -mr-6"
      onMouseEnter={handleMouseEnter}
    >
      {editing ? (
        <input
          ref={inputRef}
          className="h-7 w-full min-w-0 rounded-md border border-input bg-background px-2 text-xs outline-none"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commitRename}
          onKeyDown={(e) => {
            if (e.key === "Enter") commitRename();
            if (e.key === "Escape") {
              setDraft(title);
              setEditing(false);
            }
          }}
        />
      ) : (
        <SidebarMenuSubButton
          className="h-7 w-full min-w-0 rounded-r-none pr-12"
          isActive={isActive}
          onClick={onClick}
          onDoubleClick={(e) => {
            e.stopPropagation();
            setEditing(true);
          }}
        >
          {running && (
            <Loader2 className="h-3 w-3 shrink-0 animate-spin text-muted-foreground" />
          )}
          <span className="flex-1 truncate text-xs">
            {title || "New thread"}
          </span>
        </SidebarMenuSubButton>
      )}

      {/* Timestamp — fades out on hover */}
      {!editing && (
        <span className="pointer-events-none absolute right-2 top-1/2 -translate-y-1/2 text-[10px] text-muted-foreground transition-opacity group-hover/thread:opacity-0">
          {formatTimeAgo(updatedAt)}
        </span>
      )}

      {/* Actions — fades in on hover, stays visible when dropdown is open */}
      {!editing && (
        <div
          className="absolute right-1 top-1/2 flex -translate-y-1/2 items-center gap-0.5 opacity-0 transition-opacity group-hover/thread:opacity-100 has-[[data-state=open]]:opacity-100"
          onClick={(e) => e.stopPropagation()}
          onKeyDown={(e) => e.stopPropagation()}
        >
          <button
            type="button"
            title="Archive"
            className="inline-flex h-5 w-5 items-center justify-center rounded-md text-sidebar-foreground outline-none hover:bg-sidebar-accent"
            onClick={() =>
              send({ type: "archive_session", session_id: sessionId })
            }
          >
            <Archive className="h-3 w-3" />
          </button>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <button
                type="button"
                className="inline-flex h-5 w-5 items-center justify-center rounded-md text-sidebar-foreground outline-none hover:bg-sidebar-accent"
              >
                <EllipsisVertical className="h-3 w-3" />
              </button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="start" className="min-w-32">
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
      )}
    </SidebarMenuSubItem>
  );
}
