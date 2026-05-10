// Orchestrator kanban board.
//
// Reads/writes the `/api/orchestrator/*` HTTP surface served by the
// Rust `kanban::http` router (proxied via the `orchestrator_request`
// Tauri command — see `lib/api/orchestrator.ts`).
//
// Behaviour:
// - When the feature flag is OFF, the route still mounts (so the
//   user can flip the flag from here too) but renders a single
//   call-to-action panel. The settings page exposes the same flag.
// - When the feature is ON, shows the board: columns per state
//   (Open / Triage / Ready / Code / AgentReview / HumanReview /
//   Merge / Done / NeedsHuman / Cancelled), a free-text input to
//   create new tasks, a global tick toggle in the header, and a
//   task-detail drawer for comments + state-specific actions.
// - The agent half (triage, orchestrator, workers, tick loop) is
//   built in a separate landing — without it tasks created here
//   will sit in `Open` indefinitely. That's expected during the
//   incremental rollout: the board still works as a manual UI for
//   inspecting the data model end-to-end.

import * as React from "react";
import {
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useSidebar, SidebarTrigger } from "@/components/ui/sidebar";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Switch } from "@/components/ui/switch";
import {
  Sheet,
  SheetContent,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";

// Lightweight inline replacements for `@/components/ui/badge` and
// `@/components/ui/scroll-area` — those shadcn components aren't in
// the project's `components/ui` directory yet and pulling them in
// for the kanban view alone would bloat the surface area. Both
// have minimal styling needs that a `<span>` / `<div>` with a few
// classes covers fine; if more places start needing them we can
// refactor to the canonical shadcn variants later.
function Badge({
  children,
  variant = "default",
  className,
  title,
}: {
  children: React.ReactNode;
  variant?: "default" | "outline" | "secondary";
  className?: string;
  title?: string;
}) {
  return (
    <span
      title={title}
      className={cn(
        "inline-flex items-center rounded px-1.5 py-0.5 text-[10px] font-medium leading-none",
        variant === "default" &&
          "bg-foreground text-background",
        variant === "outline" &&
          "border border-border bg-transparent text-foreground",
        variant === "secondary" &&
          "bg-muted text-muted-foreground",
        className,
      )}
    >
      {children}
    </span>
  );
}

function ScrollArea({
  children,
  className,
}: {
  children: React.ReactNode;
  className?: string;
}) {
  // The native overflow-auto is good enough for the board's content
  // sizes. shadcn's ScrollArea wraps Radix; we don't need the custom
  // scrollbar styling here.
  return <div className={cn("overflow-auto", className)}>{children}</div>;
}
import { isMacOS } from "@/lib/popout";
import { cn } from "@/lib/utils";
import { toast } from "@/hooks/use-toast";
import {
  BOARD_COLUMNS,
  COLUMN_LABEL,
  type CommentAuthor,
  type OrchestratorStatus,
  type Task,
  type TaskComment,
  type TaskSession,
  type TaskState,
  approveHumanReview,
  cancelTask,
  createTask,
  getStatus,
  listComments,
  listTaskSessions,
  listTasks,
  postComment,
  resolveNeedsHuman,
  setFeatureFlag,
  setTickEnabled,
} from "@/lib/api/orchestrator";

const REFRESH_INTERVAL_MS = 3_000;

export function KanbanView() {
  const { state: sidebarState } = useSidebar();
  const showMacTrafficSpacer = isMacOS() && sidebarState === "collapsed";

  // Status (feature flag + tick toggle + interval). Always polled
  // at the same cadence as the board itself so a flip on another
  // device / via the settings page is reflected quickly.
  const statusQuery = useQuery<OrchestratorStatus>({
    queryKey: ["orchestrator", "status"],
    queryFn: getStatus,
    refetchInterval: REFRESH_INTERVAL_MS,
  });

  const featureEnabled = statusQuery.data?.featureEnabled ?? false;

  return (
    <div className="flex h-full flex-col">
      <header
        data-tauri-drag-region
        className="flex h-9 items-center gap-1 border-b border-border px-2 text-sm text-muted-foreground"
      >
        {showMacTrafficSpacer && (
          <div className="w-16 shrink-0" data-tauri-drag-region />
        )}
        <SidebarTrigger />
        <span className="ml-1 font-medium text-foreground">Orchestrator</span>
        {featureEnabled && statusQuery.data ? (
          <div className="ml-auto flex items-center gap-3">
            <TickToggle status={statusQuery.data} />
          </div>
        ) : null}
      </header>
      <div className="relative min-h-0 min-w-0 flex-1 overflow-hidden">
        {!featureEnabled ? (
          <FeatureFlagGate
            loading={statusQuery.isLoading}
            error={statusQuery.error}
          />
        ) : (
          <Board />
        )}
      </div>
    </div>
  );
}

// ── feature flag gate ─────────────────────────────────────────────

function FeatureFlagGate({
  loading,
  error,
}: {
  loading: boolean;
  error: unknown;
}) {
  const qc = useQueryClient();
  const enableMutation = useMutation({
    mutationFn: () => setFeatureFlag(true),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["orchestrator"] }),
    onError: (err: unknown) =>
      toast({
        title: "Could not enable orchestrator",
        description: String(err),
      }),
  });
  return (
    <div className="flex h-full flex-col items-center justify-center gap-4 p-8 text-center">
      <h2 className="text-lg font-semibold">Orchestrator is off</h2>
      <p className="max-w-md text-sm text-muted-foreground">
        The orchestrator + kanban feature is experimental and ships
        disabled. Turning it on lets you drop free-text tasks on a
        board and have flowstate route them through projects with
        per-task orchestrator + worker agents. You can turn it back
        off any time from this screen or from Settings.
      </p>
      {error ? (
        <p className="max-w-md text-xs text-destructive">{String(error)}</p>
      ) : null}
      <Button
        onClick={() => enableMutation.mutate()}
        disabled={loading || enableMutation.isPending}
      >
        {enableMutation.isPending ? "Enabling…" : "Enable orchestrator"}
      </Button>
    </div>
  );
}

