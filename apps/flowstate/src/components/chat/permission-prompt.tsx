import * as React from "react";
import type { PermissionDecision, PermissionMode } from "@/lib/types";
import { isPlanExitTool, isPlanEnterTool, renderPlanBody, renderToolArgs } from "./tool-renderers";

interface PermissionPromptProps {
  toolName: string;
  input: unknown;
  /** Called when the user approves or denies the request. The optional
   *  `modeOverride` is set by the plan-exit and plan-enter flows and is
   *  bundled into the daemon's answer_permission so the SDK applies the
   *  mode change as part of accepting the tool call. The optional
   *  `feedback` is set by plan-exit "Send feedback" and is threaded to
   *  the Claude SDK as the `message` field of `{behavior:'deny', message}`
   *  so the model sees it as the tool-denial reason and can iterate on
   *  the plan within the same turn. */
  onDecision: (
    decision: PermissionDecision,
    modeOverride?: PermissionMode,
    feedback?: string,
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
  { mode: "accept_edits", label: "Accept Edits", hint: "edits without asking" },
  { mode: "bypass", label: "Bypass Permissions", hint: "no permission prompts at all" },
];

function PermissionPromptInner({
  toolName,
  input,
  onDecision,
  queueDepth = 1,
}: PermissionPromptProps) {
  const planExit = isPlanExitTool(toolName);
  const planEnter = isPlanEnterTool(toolName);

  return (
    <div className="shrink-0 border-t border-amber-500/40 bg-amber-500/5 px-3 py-2.5">
      {queueDepth > 1 && (
        <div className="mb-1.5 text-[11px] font-medium uppercase tracking-wide text-amber-600 dark:text-amber-400">
          {queueDepth} permissions queued
        </div>
      )}
      {planExit ? (
        <PlanExitPrompt input={input} onDecision={onDecision} />
      ) : planEnter ? (
        <PlanEnterPrompt onDecision={onDecision} />
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
        {showArgs && (
          // Cap at ~60vh so a huge tool payload (big Write, long
          // Bash, multi-hunk Edit) can't push Allow / Deny and the
          // composer off-screen. The buttons sit in the row above
          // this container so they always stay visible; the args
          // scroll internally. 60vh + prompt chrome lands the total
          // at ~2/3 of the viewport.
          <div className="mt-2 max-h-[60vh] overflow-y-auto">
            {renderToolArgs(toolName, input)}
          </div>
        )}
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
    feedback?: string,
  ) => void;
}) {
  const [pending, setPending] = React.useState(false);
  // Free-form feedback surfaced to the model on deny. Sending it
  // resolves the pending canUseTool with `{behavior:'deny', message}`,
  // which the SDK passes to the model as the synthetic tool_result —
  // so the user can steer a plan without restarting the turn. Empty
  // textarea keeps the original "Reject plan" behavior intact.
  const [feedback, setFeedback] = React.useState("");
  const trimmedFeedback = feedback.trim();
  const hasFeedback = trimmedFeedback.length > 0;

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

  function denyWithFeedback() {
    if (pending) return;
    setPending(true);
    onDecision("deny", undefined, hasFeedback ? trimmedFeedback : undefined);
  }

  return (
    <div className="space-y-3">
      <div className="text-sm font-medium">Plan ready for review</div>
      {/* The plan body — bare markdown so it sits flush inside the amber
          banner. The framed variant lives in ExitPlanModeRenderer and is
          used by the post-approval tool card. */}
      <div className="max-h-80 overflow-auto">
        {renderPlanBody(input) ?? renderToolArgs("ExitPlanMode", input)}
      </div>
      {/* Optional feedback textarea. Typing here flips the reject button
          from "Reject plan" (canned denial) to "Send feedback" (denial
          with a message the model iterates on within the same turn). */}
      <textarea
        value={feedback}
        onChange={(e) => setFeedback(e.target.value)}
        disabled={pending}
        rows={3}
        placeholder="Optional: steer the plan — e.g. reuse `bar()` from utils.ts instead of writing new code"
        className="w-full resize-y rounded-md border border-amber-500/30 bg-background/60 px-2 py-1.5 text-xs text-foreground placeholder:text-muted-foreground focus:border-amber-500/60 focus:outline-none focus:ring-1 focus:ring-amber-500/40 disabled:opacity-50"
      />
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
          onClick={denyWithFeedback}
          className="ml-auto rounded-md bg-destructive px-3 py-1.5 text-xs font-medium text-destructive-foreground hover:bg-destructive/90 disabled:opacity-50"
        >
          {hasFeedback ? "Send feedback" : "Reject plan"}
        </button>
      </div>
      <p className="text-[11px] text-muted-foreground">
        The chosen mode applies for the rest of this turn (Claude SDK) and
        every subsequent message.
      </p>
    </div>
  );
}

function PlanEnterPrompt({
  onDecision,
}: {
  onDecision: (
    decision: PermissionDecision,
    modeOverride?: PermissionMode,
  ) => void;
}) {
  const [pending, setPending] = React.useState(false);

  function approve() {
    if (pending) return;
    setPending(true);
    // Bundle "plan" as the mode override so the SDK applies the mode
    // change atomically with the tool approval — same pattern as
    // PlanExitPrompt. The bridge attaches updatedPermissions:
    // [{ type: 'setMode', mode: 'plan', destination: 'session' }]
    // and the SDK switches to plan mode within the same turn.
    onDecision("allow", "plan");
  }

  return (
    <div className="space-y-3">
      <div className="text-sm font-medium">
        Agent wants to switch to Plan mode
      </div>
      <p className="text-xs text-muted-foreground">
        In plan mode the agent investigates and plans without making changes.
        You can switch back to a different mode at any time.
      </p>
      <div className="flex items-center gap-2">
        <button
          type="button"
          disabled={pending}
          onClick={approve}
          className="rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
        >
          Allow
        </button>
        <button
          type="button"
          disabled={pending}
          onClick={() => onDecision("deny")}
          className="rounded-md bg-destructive px-3 py-1.5 text-xs font-medium text-destructive-foreground hover:bg-destructive/90 disabled:opacity-50"
        >
          Deny
        </button>
      </div>
    </div>
  );
}

export const PermissionPrompt = React.memo(PermissionPromptInner);
