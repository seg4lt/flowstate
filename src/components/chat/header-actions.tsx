import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { Check, ChevronDown, Diff, FolderOpen, Search } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { openInEditor } from "@/lib/api";
import { prefetchProjectFiles } from "@/lib/queries";
import { toast } from "@/hooks/use-toast";
import type { AggregatedFileDiff } from "@/lib/session-diff";

interface HeaderActionsProps {
  sessionId: string;
  projectPath: string | null;
  diffs: AggregatedFileDiff[];
  diffOpen: boolean;
  onToggleDiff: () => void;
  // Fired on Diff button hover/focus so ChatView can pre-warm the
  // git diff summary before the click lands. Optional because the
  // throttling lives upstream — this component doesn't care whether
  // the callback is idempotent or not.
  onHoverDiff?: () => void;
}

// Known editors we offer in the Open dropdown. Each `command` is
// the CLI launcher the editor ships (or asks you to enable from
// its command palette) which accepts a directory positional arg
// and opens it as a project. If a user picks one whose CLI isn't
// installed the rust side returns Err and we toast the message.
interface EditorChoice {
  id: string;
  label: string;
  command: string;
}

const KNOWN_EDITORS: EditorChoice[] = [
  { id: "zed", label: "Zed", command: "zed" },
  { id: "vscode", label: "VS Code", command: "code" },
  { id: "idea", label: "IntelliJ IDEA", command: "idea" },
];

const DEFAULT_EDITOR_KEY = "flowzen:default-editor";

function loadDefaultEditorId(): string | null {
  try {
    return window.localStorage.getItem(DEFAULT_EDITOR_KEY);
  } catch {
    return null;
  }
}

function saveDefaultEditorId(id: string): void {
  try {
    window.localStorage.setItem(DEFAULT_EDITOR_KEY, id);
  } catch {
    /* storage may be unavailable */
  }
}

export function HeaderActions({
  sessionId,
  projectPath,
  diffs,
  diffOpen,
  onToggleDiff,
  onHoverDiff,
}: HeaderActionsProps) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  // Hover-driven prefetch for the /code view's file list. Wired to
  // both onMouseEnter and onFocus on the Search button so mouse and
  // keyboard users get the same head-start: by the time the click
  // fires, the rust-side walk is already in flight (or done) and
  // CodeView mounts straight onto cached data. Throttling lives
  // inside prefetchProjectFiles itself, so this can be called as
  // many times per second as the browser fires events.
  const handleSearchPrefetch = React.useCallback(() => {
    prefetchProjectFiles(queryClient, projectPath);
  }, [queryClient, projectPath]);
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

  // Persisted default editor id. Picking from the dropdown both
  // launches the editor AND sets it as the default, so the next
  // Cmd+O hits go straight through without re-prompting.
  const [defaultEditorId, setDefaultEditorId] = React.useState<string | null>(
    () => loadDefaultEditorId(),
  );
  const defaultEditor = React.useMemo<EditorChoice | null>(() => {
    if (!defaultEditorId) return null;
    return KNOWN_EDITORS.find((e) => e.id === defaultEditorId) ?? null;
  }, [defaultEditorId]);

  const launchEditor = React.useCallback(
    async (editor: EditorChoice) => {
      if (!projectPath) {
        toast({
          description: "This session has no project path to open.",
          duration: 3000,
        });
        return;
      }
      try {
        await openInEditor(editor.command, projectPath);
      } catch (err) {
        toast({
          description: `Could not launch ${editor.label}: ${String(err)}`,
          duration: 4000,
        });
      }
    },
    [projectPath],
  );

  const handlePickEditor = React.useCallback(
    (editor: EditorChoice) => {
      setDefaultEditorId(editor.id);
      saveDefaultEditorId(editor.id);
      void launchEditor(editor);
    },
    [launchEditor],
  );

  // Cmd/Ctrl+O — open the project in the current default editor.
  // Skips when the user is typing in an input/textarea so it
  // doesn't fight any in-textbox shortcut. Falls back to a toast
  // when no default has been picked yet.
  React.useEffect(() => {
    function isInTextInput(target: EventTarget | null): boolean {
      if (!(target instanceof HTMLElement)) return false;
      const tag = target.tagName;
      return (
        tag === "INPUT" ||
        tag === "TEXTAREA" ||
        target.isContentEditable === true
      );
    }
    function onKeyDown(e: KeyboardEvent) {
      const mod = e.metaKey || e.ctrlKey;
      if (!mod || e.shiftKey || e.altKey) return;
      if (e.key.toLowerCase() !== "o") return;
      if (isInTextInput(e.target)) return;
      e.preventDefault();
      if (!defaultEditor) {
        toast({
          description:
            "Pick a default editor from the Open menu first, then Cmd+O will use it.",
          duration: 4000,
        });
        return;
      }
      void launchEditor(defaultEditor);
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [defaultEditor, launchEditor]);

  return (
    <div className="flex items-center gap-1">
      {hasChanges && (
        <Button
          variant={diffOpen ? "secondary" : "outline"}
          size="xs"
          onClick={onToggleDiff}
          onMouseEnter={onHoverDiff}
          onFocus={onHoverDiff}
          aria-pressed={diffOpen}
          title={
            diffOpen ? "Hide diff panel" : "Show diff panel for this session"
          }
        >
          <Diff className="h-3 w-3" />
          <span className="tabular-nums text-green-600 dark:text-green-400">
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
        onClick={() =>
          navigate({ to: "/code/$sessionId", params: { sessionId } })
        }
        onMouseEnter={handleSearchPrefetch}
        onFocus={handleSearchPrefetch}
        title="Search project files"
      >
        <Search className="h-3 w-3" />
        Search
      </Button>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          {/* Native <button> (not the shadcn Button) — matches the
              working DropdownMenu pattern in model-selector.tsx /
              thread-actions.tsx. The shadcn Button + asChild Slot
              composition wasn't forwarding the trigger props in
              this codebase, so the menu wouldn't open. */}
          <button
            type="button"
            title={
              defaultEditor
                ? `Open project in ${defaultEditor.label}  (Cmd/Ctrl+O)`
                : "Pick an editor to open the project in"
            }
            className="inline-flex h-6 shrink-0 items-center gap-1 rounded-[min(var(--radius-md),10px)] border border-border bg-background px-2 text-xs font-medium hover:bg-muted hover:text-foreground dark:border-input dark:bg-input/30 dark:hover:bg-input/50"
          >
            <FolderOpen className="h-3 w-3" />
            Open
            <ChevronDown className="h-3 w-3" />
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="min-w-[160px]">
          {KNOWN_EDITORS.map((editor) => {
            const isDefault = defaultEditorId === editor.id;
            return (
              <DropdownMenuItem
                key={editor.id}
                onClick={() => handlePickEditor(editor)}
                className="flex items-center justify-between gap-2"
              >
                <span>{editor.label}</span>
                {isDefault && (
                  <Check className="h-3 w-3 text-muted-foreground" />
                )}
              </DropdownMenuItem>
            );
          })}
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}
