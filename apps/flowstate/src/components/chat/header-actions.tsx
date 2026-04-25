import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import {
  Check,
  ChevronDown,
  Compass,
  Diff,
  ExternalLink,
  FolderOpen,
  Pin,
  PinOff,
  Search,
  Terminal,
} from "lucide-react";
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
import { selectDockOpen, useTerminal } from "@/stores/terminal-store";
import {
  isPopoutWindow,
  popoutThread,
  POPOUT_PIN_CHANGED_EVENT,
  readPopoutPinPref,
  setPopoutPinned,
} from "@/lib/popout";
import {
  LAUNCH_DEFAULT_EDITOR_EVENT,
  OPEN_EDITOR_PICKER_EVENT,
} from "@/lib/keyboard-shortcuts";

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
  contextOpen: boolean;
  onToggleContext: () => void;
  // Live `N of M` badge shown on the context button whenever the
  // session's latest main-agent TodoWrite has at least one item.
  // Null hides the badge entirely.
  todoProgress: { completed: number; total: number } | null;
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

const DEFAULT_EDITOR_KEY = "flowstate:default-editor";

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
  contextOpen,
  onToggleContext,
  todoProgress,
}: HeaderActionsProps) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  // Terminal store is a global context — read it directly rather
  // than prop-drilling dockOpen through ChatView. The toggle button
  // below dispatches the same `toggle_dock` action the Cmd+J
  // shortcut (src/router.tsx) uses, so both entry points stay in
  // sync via `selectDockOpen`.
  const { state: terminalState, dispatch: terminalDispatch } = useTerminal();
  const dockOpen = selectDockOpen(terminalState, sessionId);

  // Thread popout: "Pop out" button only shown in the main window;
  // a Pin toggle is only shown when we *are* the popout. The two
  // branches never coexist, so the header row never doubles in
  // width. Decide once at mount — a window's role (main vs popout)
  // never flips after creation.
  const inPopout = React.useMemo(() => isPopoutWindow(), []);
  const [pinned, setPinned] = React.useState<boolean>(() =>
    readPopoutPinPref(),
  );
  const handlePopout = React.useCallback(async () => {
    try {
      await popoutThread(sessionId);
    } catch (err) {
      toast({
        description: `Could not pop out thread: ${String(err)}`,
        duration: 4000,
      });
    }
  }, [sessionId]);
  const handleTogglePin = React.useCallback(async () => {
    const next = !pinned;
    setPinned(next);
    try {
      await setPopoutPinned(next);
    } catch (err) {
      // Roll back the optimistic flip if the OS rejected the
      // request — keeps the toggle's visual state honest.
      setPinned(pinned);
      toast({
        description: `Could not ${next ? "pin" : "unpin"} window: ${String(err)}`,
        duration: 4000,
      });
    }
  }, [pinned]);
  // Keep the local `pinned` state in sync when the global ⌘⇧T
  // shortcut flips the preference out from under us. setPopoutPinned
  // dispatches POPOUT_PIN_CHANGED_EVENT after the localStorage write
  // so the header button visually flips at the same instant the OS
  // applies the always-on-top change.
  React.useEffect(() => {
    function onPinChanged(e: Event) {
      const detail = (e as CustomEvent<{ enabled: boolean }>).detail;
      if (detail && typeof detail.enabled === "boolean") {
        setPinned(detail.enabled);
      } else {
        // Defensive — re-read the source of truth if the event
        // somehow arrived without a payload.
        setPinned(readPopoutPinPref());
      }
    }
    window.addEventListener(POPOUT_PIN_CHANGED_EVENT, onPinChanged);
    return () =>
      window.removeEventListener(POPOUT_PIN_CHANGED_EVENT, onPinChanged);
  }, []);

  // Controlled editor DropdownMenu state. The mouse path opens it via
  // the trigger button (Radix's default behavior); the ⌘⇧O global
  // shortcut opens it by dispatching OPEN_EDITOR_PICKER_EVENT below.
  // Once open, Radix handles arrow-key navigation between
  // DropdownMenuItems out of the box; Enter activates.
  const [editorMenuOpen, setEditorMenuOpen] = React.useState(false);
  React.useEffect(() => {
    function onOpenPicker() {
      setEditorMenuOpen(true);
    }
    window.addEventListener(OPEN_EDITOR_PICKER_EVENT, onOpenPicker);
    return () =>
      window.removeEventListener(OPEN_EDITOR_PICKER_EVENT, onOpenPicker);
  }, []);
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

  // ⌘O — open the project in the current default editor.
  // The keystroke itself is owned by the global shortcut registry
  // (`launch-default-editor`, see lib/keyboard/registry.tsx) which
  // dispatches LAUNCH_DEFAULT_EDITOR_EVENT here. The registry has
  // `fireInTextInputs: true` so this works even while the chat
  // composer is focused — without that flag the browser's built-in
  // "Open File…" dialog would hijack ⌘O. Falls back to a toast when
  // no default has been picked yet.
  React.useEffect(() => {
    function onLaunch() {
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
    window.addEventListener(LAUNCH_DEFAULT_EDITOR_EVENT, onLaunch);
    return () =>
      window.removeEventListener(LAUNCH_DEFAULT_EDITOR_EVENT, onLaunch);
  }, [defaultEditor, launchEditor]);

  return (
    <div className="flex items-center gap-1">
      <Button
        variant={contextOpen ? "secondary" : "outline"}
        size="xs"
        onClick={onToggleContext}
        aria-pressed={contextOpen}
        title={
          contextOpen
            ? "Hide agent context"
            : "Show agent context (plan + todos)"
        }
      >
        <Compass className="h-3 w-3" />
        {todoProgress && (
          <span className="tabular-nums text-muted-foreground">
            {todoProgress.completed} of {todoProgress.total}
          </span>
        )}
      </Button>
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
        {hasChanges && (
          <>
            <span className="tabular-nums text-green-600 dark:text-green-400">
              +{additions}
            </span>
            <span className="tabular-nums text-red-600 dark:text-red-400">
              −{deletions}
            </span>
          </>
        )}
      </Button>
      <Button
        variant={dockOpen ? "secondary" : "outline"}
        size="xs"
        onClick={() =>
          terminalDispatch({ type: "toggle_dock", sessionId })
        }
        aria-pressed={dockOpen}
        title={
          dockOpen
            ? "Hide integrated terminal (⌘J)"
            : "Open integrated terminal (⌘J)"
        }
      >
        <Terminal className="h-3 w-3" />
      </Button>
      {inPopout ? (
        <Button
          variant={pinned ? "secondary" : "outline"}
          size="xs"
          onClick={handleTogglePin}
          aria-pressed={pinned}
          title={
            pinned
              ? "Unpin — window returns to normal z-order"
              : "Pin — keep this window above other apps"
          }
        >
          {pinned ? (
            <PinOff className="h-3 w-3" />
          ) : (
            <Pin className="h-3 w-3" />
          )}
        </Button>
      ) : (
        <Button
          variant="outline"
          size="xs"
          onClick={handlePopout}
          title="Pop out this thread into its own window"
        >
          <ExternalLink className="h-3 w-3" />
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
      </Button>
      <DropdownMenu open={editorMenuOpen} onOpenChange={setEditorMenuOpen}>
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
                ? `Open project in ${defaultEditor.label}  (Cmd/Ctrl+O · ⇧ for picker)`
                : "Pick an editor to open the project in (Cmd/Ctrl+Shift+O)"
            }
            className="inline-flex h-6 shrink-0 items-center gap-1 rounded-[min(var(--radius-md),10px)] border border-border bg-background px-2 text-xs font-medium hover:bg-muted hover:text-foreground dark:border-input dark:bg-input/30 dark:hover:bg-input/50"
          >
            <FolderOpen className="h-3 w-3" />
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
