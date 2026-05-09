/**
 * HTML viewer — code editor with a toggle to a sandboxed preview.
 *
 * Sits next to `MarkdownEditor` and `CodeEditor` in the dispatch
 * ladder of `code-view.tsx`. For `.html` / `.htm` files we want the
 * usual editing experience plus a one-click rendered preview, without
 * mounting a parallel CM6 stack.
 *
 * Implementation:
 *   - "Code" mode: reuses `<CodeEditor>` (Shiki HTML grammar via the
 *     existing `EXT_TO_LANG.htm = "html"` mapping). All editor
 *     features — vim, save, git mode, comments — come along for free.
 *   - "Preview" mode: renders the current buffer in an
 *     `<iframe sandbox="" srcdoc=…>`. Buffer is mirrored via the
 *     `onChange` prop on `<CodeEditor>` so the preview reflects
 *     unsaved edits.
 *
 *   ┌─────────────────────────────────────────────┐
 *   │ [Code] [Preview]                            │  toggle bar
 *   ├─────────────────────────────────────────────┤
 *   │                                             │
 *   │   <CodeEditor> | <iframe srcdoc=…>          │
 *   │                                             │
 *   └─────────────────────────────────────────────┘
 *
 * Security: `sandbox=""` (empty token list) is the strictest setting.
 * No scripts, no forms, no popups, no top-level navigation; the
 * document gets a unique opaque origin so it can't reach parent
 * storage. `srcdoc` keeps the HTML in-memory so no asset-protocol
 * handles or `file://` URIs leak. Combined with sandbox the iframe
 * has no network access either, so external `<img>`, `<link>`,
 * remote `<script>`, etc. won't load — that's the intended trade-off
 * for "strict" mode.
 *
 * DO NOT loosen the sandbox without re-evaluating: there's no HTML
 * sanitizer in front of the iframe, the sandbox IS the defence.
 */

import * as React from "react";
import { CodeEditor } from "@/components/code/code-editor";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

export interface HtmlEditorProps {
  /** Project-relative path. Used as React key — switching paths
   *  remounts the editor with the new file's content. */
  path: string;
  /** File contents at mount. After mount the EditorState owns the
   *  text — `initialContent` changes are ignored unless `path`
   *  also changes (which forces a remount). */
  initialContent: string;
  /** Resolved app theme. Forwarded to the inner `<CodeEditor>`. */
  theme: "light" | "dark";
  /** Whether vim mode is active. Forwarded. */
  vimEnabled: boolean;
  /** Whether git diff mode is on. Forwarded. */
  gitModeEnabled?: boolean;
  /** Project root for git-diff fetch + asset resolution. Forwarded. */
  projectPath?: string | null;
  /** Active chat session id for the line-comment affordance.
   *  Forwarded. */
  sessionId?: string | null;
  /** Save callback. Forwarded. */
  onSave: (contents: string) => Promise<void>;
  /** Dirty-state callback. Forwarded. */
  onDirtyChange: (dirty: boolean) => void;
}

type Mode = "code" | "preview";

export function HtmlEditor({
  path,
  initialContent,
  theme,
  vimEnabled,
  gitModeEnabled = false,
  projectPath = null,
  sessionId = null,
  onSave,
  onDirtyChange,
}: HtmlEditorProps): React.ReactElement {
  const [mode, setMode] = React.useState<Mode>("code");

  // Live mirror of the editor buffer. Seeded with `initialContent`
  // and updated on every doc change via `<CodeEditor>`'s `onChange`.
  // Held in a ref so `<CodeEditor>` re-renders aren't forced on every
  // keystroke; the preview iframe reads from state instead, populated
  // lazily when the user toggles into preview mode.
  const liveContentRef = React.useRef<string>(initialContent);
  const [previewContent, setPreviewContent] =
    React.useState<string>(initialContent);

  // Reset the mirror when the open file changes (path key remounts
  // the inner CodeEditor anyway, but we own this state).
  React.useEffect(() => {
    liveContentRef.current = initialContent;
    setPreviewContent(initialContent);
  }, [path, initialContent]);

  // Track the active mode in a ref so the (stable) `onChange`
  // handler can decide whether to push edits into preview state
  // without changing identity on every mode flip.
  const modeRef = React.useRef<Mode>(mode);
  React.useEffect(() => {
    modeRef.current = mode;
  }, [mode]);

  const handleChange = React.useCallback((contents: string) => {
    liveContentRef.current = contents;
    // Only push keystroke-rate setState when the preview is actually
    // visible. In code mode the iframe is hidden, so we skip the
    // re-render and let `showPreview` snapshot the ref on toggle.
    if (modeRef.current === "preview") setPreviewContent(contents);
  }, []);

  const showPreview = React.useCallback(() => {
    setPreviewContent(liveContentRef.current);
    setMode("preview");
  }, []);

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-1 flex-col">
      <div className="flex shrink-0 items-center gap-1 border-b border-border bg-background px-2 py-1">
        <ToggleButton
          active={mode === "code"}
          onClick={() => setMode("code")}
          label="Code"
        />
        <ToggleButton
          active={mode === "preview"}
          onClick={showPreview}
          label="Preview"
        />
        <span className="ml-2 text-[10px] text-muted-foreground">
          Sandboxed: scripts, forms, and network are disabled.
        </span>
      </div>
      {/*
        Both panes are mounted at all times; we toggle visibility
        with `hidden` rather than conditionally rendering. That keeps
        the CodeMirror EditorView alive across toggles (preserving
        cursor, scroll, undo) and lets us push live edits into the
        preview iframe via the `onChange` handler below.
      */}
      <div className={cn("min-h-0 min-w-0 flex-1", mode === "code" ? "flex flex-col" : "hidden")}>
        <CodeEditor
          path={path}
          initialContent={initialContent}
          theme={theme}
          vimEnabled={vimEnabled}
          gitModeEnabled={gitModeEnabled}
          projectPath={projectPath}
          sessionId={sessionId}
          onSave={onSave}
          onDirtyChange={onDirtyChange}
          onChange={handleChange}
        />
      </div>
      <div className={cn("min-h-0 min-w-0 flex-1", mode === "preview" ? "flex flex-col" : "hidden")}>
        <iframe
          // Empty sandbox = strictest. Do not loosen without a
          // sanitizer in front of `srcdoc` — see file header.
          sandbox=""
          srcDoc={previewContent}
          title={`Preview: ${path}`}
          className="h-full w-full flex-1 border-0 bg-white"
        />
      </div>
    </div>
  );
}

function ToggleButton({
  active,
  onClick,
  label,
}: {
  active: boolean;
  onClick: () => void;
  label: string;
}): React.ReactElement {
  return (
    <Button
      type="button"
      size="xs"
      variant={active ? "secondary" : "ghost"}
      onClick={onClick}
      aria-pressed={active}
    >
      {label}
    </Button>
  );
}

export default HtmlEditor;