// ── tick toggle in the header ────────────────────────────────────

function TickToggle({ status }: { status: OrchestratorStatus }) {
  const qc = useQueryClient();
  const mutation = useMutation({
    mutationFn: (enabled: boolean) => setTickEnabled(enabled),
    onSuccess: () =>
      qc.invalidateQueries({ queryKey: ["orchestrator", "status"] }),
    onError: (err) =>
      toast({
        title: "Could not change loop state",
        description: String(err),
      }),
  });
  return (
    <label className="flex items-center gap-2 text-xs">
      <span>Loop</span>
      <Switch
        checked={status.tickEnabled}
        onCheckedChange={(v) => mutation.mutate(Boolean(v))}
        aria-label="Orchestrator loop toggle"
      />
      <span
        className={cn(
          "text-[10px] tabular-nums",
          status.tickEnabled
            ? "text-emerald-600 dark:text-emerald-400"
            : "text-muted-foreground",
        )}
      >
        {status.tickEnabled ? "ON" : "OFF"} · {Math.round(status.tickIntervalMs / 100) / 10}s
      </span>
    </label>
  );
}

// ── board ─────────────────────────────────────────────────────────

function Board() {
  const qc = useQueryClient();
  const tasksQuery = useQuery<Task[]>({
    queryKey: ["orchestrator", "tasks"],
    queryFn: listTasks,
    refetchInterval: REFRESH_INTERVAL_MS,
  });
  const [activeTaskId, setActiveTaskId] = React.useState<string | null>(null);

  // Group tasks by state once per render. The board's state set is
  // fixed, so this is a single O(n) pass over the task list.
  const byState = React.useMemo(() => {
    const out: Record<TaskState, Task[]> = {
      Open: [],
      Triage: [],
      Ready: [],
      Code: [],
      AgentReview: [],
      HumanReview: [],
      Merge: [],
      Done: [],
      NeedsHuman: [],
      Cancelled: [],
    };
    for (const t of tasksQuery.data ?? []) {
      out[t.state]?.push(t);
    }
    return out;
  }, [tasksQuery.data]);

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col">
      <NewTaskInput
        onCreated={() => {
          qc.invalidateQueries({ queryKey: ["orchestrator", "tasks"] });
        }}
      />
      <ScrollArea className="min-h-0 flex-1">
        <div className="flex gap-3 p-3">
          {BOARD_COLUMNS.map((col) => (
            <BoardColumn
              key={col}
              state={col}
              tasks={byState[col]}
              onTaskClick={(t) => setActiveTaskId(t.taskId)}
            />
          ))}
        </div>
      </ScrollArea>
      <TaskDetailDrawer
        taskId={activeTaskId}
        onClose={() => setActiveTaskId(null)}
      />
    </div>
  );
}

