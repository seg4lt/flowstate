import { Button } from "../ui/button";
import { Badge } from "../ui/badge";
import { actions, useAppStore, type SendClientMessage } from "../../state/appStore";
import type { PendingPermission, PermissionDecision } from "../../types";

interface Props {
  sendClientMessage: SendClientMessage;
}

export function PermissionDialog({ sendClientMessage }: Props) {
  const pending = useAppStore((s) => s.pendingPermissions);

  if (pending.length === 0) return null;
  const current = pending[0];

  const answer = (req: PendingPermission, decision: PermissionDecision) => {
    sendClientMessage({
      type: "answer_permission",
      session_id: req.sessionId,
      request_id: req.requestId,
      decision,
    });
    actions.removePermission(req.requestId);
  };

  return (
    <div className="fixed inset-0 flex items-center justify-center bg-background/70 backdrop-blur-sm z-50">
      <div className="w-[480px] max-w-[90vw] rounded-lg border border-border bg-card text-card-foreground shadow-xl p-5 space-y-4">
        <div className="flex items-center justify-between">
          <h3 className="text-sm font-semibold">Permission required</h3>
          {pending.length > 1 && (
            <Badge variant="secondary" className="text-[10px]">
              +{pending.length - 1} queued
            </Badge>
          )}
        </div>
        <div className="space-y-2">
          <div className="text-xs text-muted-foreground">Tool</div>
          <code className="block text-xs font-mono bg-muted/50 rounded px-2 py-1">
            {current.toolName}
          </code>
        </div>
        <div className="space-y-2">
          <div className="text-xs text-muted-foreground">Input</div>
          <pre className="text-xs font-mono bg-muted/50 rounded p-2 max-h-48 overflow-auto whitespace-pre-wrap">
            {JSON.stringify(current.input, null, 2)}
          </pre>
        </div>
        <div className="grid grid-cols-2 gap-2 pt-2">
          <Button size="sm" onClick={() => answer(current, "allow")}>
            Allow once
          </Button>
          <Button
            size="sm"
            variant="secondary"
            onClick={() => answer(current, "allow_always")}
          >
            Allow always
          </Button>
          <Button
            size="sm"
            variant="outline"
            onClick={() => answer(current, "deny")}
          >
            Deny once
          </Button>
          <Button
            size="sm"
            variant="destructive"
            onClick={() => answer(current, "deny_always")}
          >
            Deny always
          </Button>
        </div>
      </div>
    </div>
  );
}
