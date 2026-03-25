import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  Archive,
  EllipsisVertical,
  GitBranch,
  Loader2,
  Trash2,
} from "lucide-react";
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
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { useApp } from "@/stores/app-store";
import { prefetchSession } from "@/lib/queries";
import { cn } from "@/lib/utils";

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
  /** Set on threads tied to a git worktree project. Drives the small
   *  branch-icon indicator before the title + its hover tooltip. Null
   *  for main-project threads. */
  worktreeBranch?: string | null;
  worktreePath?: string | null;
  /** True while the session has an in-flight turn. Renders a small
   *  spinner next to the title so the user can see at a glance which
   *  threads are working without having to open them. */
  running: boolean;
  /** True when the agent has paused mid-turn and is actively waiting
   *  on the user — a permission prompt, an AskUserQuestion, or an
   *  ExitPlanMode plan approval. Distinct from `running`: a thread
   *  can be running without awaiting input (model is generating)
   *  and can briefly be awaiting input without being running (in
   *  the gap between a turn ending and a new one starting, though
   *  that's rare). Drives the "Need Response" badge. */
  awaitingInput: boolean;
  /** True when the most recent turn finished while the user was on
   *  a different screen / thread. The store clears this the moment
   *  the user activates the thread, so the badge naturally
   *  disappears on click. Always false on the active thread. */
  pendingDone: boolean;
  onClick: () => void;
}

export function ThreadItem({
  sessionId,
  title,
  updatedAt,
  isActive,
  worktreeBranch = null,
  worktreePath = null,
  running,
  awaitingInput,
  pendingDone,
  onClick,
}: ThreadItemProps) {
  const { send, renameSession } = useApp();
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
      void renameSession(sessionId, trimmed);
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
          className={cn(
            "h-7 w-full min-w-0 rounded-r-none",
            // pr-12 leaves room for the absolute hover-buttons (~46px)
            // and the short "12m" timestamp. The "Need Response" chip
            // is much wider (~90px), so when it's visible we bump the
            // padding to pr-24 to keep it from overlapping the title.
            // Active threads never show "Need Response" so they stay
            // on the compact pr-12.
            awaitingInput && !isActive ? "pr-24" : "pr-12",
          )}
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
          {worktreePath && (
            <Tooltip>
              <TooltipTrigger asChild>
                <span
                  className="inline-flex shrink-0 items-center text-muted-foreground"
                  onClick={(e) => e.stopPropagation()}
                >
                  <GitBranch className="h-3 w-3" />
                </span>
              </TooltipTrigger>
              <TooltipContent side="right" className="max-w-xs">
                <div className="space-y-0.5">
                  <div className="font-medium">
                    Worktree
                    {worktreeBranch ? `: ${worktreeBranch}` : ""}
                  </div>
                  <div className="font-mono text-[10px] opacity-80">
                    {worktreePath}
                  </div>
                </div>
              </TooltipContent>
            </Tooltip>
          )}
          <span className="flex-1 truncate text-xs">
            {title || "New thread"}
          </span>
        </SidebarMenuSubButton>
      )}

      {/* Right-side status chip — fades out on hover so the action
       *  buttons can take its place. Priority:
       *    1. agent is actively waiting on the user         → "Need Response" (blue)
       *    2. recently finished while user was elsewhere    → "Done"          (green)
       *    3. otherwise                                     → relative time
       *  Active threads always show the time — the user can already
       *  see what's happening in the message timeline so the badge
       *  would be redundant noise. Note that "Need Response" is NOT
       *  the same as "running" — it only fires when the agent has
       *  actually paused on a permission prompt / AskUserQuestion /
       *  ExitPlanMode, not while the model is generating. */}
      {!editing && (
        <span
          className={
            awaitingInput && !isActive
              ? "pointer-events-none absolute right-2 top-1/2 -translate-y-1/2 text-[10px] font-medium text-blue-600 transition-opacity group-hover/thread:opacity-0 dark:text-blue-400"
              : pendingDone && !isActive
                ? "pointer-events-none absolute right-2 top-1/2 -translate-y-1/2 text-[10px] font-medium text-green-600 transition-opacity group-hover/thread:opacity-0 dark:text-green-400"
                : "pointer-events-none absolute right-2 top-1/2 -translate-y-1/2 text-[10px] text-muted-foreground transition-opacity group-hover/thread:opacity-0"
          }
        >
          {awaitingInput && !isActive
            ? "Need Response"
            : pendingDone && !isActive
              ? "Done"
              : formatTimeAgo(updatedAt)}
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
