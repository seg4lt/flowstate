import * as React from "react";
import { Target, ChevronDown, Trash2, Pause, Play, Check } from "lucide-react";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { useApp, useThreadGoal } from "@/stores/app-store";
import { toast } from "@/hooks/use-toast";
import type { ThreadGoalStatus } from "@/lib/types";

interface GoalChipProps {
  sessionId: string;
}

/** Status → human label + accent class. Codex distinguishes
 *  `budgetLimited` from `paused` (the agent stopped on its own when
 *  it hit its token cap, vs an explicit user pause) — surface both so
 *  the user can tell at a glance whether they need to bump the budget
 *  or hit Resume. */
const STATUS_META: Record<
  ThreadGoalStatus,
  { label: string; tone: string }
> = {
  active: { label: "Active", tone: "text-emerald-600 dark:text-emerald-400" },
  paused: { label: "Paused", tone: "text-amber-600 dark:text-amber-400" },
  budgetLimited: {
    label: "Budget reached",
    tone: "text-orange-600 dark:text-orange-400",
  },
  complete: { label: "Complete", tone: "text-muted-foreground" },
};

/** Compact integer formatter for token counts ("12.3k", "1.2M"). */
function formatTokens(n: number): string {
  if (n < 1_000) return n.toString();
  if (n < 1_000_000) return `${(n / 1_000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

export function GoalChip({ sessionId }: GoalChipProps) {
  const goal = useThreadGoal(sessionId);
  const { send } = useApp();
  const [dialogOpen, setDialogOpen] = React.useState(false);
  // Loading flag covers all three RPC paths (set / pause-resume /
  // clear). The chip stays clickable so the user can cancel the
  // dialog, but the action buttons inside disable to prevent double-
  // submits on a slow daemon.
  const [pending, setPending] = React.useState(false);

  // ----- handlers ---------------------------------------------------

  const submitSet = React.useCallback(
    async (objective: string, tokenBudget: number | null) => {
      setPending(true);
      try {
        const resp = await send({
          type: "set_goal",
          session_id: sessionId,
          objective,
          // ClientMessage's TS shape uses `?: number`, so send
          // `undefined` (omitted on the wire) for "no cap" rather
          // than `null` — null would break tsc and serialize as
          // explicit JSON null, which the runtime's serde would
          // reject as a deserialization error rather than treat as
          // "leave alone".
          token_budget: tokenBudget ?? undefined,
          // `Active` on create; explicit so a re-set after a Pause
          // resumes the goal in one round-trip.
          status: "active",
        });
        if (resp?.type === "error") {
          toast({ title: "Couldn't set goal", description: resp.message });
          return false;
        }
        return true;
      } finally {
        setPending(false);
      }
    },
    [send, sessionId],
  );

  const updateStatus = React.useCallback(
    async (status: ThreadGoalStatus) => {
      if (!goal) return;
      setPending(true);
      try {
        const resp = await send({
          type: "set_goal",
          session_id: sessionId,
          // Re-send the current objective + budget so codex's
          // ThreadGoalSetParams treats this as a status-only update;
          // the runtime forwards verbatim.
          objective: goal.objective,
          token_budget: goal.tokenBudget ?? undefined,
          status,
        });
        if (resp?.type === "error") {
          toast({
            title: `Couldn't ${status} goal`,
            description: resp.message,
          });
        }
      } finally {
        setPending(false);
      }
    },
    [goal, send, sessionId],
  );

  const clearGoal = React.useCallback(async () => {
    setPending(true);
    try {
      const resp = await send({ type: "clear_goal", session_id: sessionId });
      if (resp?.type === "error") {
        toast({ title: "Couldn't clear goal", description: resp.message });
      }
    } finally {
      setPending(false);
    }
  }, [send, sessionId]);

  // ----- render -----------------------------------------------------

  // No goal: render a thin "Set goal" chip that opens the dialog.
  // Same visual weight as Mode/Effort chips so the toolbar doesn't
  // jump when a goal arrives.
  if (!goal) {
    return (
      <>
        <button
          type="button"
          className="inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-xs text-muted-foreground hover:text-foreground"
          onClick={() => setDialogOpen(true)}
        >
          <Target className="h-3 w-3" />
          Set goal
        </button>
        <GoalDialog
          open={dialogOpen}
          onOpenChange={setDialogOpen}
          pending={pending}
          mode="create"
          onSubmit={async (objective, budget) => {
            const ok = await submitSet(objective, budget);
            if (ok) setDialogOpen(false);
          }}
        />
      </>
    );
  }

  const meta = STATUS_META[goal.status];
  const usagePct =
    goal.tokenBudget && goal.tokenBudget > 0
      ? Math.min(100, Math.round((goal.tokensUsed / goal.tokenBudget) * 100))
      : null;

  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <button
            type="button"
            className={`inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-xs hover:text-foreground ${meta.tone}`}
            title={goal.objective}
          >
            <Target className="h-3 w-3" />
            <span className="max-w-[16ch] truncate">{goal.objective}</span>
            {usagePct !== null && (
              <span className="text-muted-foreground">· {usagePct}%</span>
            )}
            <ChevronDown className="h-3 w-3 text-muted-foreground" />
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" className="min-w-64">
          <DropdownMenuLabel className="flex items-center gap-1">
            <span className={meta.tone}>{meta.label}</span>
            <span className="text-muted-foreground">· goal</span>
          </DropdownMenuLabel>
          <div className="px-2 pb-2 text-xs">
            <div className="break-words text-foreground">{goal.objective}</div>
            <div className="mt-1 text-muted-foreground">
              {formatTokens(goal.tokensUsed)} tokens
              {goal.tokenBudget != null && goal.tokenBudget > 0
                ? ` / ${formatTokens(goal.tokenBudget)} budget`
                : ""}
              {goal.timeUsedSeconds > 0 &&
                ` · ${Math.round(goal.timeUsedSeconds / 60)} min`}
            </div>
          </div>
          <DropdownMenuSeparator />
          <DropdownMenuItem
            disabled={pending}
            onClick={() => setDialogOpen(true)}
          >
            <Check className="mr-2 h-3 w-3" />
            Edit goal
          </DropdownMenuItem>
          {goal.status === "active" && (
            <DropdownMenuItem
              disabled={pending}
              onClick={() => updateStatus("paused")}
            >
              <Pause className="mr-2 h-3 w-3" />
              Pause
            </DropdownMenuItem>
          )}
          {(goal.status === "paused" || goal.status === "budgetLimited") && (
            <DropdownMenuItem
              disabled={pending}
              onClick={() => updateStatus("active")}
            >
              <Play className="mr-2 h-3 w-3" />
              Resume
            </DropdownMenuItem>
          )}
          {goal.status !== "complete" && (
            <DropdownMenuItem
              disabled={pending}
              onClick={() => updateStatus("complete")}
            >
              <Check className="mr-2 h-3 w-3" />
              Mark complete
            </DropdownMenuItem>
          )}
          <DropdownMenuSeparator />
          <DropdownMenuItem
            disabled={pending}
            className="text-destructive focus:text-destructive"
            onClick={clearGoal}
          >
            <Trash2 className="mr-2 h-3 w-3" />
            Clear goal
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <GoalDialog
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        pending={pending}
        mode="edit"
        initialObjective={goal.objective}
        initialBudget={goal.tokenBudget ?? null}
        onSubmit={async (objective, budget) => {
          const ok = await submitSet(objective, budget);
          if (ok) setDialogOpen(false);
        }}
      />
    </>
  );
}