function NewTaskInput({ onCreated }: { onCreated: () => void }) {
  const [text, setText] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  async function submit() {
    const trimmed = text.trim();
    if (!trimmed) return;
    setBusy(true);
    try {
      await createTask(trimmed);
      setText("");
      onCreated();
    } catch (err) {
      toast({ title: "Could not create task", description: String(err) });
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="flex items-start gap-2 border-b border-border px-3 py-2">
      <Textarea
        value={text}
        onChange={(e) => setText(e.target.value)}
        placeholder="Describe what you want done…  (⌘↵ to submit)"
        // Keep the textarea compact so the board has maximum room.
        className="min-h-[2.5rem] resize-none"
        onKeyDown={(e) => {
          if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
            e.preventDefault();
            void submit();
          }
        }}
      />
      <Button onClick={() => void submit()} disabled={busy || !text.trim()}>
        {busy ? "Adding…" : "Add task"}
      </Button>
    </div>
  );
}

function BoardColumn({
  state,
  tasks,
  onTaskClick,
}: {
  state: TaskState;
  tasks: Task[];
  onTaskClick: (t: Task) => void;
}) {
  // Hide empty Cancelled column to reduce noise — visible only once
  // someone has actually cancelled something.
  if (state === "Cancelled" && tasks.length === 0) return null;
  return (
    <div className="flex w-[260px] shrink-0 flex-col gap-2 rounded-md border border-border bg-card/40 p-2">
      <div className="flex items-baseline justify-between px-1">
        <span className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
          {COLUMN_LABEL[state]}
        </span>
        <span className="text-[10px] tabular-nums text-muted-foreground">
          {tasks.length}
        </span>
      </div>
      <div className="flex flex-col gap-2">
        {tasks.length === 0 ? (
          <div className="rounded border border-dashed border-border/60 p-3 text-[11px] text-muted-foreground">
            empty
          </div>
        ) : (
          tasks.map((t) => (
            <TaskCard key={t.taskId} task={t} onClick={() => onTaskClick(t)} />
          ))
        )}
      </div>
    </div>
  );
}

function TaskCard({ task, onClick }: { task: Task; onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "group flex flex-col items-start gap-1 rounded border border-border bg-background px-2 py-2 text-left",
        "hover:border-foreground/40 focus-visible:outline-none focus-visible:ring-1",
      )}
    >
      <span className="line-clamp-2 text-sm font-medium leading-snug">
        {task.title}
      </span>
      <div className="flex w-full items-center justify-between gap-1">
        {task.projectId ? (
          <Badge variant="outline" className="text-[10px]">
            {abbrev(task.projectId)}
          </Badge>
        ) : (
          <span className="text-[10px] uppercase text-muted-foreground">
            unassigned
          </span>
        )}
        {task.needsHumanReason ? (
          <span
            className="truncate text-[10px] text-amber-600 dark:text-amber-400"
            title={task.needsHumanReason}
          >
            ! {task.needsHumanReason}
          </span>
        ) : null}
      </div>
    </button>
  );
}

function abbrev(id: string): string {
  // task_ / proj_ / cmt_ ids carry a uuid suffix; trim to 8 chars
  // for a card label.
  const parts = id.split("_");
  const tail = parts[parts.length - 1] ?? id;
  return `${parts[0] ?? "id"}_${tail.slice(0, 8)}`;
}

