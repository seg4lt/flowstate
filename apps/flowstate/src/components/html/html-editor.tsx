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
 *     `<iframe sandbox="allow-same-origin" srcdoc=…>` with a `<base>`
 *     element injected so relative URLs resolve against the project
 *     directory via Tauri's asset protocol. Buffer is mirrored via
 *     the `onChange` prop on `<CodeEditor>` so the preview reflects
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
 * Why this sandbox configuration:
 *
 *   `sandbox="allow-same-origin"` (and nothing else) is the smallest
 *   relaxation that lets relative `<link>`, `<img>`, and `<a>` work
 *   while still keeping every other attack vector closed:
 *
 *     - NO `allow-scripts`     → no JS executes in the preview, so
 *                                 there is no runtime to weaponise
 *                                 the same-origin grant.
 *     - NO `allow-forms`       → form submissions blocked.
 *     - NO `allow-top-navigation`
 *                              → links can navigate the iframe but
 *                                 cannot replace the host app.
 *     - NO `allow-popups`      → `target="_blank"` is inert.
 *     - NO `allow-modals`      → `print()` etc. blocked (also moot
 *                                 without scripts).
 *
 *   With `allow-same-origin`, the srcdoc inherits the parent's origin
 *   for the initial document, but the moment the user clicks a
 *   relative link the iframe navigates to `asset://localhost/…` and
 *   the new doc takes that origin. In neither case does the iframe
 *   gain capability beyond "load resources whose URL it already
 *   knows" — exactly what the user wants for previewing their own
 *   project files. Without `allow-scripts` there is no CSS-exfil /
 *   storage-read / IPC-call path even in the worst case.
 *
 *   The asset protocol scope in `tauri.conf.json` is `**`, so the
 *   webview will serve any file the OS lets the app read. We rely on
 *   `<base href>` to anchor relative URLs at the project root rather
 *   than trying to enforce containment in the resolver — author HTML
 *   could escape with `../`, but the author IS the user previewing
 *   their own file. There is no adversarial content in this flow.
 *
 *   Things still NOT handled (and would require more work):
 *     - Untrusted HTML. If we ever preview content the user didn't
 *       author (e.g., AI-generated, downloaded), the lack of an HTML
 *       sanitizer in front of `srcdoc` becomes a real problem and
 *       the sandbox would need to drop back to `""` plus a sanitizer
 *       (DOMPurify with `FORBID_TAGS: ["script","style","link"]` etc.).
 *     - Cross-tree linking. `<a href="../../other/proj.html">` does
 *       work because asset scope is `**`, but it's not constrained
 *       to the project either.
 */

import * as React from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
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

/**
 * Compute the absolute filesystem directory of `filePath` within
 * `projectPath`, normalised with a trailing slash so it can be
 * concatenated with relative URLs in `<base href>`.
 *
 * Returns `null` if we can't safely build a base — caller should
 * render the HTML untouched, which preserves the pre-fix behaviour
 * (relative URLs won't resolve, but nothing breaks).
 */
function projectBaseHref(
  projectPath: string | null,
  filePath: string,
): string | null {
  if (!projectPath) return null;
  const dir = filePath.includes("/")
    ? filePath.slice(0, filePath.lastIndexOf("/"))
    : "";
  // Tauri's `convertFileSrc` is what `assetUrl()` wraps in the
  // markdown editor; importing it directly here keeps this file
  // independent of the markdown-editor module graph.
  const absDir = dir ? `${projectPath}/${dir}` : projectPath;
  // `convertFileSrc` returns something like
  // `asset://localhost/<percent-encoded-abs-path>` (or the
  // `https://asset.localhost/...` flavour on Windows). Either way
  // we just need a trailing slash so a `<base href>` resolves
  // relative URLs as if they sat next to the source file.
  const base = convertFileSrc(absDir);
  return base.endsWith("/") ? base : `${base}/`;
}

/**
 * Inject `<base href="…">` so relative URLs in the previewed HTML
 * resolve against the source file's directory in the project. We
 * splice into `<head>` if one exists, otherwise prepend a fresh
 * `<head>` before `<html>`'s body (HTML parsers are lenient — a
 * stray `<base>` with no surrounding `<head>` still works, but
 * matching the well-formed shape avoids quirks-mode surprises).
 *
 * If the document already declares a `<base href>` we leave it alone
 * — author intent wins. (They'll just have to write absolute or
 * project-rooted URLs themselves in that case.)
 */
function withProjectBase(
  html: string,
  projectPath: string | null,
  filePath: string,
): string {
  const baseHref = projectBaseHref(projectPath, filePath);
  if (!baseHref) return html;
  // Cheap pre-check: if the author already wrote a <base>, bail.
  // Using a non-greedy regex over the first ~kilobyte is enough —
  // <base> is supposed to live in <head>, so it shows up early or
  // not at all.
  if (/<base\b[^>]*\bhref\s*=/i.test(html.slice(0, 4096))) return html;

  const tag = `<base href="${escapeAttr(baseHref)}">`;

  // Prefer to splice immediately after the opening <head> so the
  // base is in scope for every subsequent <link>/<script>/<a>.
  const headOpen = /<head\b[^>]*>/i.exec(html);
  if (headOpen) {
    const insertAt = headOpen.index + headOpen[0].length;
    return html.slice(0, insertAt) + tag + html.slice(insertAt);
  }

  // No <head>. Try after <html …>; if even that's missing, prepend.
  const htmlOpen = /<html\b[^>]*>/i.exec(html);
  if (htmlOpen) {
    const insertAt = htmlOpen.index + htmlOpen[0].length;
    return html.slice(0, insertAt) + `<head>${tag}</head>` + html.slice(insertAt);
  }
  return `<!DOCTYPE html><html><head>${tag}</head><body>${html}</body></html>`;
}

/** Escape a string for use inside a double-quoted HTML attribute. */
function escapeAttr(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/"/g, "&quot;");
}

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

  // Project-base injection is pure / synchronous — no IPC, no async
  // races to worry about. `useMemo` keeps the iframe stable across
  // unrelated re-renders so it doesn't reload on every keystroke
  // when the preview isn't visible.
  const renderedContent = React.useMemo(
    () => withProjectBase(previewContent, projectPath, path),
    [previewContent, projectPath, path],
  );

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
          Sandboxed: scripts, forms, and top-frame navigation are disabled.
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
          // `allow-same-origin` is the smallest relaxation that lets
          // relative <link>/<img>/<a> resolve through the asset
          // protocol. NO `allow-scripts` — see file header for the
          // full security rationale.
          sandbox="allow-same-origin"
          srcDoc={renderedContent}
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
