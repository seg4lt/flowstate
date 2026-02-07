import type { PermissionDecision } from "@/lib/types";

interface PermissionDialogProps {
  toolName: string;
  input: unknown;
  suggested: string;
  onDecision: (decision: PermissionDecision) => void;
}

export function PermissionDialog({
  toolName,
  input,
  onDecision,
}: PermissionDialogProps) {
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
      <div className="mx-4 w-full max-w-md rounded-lg border border-border bg-background p-4 shadow-lg">
        <h3 className="mb-2 text-sm font-semibold">Permission Required</h3>
        <p className="mb-3 text-sm text-muted-foreground">
          The agent wants to use{" "}
          <span className="font-mono font-medium text-foreground">
            {toolName}
          </span>
        </p>

        <pre className="mb-4 max-h-40 overflow-auto rounded bg-muted p-2 text-xs">
          {JSON.stringify(input, null, 2)}
        </pre>

        <div className="flex gap-2">
          <button
            type="button"
            onClick={() => onDecision("allow")}
            className="flex-1 rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90"
          >
            Allow
          </button>
          <button
            type="button"
            onClick={() => onDecision("allow_always")}
            className="flex-1 rounded-md border border-input bg-background px-3 py-1.5 text-xs font-medium hover:bg-accent"
          >
            Always Allow
          </button>
          <button
            type="button"
            onClick={() => onDecision("deny")}
            className="flex-1 rounded-md bg-destructive px-3 py-1.5 text-xs font-medium text-destructive-foreground hover:bg-destructive/90"
          >
            Deny
          </button>
        </div>
      </div>
    </div>
  );
}
