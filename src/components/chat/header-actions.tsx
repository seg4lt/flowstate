import { Plus, FolderOpen, GitCommitVertical } from "lucide-react";
import { Button } from "@/components/ui/button";

export function HeaderActions() {
  return (
    <div className="flex items-center gap-1">
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
        onClick={() => {
          // TODO: implement open
        }}
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
