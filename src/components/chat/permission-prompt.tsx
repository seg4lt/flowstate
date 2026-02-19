import * as React from "react";
import type { PermissionDecision } from "@/lib/types";

interface PermissionPromptProps {
  toolName: string;
  input: unknown;
  onDecision: (decision: PermissionDecision) => void;
}

function PermissionPromptInner({
  toolName,
  input,
  onDecision,
}: PermissionPromptProps) {
  return (
    <div className="shrink-0 border-t border-amber-500/40 bg-amber-500/5 px-3 py-2.5">
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
      <details className="mt-2 text-xs text-muted-foreground">
        <summary className="cursor-pointer select-none hover:text-foreground">
          show args
        </summary>
        <pre className="mt-2 max-h-40 overflow-auto rounded bg-background p-2 text-[11px]">
          {JSON.stringify(input, null, 2)}
        </pre>
      </details>
    </div>
  );
}

export const PermissionPrompt = React.memo(PermissionPromptInner);
