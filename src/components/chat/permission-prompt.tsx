import * as React from "react";
import type { PermissionDecision, PermissionMode } from "@/lib/types";
import { isPlanExitTool, renderToolArgs } from "./tool-renderers";

interface PermissionPromptProps {
  toolName: string;
  input: unknown;
  /** Called when the user approves or denies the request. The optional
   *  modeOverride is only set by the plan-exit flow and is bundled into
   *  the daemon's answer_permission so the SDK applies the mode change
   *  as part of accepting the tool call. */
  onDecision: (
    decision: PermissionDecision,
    modeOverride?: PermissionMode,
  ) => void;
  /** Total number of queued prompts including this one. When the SDK
   *  fires parallel canUseTool callbacks (e.g. three Grep calls in one
   *  assistant turn), every answer pops one prompt and the next one
   *  slides in; showing "1 of 3" tells the user why clicking Allow
   *  doesn't make the prompt disappear. Defaults to 1. */
  queueDepth?: number;
}

const PLAN_EXIT_MODES: { mode: PermissionMode; label: string; hint: string }[] = [
  { mode: "default", label: "Default", hint: "ask before each edit" },
  { mode: "accept_edits", label: "Auto-edit", hint: "edits without asking" },
  { mode: "bypass", label: "Full access", hint: "no permission prompts at all" },
];

function PermissionPromptInner({
  toolName,
  input,
  onDecision,
  queueDepth = 1,
}: PermissionPromptProps) {
  const planExit = isPlanExitTool(toolName);

  return (
    <div className="shrink-0 border-t border-amber-500/40 bg-amber-500/5 px-3 py-2.5">
      {queueDepth > 1 && (
        <div className="mb-1.5 text-[11px] font-medium uppercase tracking-wide text-amber-600 dark:text-amber-400">
          {queueDepth} permissions queued
        </div>
      )}
      {planExit ? (
        <PlanExitPrompt input={input} onDecision={onDecision} />
      ) : (
        <DefaultPrompt
          toolName={toolName}
          input={input}
          onDecision={(decision) => onDecision(decision)}
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
  const [showArgs, setShowArgs] = React.useState(true);
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
  onDecision,
}: {
  input: unknown;
  onDecision: (
    decision: PermissionDecision,
    modeOverride?: PermissionMode,
  ) => void;
}) {
  const [pending, setPending] = React.useState(false);

  function approveWith(mode: PermissionMode) {
    if (pending) return;
    setPending(true);
    // Single combined answer_permission with the mode bundled. The
    // daemon stashes the mode on the sink, the Claude SDK adapter pulls
    // it via take_mode_override, and the bridge attaches
    // updatedPermissions: [{ type: 'setMode', mode, destination:
    // 'session' }] to the canUseTool result. The SDK applies the mode
    // change AS PART OF accepting the tool call, which is the only
    // path that makes the model continue executing within the same
    // turn after exiting plan mode. Calling setPermissionMode
    // separately and then resolving the permission doesn't work — the
    // SDK's plan-mode constraints have the model winding down by then.
    onDecision("allow", mode);
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
