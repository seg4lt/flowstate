import { Archive, Ellipsis, Trash2 } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useApp } from "@/stores/app-store";

interface ThreadActionsProps {
  sessionId: string;
}

export function ThreadActions({ sessionId }: ThreadActionsProps) {
  const { send } = useApp();

  async function handleArchive() {
    await send({ type: "archive_session", session_id: sessionId });
  }

  async function handleDelete() {
    await send({ type: "delete_session", session_id: sessionId });
  }

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          className="inline-flex h-5 w-5 shrink-0 items-center justify-center rounded-md text-muted-foreground opacity-0 transition-opacity hover:text-foreground group-hover/thread:opacity-100"
          onClick={(e) => e.stopPropagation()}
        >
          <Ellipsis className="h-3.5 w-3.5" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="min-w-36">
        <DropdownMenuItem onClick={handleArchive}>
          <Archive className="mr-2 h-3.5 w-3.5" />
          Archive
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem variant="destructive" onClick={handleDelete}>
          <Trash2 className="mr-2 h-3.5 w-3.5" />
          Delete
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
