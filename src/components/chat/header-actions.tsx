import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import {
  Diff,
  FolderOpen,
  GitCommitVertical,
  Plus,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import type { AggregatedFileDiff } from "@/lib/session-diff";

interface HeaderActionsProps {
  sessionId: string;
  diffs: AggregatedFileDiff[];
  diffOpen: boolean;
  onToggleDiff: () => void;
}

export function HeaderActions({
  sessionId,
  diffs,
  diffOpen,
  onToggleDiff,
}: HeaderActionsProps) {
  const navigate = useNavigate();
  const { additions, deletions } = React.useMemo(() => {
    let a = 0;
    let d = 0;
    for (const diff of diffs) {
      a += diff.additions;
      d += diff.deletions;
    }
    return { additions: a, deletions: d };
  }, [diffs]);

  const hasChanges = diffs.length > 0;

  return (
    <div className="flex items-center gap-1">
      {hasChanges && (
        <Button
          variant={diffOpen ? "secondary" : "outline"}
          size="xs"
          onClick={onToggleDiff}
          aria-pressed={diffOpen}
          title={
            diffOpen ? "Hide diff panel" : "Show diff panel for this session"
          }
        >
          <Diff className="h-3 w-3" />
          Show diff
          <span className="ml-0.5 tabular-nums text-green-600 dark:text-green-400">
            +{additions}
          </span>
          <span className="tabular-nums text-red-600 dark:text-red-400">
            −{deletions}
          </span>
        </Button>
      )}
      <Button
        variant="outline"
        size="xs"
        onClick={() => {
          // TODO: implement add action
        }}
      >
        <Plus className="h-3 w-3" />
        Add action
      </Button>
      <Button
        variant="outline"
        size="xs"
        onClick={() =>
          navigate({ to: "/code/$sessionId", params: { sessionId } })
        }
        title="Open the project file browser"
      >
        <FolderOpen className="h-3 w-3" />
        Open
      </Button>
      <Button
        variant="outline"
        size="xs"
        onClick={() => {
          // TODO: implement commit & push
        }}
      >
        <GitCommitVertical className="h-3 w-3" />
        Commit & push
      </Button>
    </div>
  );
}