// ── task detail drawer ────────────────────────────────────────────

function TaskDetailDrawer({
  taskId,
  onClose,
}: {
  taskId: string | null;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const open = taskId !== null;

  const taskQuery = useQuery<Task[]>({
    // Reuse the list query so we don't double-fetch the same data;
    // pick the one we care about in the selector. Keeps cache simple.
    queryKey: ["orchestrator", "tasks"],
    queryFn: listTasks,
    refetchInterval: open ? REFRESH_INTERVAL_MS : false,
  });
  const task = taskQuery.data?.find((t) => t.taskId === taskId) ?? null;

  const commentsQuery = useQuery<TaskComment[]>({
    queryKey: ["orchestrator", "comments", taskId],
    queryFn: () => listComments(taskId!),
    enabled: open,
    refetchInterval: open ? REFRESH_INTERVAL_MS : false,
  });

  const sessionsQuery = useQuery<TaskSession[]>({
    queryKey: ["orchestrator", "sessions", taskId],
    queryFn: () => listTaskSessions(taskId!),
    enabled: open,
    refetchInterval: open ? REFRESH_INTERVAL_MS : false,
  });

  const [newComment, setNewComment] = React.useState("");
  React.useEffect(() => {
    if (!open) setNewComment("");
  }, [open]);

  const approveMutation = useMutation({
    mutationFn: () => approveHumanReview(taskId!),
    onSuccess: () =>
      qc.invalidateQueries({ queryKey: ["orchestrator"] }),
    onError: (err) =>
      toast({ title: "Approve failed", description: String(err) }),
  });
  const resolveMutation = useMutation({
    mutationFn: (comment: string) =>
      resolveNeedsHuman(taskId!, comment ? { comment } : {}),
    onSuccess: () =>
      qc.invalidateQueries({ queryKey: ["orchestrator"] }),
    onError: (err) =>
      toast({ title: "Resolve failed", description: String(err) }),
  });
  const cancelMutation = useMutation({
    mutationFn: () => cancelTask(taskId!),
    onSuccess: () =>
      qc.invalidateQueries({ queryKey: ["orchestrator"] }),
    onError: (err) =>
      toast({ title: "Cancel failed", description: String(err) }),
  });
  const commentMutation = useMutation({
    mutationFn: (body: string) =>
      postComment(taskId!, body, "user" satisfies CommentAuthor),
    onSuccess: () => {
      setNewComment("");
      qc.invalidateQueries({
        queryKey: ["orchestrator", "comments", taskId],
      });
    },
    onError: (err) =>
      toast({ title: "Comment failed", description: String(err) }),
  });

  return (
    <Sheet open={open} onOpenChange={(v) => !v && onClose()}>
      <SheetContent side="right" className="flex w-[520px] flex-col gap-3">
        <SheetHeader>
          <SheetTitle className="line-clamp-2 pr-8 text-base">
            {task?.title ?? "(loading)"}
          </SheetTitle>
        </SheetHeader>
        {task ? (
          <div className="flex min-h-0 flex-1 flex-col gap-3">
            <div className="flex flex-wrap items-center gap-1.5">
              <Badge>{task.state}</Badge>
              {task.projectId ? (
                <Badge variant="outline">{abbrev(task.projectId)}</Badge>
              ) : (
                <Badge variant="outline">unassigned</Badge>
              )}
              {task.branch ? (
                <Badge variant="secondary">{task.branch}</Badge>
              ) : null}
            </div>
            {task.needsHumanReason ? (
              <div className="rounded border border-amber-500/40 bg-amber-500/10 p-2 text-xs">
                <div className="font-medium">Needs Human</div>
                <div className="mt-0.5 whitespace-pre-wrap">
                  {task.needsHumanReason}
                </div>
              </div>
            ) : null}
            <div className="flex flex-wrap items-center gap-2">
              {task.state === "HumanReview" ? (
                <Button
                  size="sm"
                  onClick={() => approveMutation.mutate()}
                  disabled={approveMutation.isPending}
                >
                  Approve & merge
                </Button>
              ) : null}
              {task.state === "NeedsHuman" ? (
                <Button
                  size="sm"
                  onClick={() => resolveMutation.mutate("")}
                  disabled={resolveMutation.isPending}
                >
                  Resolve
                </Button>
              ) : null}
              {task.state !== "Done" && task.state !== "Cancelled" ? (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => cancelMutation.mutate()}
                  disabled={cancelMutation.isPending}
                >
                  Cancel task
                </Button>
              ) : null}
            </div>
            <details className="rounded border border-border p-2 text-xs">
              <summary className="cursor-pointer text-muted-foreground">
                Original request
              </summary>
              <pre className="mt-2 whitespace-pre-wrap break-words font-sans text-foreground">
                {task.body}
              </pre>
            </details>
            <div className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
              Sessions
            </div>
            <div className="flex flex-wrap gap-1.5">
              {(sessionsQuery.data ?? []).length === 0 ? (
                <span className="text-xs text-muted-foreground">
                  No agent sessions yet.
                </span>
              ) : (
                (sessionsQuery.data ?? []).map((s) => (
                  // Clickable badge — navigates to the session's
                  // chat thread so the user can inspect what the
                  // agent actually said. Closes the drawer first
                  // (otherwise the route change leaves an open
                  // sheet floating over the chat view).
                  <button
                    key={s.sessionId}
                    type="button"
                    onClick={() => {
                      onClose();
                      navigate({
                        to: "/chat/$sessionId",
                        params: { sessionId: s.sessionId },
                      });
                    }}
                    className="cursor-pointer focus-visible:outline-none focus-visible:ring-1 rounded"
                    title={`Open ${s.role} thread${s.retiredAt ? " (retired)" : ""}`}
                  >
                    <Badge
                      variant={s.retiredAt ? "outline" : "secondary"}
                      className="hover:bg-foreground hover:text-background"
                    >
                      {s.role}
                      {s.retiredAt ? " ✓" : " ·"}
                    </Badge>
                  </button>
                ))
              )}
            </div>
            <div className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
              Comments
            </div>
            <ScrollArea className="min-h-0 flex-1 rounded border border-border">
              <div className="flex flex-col divide-y divide-border">
                {(commentsQuery.data ?? []).length === 0 ? (
                  <div className="p-3 text-xs text-muted-foreground">
                    No comments yet.
                  </div>
                ) : (
                  (commentsQuery.data ?? []).map((c) => (
                    <CommentRow key={c.commentId} comment={c} />
                  ))
                )}
              </div>
            </ScrollArea>
            <div className="flex items-end gap-2">
              <Textarea
                value={newComment}
                onChange={(e) => setNewComment(e.target.value)}
                placeholder="Add a comment…  (⌘↵ to submit)"
                className="min-h-[2.5rem] resize-none"
                onKeyDown={(e) => {
                  if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
                    e.preventDefault();
                    if (newComment.trim()) commentMutation.mutate(newComment.trim());
                  }
                }}
              />
              <Button
                size="sm"
                onClick={() =>
                  newComment.trim() &&
                  commentMutation.mutate(newComment.trim())
                }
                disabled={!newComment.trim() || commentMutation.isPending}
              >
                Post
              </Button>
            </div>
          </div>
        ) : null}
      </SheetContent>
    </Sheet>
  );
}

function CommentRow({ comment }: { comment: TaskComment }) {
  return (
    <div className="flex flex-col gap-1 p-2">
      <div className="flex items-center justify-between text-[10px] uppercase tracking-wide text-muted-foreground">
        <span>{comment.author}</span>
        <time dateTime={new Date(comment.createdAt * 1000).toISOString()}>
          {new Date(comment.createdAt * 1000).toLocaleString()}
        </time>
      </div>
      <div className="whitespace-pre-wrap break-words text-sm">{comment.body}</div>
    </div>
  );
}

// Silence dead-code warnings on the ad-hoc Input import — kept so a
// later refactor that swaps Textarea→Input on the title field
// doesn't have to re-import.
const _input_referenced = Input;
void _input_referenced;
