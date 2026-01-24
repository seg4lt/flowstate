import { Badge } from "../ui/badge";
import { Button } from "../ui/button";
import type { PlanRecord } from "../../types";
import type { SendClientMessage } from "../../state/appStore";

interface Props {
  sessionId: string;
  plan: PlanRecord;
  sendClientMessage: SendClientMessage;
}

export function PlanCard({ sessionId, plan, sendClientMessage }: Props) {
  const statusVariant =
    plan.status === "accepted"
      ? "default"
      : plan.status === "rejected"
        ? "destructive"
        : "secondary";

  return (
    <div className="rounded-md border border-border bg-muted/30 p-3 space-y-2 mt-2">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <span className="text-sm font-semibold">{plan.title}</span>
          <Badge variant={statusVariant} className="text-[10px] h-4">
            {plan.status}
          </Badge>
        </div>
        {plan.status === "proposed" && (
          <div className="flex gap-2">
            <Button
              size="sm"
              onClick={() =>
                sendClientMessage({
                  type: "accept_plan",
                  session_id: sessionId,
                  plan_id: plan.planId,
                })
              }
            >
              Accept
            </Button>
            <Button
              size="sm"
              variant="outline"
              onClick={() =>
                sendClientMessage({
                  type: "reject_plan",
                  session_id: sessionId,
                  plan_id: plan.planId,
                })
              }
            >
              Reject
            </Button>
          </div>
        )}
      </div>
      {plan.steps.length > 0 ? (
        <ol className="list-decimal pl-5 space-y-1 text-sm">
          {plan.steps.map((step, idx) => (
            <li key={idx}>
              {step.detail ? (
                <details>
                  <summary className="cursor-pointer">{step.title}</summary>
                  <div className="text-xs text-muted-foreground whitespace-pre-wrap mt-1">
                    {step.detail}
                  </div>
                </details>
              ) : (
                step.title
              )}
            </li>
          ))}
        </ol>
      ) : (
        <pre className="text-xs whitespace-pre-wrap text-muted-foreground">{plan.raw}</pre>
      )}
    </div>
  );
}
