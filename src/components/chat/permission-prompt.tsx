import * as React from "react";
import type { PermissionDecision, PermissionMode } from "@/lib/types";
import { sendMessage } from "@/lib/api";
import { isPlanExitTool, renderToolArgs } from "./tool-renderers";

interface PermissionPromptProps {
  toolName: string;
  input: unknown;
  sessionId: string;
  /** Called when the user approves or denies the request. */
  onDecision: (decision: PermissionDecision) => void;
  /** Updates chat-view's local permissionMode state — keeps the toolbar
   *  picker in sync after a plan-exit approval. */
  onSwitchMode?: (mode: PermissionMode) => void;
}

const PLAN_EXIT_MODES: { mode: PermissionMode; label: string; hint: string }[] = [
  { mode: "default", label: "Default", hint: "ask before each edit" },
  { mode: "accept_edits", label: "Auto-edit", hint: "edits without asking" },
  { mode: "bypass", label: "Full access", hint: "no permission prompts at all" },
];

function PermissionPromptInner({
  toolName,
  input,
  sessionId,
  onDecision,
  onSwitchMode,
}: PermissionPromptProps) {
  const planExit = isPlanExitTool(toolName);

  return (
    <div className="shrink-0 border-t border-amber-500/40 bg-amber-500/5 px-3 py-2.5">
      {planExit ? (
        <PlanExitPrompt
          input={input}
          sessionId={sessionId}
          onDecision={onDecision}
          onSwitchMode={onSwitchMode}
        />
      ) : (
        <DefaultPrompt
          toolName={toolName}
          input={input}
          onDecision={onDecision}
        />
      )}
    </div>
  );
}

function DefaultPrompt({
  toolName,
  input,
  onDecision,
}: {
  toolName: string;
  input: unknown;
  onDecision: (decision: PermissionDecision) => void;
}) {
  const [showArgs, setShowArgs] = React.useState(false);
  return (
    <>
      <div className="flex items-center justify-between gap-3">
        <div className="min-w-0 flex-1 text-sm">
          Permission requested:{" "}
          <span className="font-mono font-medium">{toolName}</span>
        </div>
        <div className="flex shrink-0 gap-2">
          <button
            type="button"
            onClick={() => onDecision("allow")}
            className="rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90"
          >
            Allow
          </button>
          <button
            type="button"
            onClick={() => onDecision("allow_always")}
            className="rounded-md border border-input bg-background px-3 py-1.5 text-xs font-medium hover:bg-accent"
          >
            Always
          </button>
          <button
            type="button"
            onClick={() => onDecision("deny")}
            className="rounded-md bg-destructive px-3 py-1.5 text-xs font-medium text-destructive-foreground hover:bg-destructive/90"
          >
            Deny
          </button>
        </div>
      </div>
      <div className="mt-2 text-xs text-muted-foreground">
        <button
          type="button"
          onClick={() => setShowArgs((s) => !s)}
          className="cursor-pointer hover:text-foreground"
        >
          {showArgs ? "hide" : "show"} args
        </button>
        {showArgs && <div className="mt-2">{renderToolArgs(toolName, input)}</div>}
      </div>
    </>
  );
}

function PlanExitPrompt({
  input,
  sessionId,
  onDecision,
  onSwitchMode,
}: {
  input: unknown;
  sessionId: string;
  onDecision: (decision: PermissionDecision) => void;
  onSwitchMode?: (mode: PermissionMode) => void;
}) {
  const [pending, setPending] = React.useState(false);

  async function approveWith(mode: PermissionMode) {
    if (pending) return;
    setPending(true);
    // 1. Tell the daemon to switch the active SDK query's mode BEFORE
    //    we release the permission. The Claude bridge calls
    //    query.setPermissionMode(mode) on the live Query handle, so the
    //    rest of this turn runs under the chosen mode. Adapters that
    //    don't support mid-turn switching no-op silently.
    // 2. Sync the chat-view local state so the toolbar reflects the
    //    new mode and subsequent send_turn calls inherit it.
    // 3. Release the permission gate so the model proceeds.
    try {
      await sendMessage({
        type: "update_permission_mode",
        session_id: sessionId,
        permission_mode: mode,
      });
    } catch (err) {
      // Don't block approval on a bridge error — log and continue.
      console.error("update_permission_mode failed", err);
    }
    onSwitchMode?.(mode);
    onDecision("allow");
  }

  return (
    <div className="space-y-3">
      <div className="text-sm font-medium">Plan ready for review</div>
      {/* The plan body — rendered as markdown via the ExitPlanMode renderer. */}
      <div className="max-h-80 overflow-auto">
        {renderToolArgs("ExitPlanMode", input)}
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-xs text-muted-foreground">Approve and switch to:</span>
        {PLAN_EXIT_MODES.map((opt) => (
          <button
            key={opt.mode}
            type="button"
            disabled={pending}
            onClick={() => approveWith(opt.mode)}
            title={opt.hint}
            className="rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
          >
            {opt.label}
          </button>
        ))}
        <button
          type="button"
          disabled={pending}
          onClick={() => onDecision("deny")}
          className="ml-auto rounded-md bg-destructive px-3 py-1.5 text-xs font-medium text-destructive-foreground hover:bg-destructive/90 disabled:opacity-50"
        >
          Reject plan
        </button>
      </div>
      <p className="text-[11px] text-muted-foreground">
        The chosen mode applies for the rest of this turn (Claude SDK) and
        every subsequent message.
      </p>
    </div>
  );
}

export const PermissionPrompt = React.memo(PermissionPromptInner);