// ---------------------------------------------------------------------
// Dialog
// ---------------------------------------------------------------------

interface GoalDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  pending: boolean;
  mode: "create" | "edit";
  initialObjective?: string;
  initialBudget?: number | null;
  /** `tokenBudget` is `null` when the user clears the field — the
   *  runtime forwards `None` and codex treats that as "no cap". */
  onSubmit: (objective: string, tokenBudget: number | null) => Promise<void>;
}

function GoalDialog({
  open,
  onOpenChange,
  pending,
  mode,
  initialObjective,
  initialBudget,
  onSubmit,
}: GoalDialogProps) {
  const [objective, setObjective] = React.useState("");
  const [budgetText, setBudgetText] = React.useState("");

  // Reset state on every open. Without this, opening the dialog again
  // after a successful set would show stale text from the prior
  // session's goal — a footgun once chat-toolbar is shared across
  // sessions in tabs.
  React.useEffect(() => {
    if (open) {
      setObjective(initialObjective ?? "");
      setBudgetText(
        initialBudget != null && initialBudget > 0
          ? String(initialBudget)
          : "",
      );
    }
  }, [open, initialObjective, initialBudget]);

  const trimmed = objective.trim();
  const parsedBudget = budgetText.trim() === "" ? null : Number(budgetText);
  const budgetInvalid =
    parsedBudget !== null &&
    (!Number.isFinite(parsedBudget) || parsedBudget < 1);
  const canSubmit = trimmed.length > 0 && !budgetInvalid && !pending;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            {mode === "create" ? "Set goal" : "Edit goal"}
          </DialogTitle>
          <DialogDescription>
            Codex will track this as a long-running objective for the
            thread, with optional token budget. The agent can also
            update or complete the goal on its own as work progresses.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          <label className="block text-xs">
            <div className="mb-1 text-muted-foreground">Objective</div>
            <Input
              value={objective}
              onChange={(e) => setObjective(e.target.value)}
              placeholder="e.g. Ship the auth-rewrite branch with tests"
              autoFocus
            />
          </label>
          <label className="block text-xs">
            <div className="mb-1 text-muted-foreground">
              Token budget <span className="opacity-60">(optional)</span>
            </div>
            <Input
              type="number"
              min={1}
              value={budgetText}
              onChange={(e) => setBudgetText(e.target.value)}
              placeholder="e.g. 100000"
            />
            {budgetInvalid && (
              <div className="mt-1 text-destructive">
                Budget must be a positive number, or empty for no cap.
              </div>
            )}
          </label>
        </div>
        <DialogFooter>
          <Button
            variant="ghost"
            onClick={() => onOpenChange(false)}
            disabled={pending}
          >
            Cancel
          </Button>
          <Button
            disabled={!canSubmit}
            onClick={() => {
              void onSubmit(trimmed, parsedBudget);
            }}
          >
            {pending ? "Saving…" : mode === "create" ? "Set goal" : "Save"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
