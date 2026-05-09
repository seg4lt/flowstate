/**
 * Markdown live-preview editor.
 *
 * Sits next to `CodeEditor` (raw code) — both host CodeMirror 6
 * `EditorView`s, both share the vim/save plumbing in `cm-shared`, but
 * the extension stack differs:
 *
 *   - `@codemirror/lang-markdown` parses the document and lazy-loads
 *     fenced-code-block parsers via `@codemirror/language-data`.
 *   - The `livePreview()` extension layers the cursor-toggled raw /
 *     rendered styling, image widgets, mermaid widget, link click
 *     handler, and link-path autocomplete.
 *   - `Prec.high(syntaxHighlighting(markdownHighlightStyle))` ensures
 *     the markdown body colours win over any base theme highlight.
 *
 * Deliberately no Shiki, no git-diff overlay, no comment extension —
 * the live-preview decorations need to be the only authority on
 * what's drawn over the markdown source.
 */

import * as React from "react";
import {
  Compartment,
  EditorState,
  Prec,
  type Extension,
} from "@codemirror/state";
import {
  EditorView,
  crosshairCursor,
  drawSelection,
  dropCursor,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  keymap,
  lineNumbers,
  rectangularSelection,
  scrollPastEnd,
} from "@codemirror/view";
import {
  bracketMatching,
  codeFolding,
  foldGutter,
  foldKeymap,
  indentOnInput,
  indentUnit,
  syntaxHighlighting,
} from "@codemirror/language";
import {
  defaultKeymap,
  history,
  historyKeymap,
} from "@codemirror/commands";
import {
  highlightSelectionMatches,
  search,
  searchKeymap,
} from "@codemirror/search";
import { closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
import { Vim, vim } from "@replit/codemirror-vim";
import { markdown } from "@codemirror/lang-markdown";
import { languages } from "@codemirror/language-data";
import {
  buildExtraKeymap,
  ensureClipboardSyncRegistered,
  ensureVimWriteRegistered,
  saveHandlers,
} from "@/components/code/cm-shared";
import { dirname, joinPath } from "@/lib/paths";
import { livePreview } from "./lib/live-preview";
import { markdownHighlightStyle } from "./lib/markdown-highlight";
import { imagePasteHandler } from "./lib/image-paste";

// Reuse the same chrome-only theme the code editor uses so headings,
// gutter, panels, and search panel all match.
import { buildEditorTheme } from "@/components/code/editor-theme";

export interface MarkdownEditorProps {
  /** Project-relative path of the open file. Drives `getDocDir`
   *  resolution and is used as the React key in the parent. */
  path: string;
  /** Absolute project root. Required so relative `![…](…)` paths can
   *  resolve to a webview-loadable asset URL. Allowed to be `null`
   *  when no project is open (image widgets just show fallbacks). */
  projectPath: string | null;
  /** File contents at mount. Subsequent prop updates are ignored
   *  unless `path` also changes (which forces a remount via React
   *  key in the parent). */
  initialContent: string;
  theme: "light" | "dark";
  vimEnabled: boolean;
  readOnly?: boolean;
  /** Project file index (relative paths). Used by the link-path
   *  autocomplete; passed in from the host so the markdown editor
   *  doesn't reach into React Query directly. */
  projectFiles: readonly string[];
  /** Called on save (Cmd+S, Vim `:w`, blur auto-save). */
  onSave: (contents: string) => Promise<void>;
  /** Called whenever the editor's dirty state flips. */
  onDirtyChange: (dirty: boolean) => void;
  /** Called on `Mod+click` of a `[label](url)` link. The host decides
   *  whether to open the URL externally (http/https) or as a tab
   *  (relative `.md`). */
  onLinkOpen: (url: string) => void;
  /** Optional: fired after a clipboard image has been written next to
   *  the open document. The host invalidates the file-tree query so
   *  the new image shows up in the sidebar. */
  onImageSaved?: (relPath: string) => void;
}

export function MarkdownEditor({
  path,
  projectPath,
  initialContent,
  theme,
  vimEnabled,
  readOnly = false,
  projectFiles,
  onSave,
  onDirtyChange,
  onLinkOpen,
  onImageSaved,
}: MarkdownEditorProps): React.ReactElement {
  const containerRef = React.useRef<HTMLDivElement | null>(null);
  const viewRef = React.useRef<EditorView | null>(null);
  const onSaveRef = React.useRef<() => Promise<void>>(() => Promise.resolve());
  const onDirtyChangeRef = React.useRef(onDirtyChange);
  const savedContentRef = React.useRef<string>(initialContent);
  const isDirtyRef = React.useRef<boolean>(false);
  const inImeRef = React.useRef<boolean>(false);

  // Closure-stable accessors for state the long-lived extensions need.
  const projectPathRef = React.useRef(projectPath);
  const projectFilesRef = React.useRef(projectFiles);
  const onLinkOpenRef = React.useRef(onLinkOpen);
  const onImageSavedRef = React.useRef(onImageSaved);
  const themeRef = React.useRef(theme);
  React.useEffect(() => {
    projectPathRef.current = projectPath;
    projectFilesRef.current = projectFiles;
    onLinkOpenRef.current = onLinkOpen;
    onImageSavedRef.current = onImageSaved;
    themeRef.current = theme;
  }, [projectPath, projectFiles, onLinkOpen, onImageSaved, theme]);

  const vimCompartmentRef = React.useRef(new Compartment());
  const themeCompartmentRef = React.useRef(new Compartment());
  const readOnlyCompartmentRef = React.useRef(new Compartment());

  // Save callback ref kept fresh so the (long-lived) keymap can
  // always see the latest `onSave`. Same pattern code-editor uses.
  React.useEffect(() => {
    onDirtyChangeRef.current = onDirtyChange;
  }, [onDirtyChange]);

  React.useEffect(() => {
    onSaveRef.current = async () => {
      const view = viewRef.current;
      if (!view) return;
      const current = view.state.doc.toString();
      if (current === savedContentRef.current) return;
      try {
        await onSave(current);
        savedContentRef.current = current;
        const stillDirty = view.state.doc.toString() !== current;
        if (isDirtyRef.current !== stillDirty) {
          isDirtyRef.current = stillDirty;
          onDirtyChangeRef.current(stillDirty);
        }
      } catch (err) {
        throw err;
      }
    };
  }, [onSave]);

  // Compute the absolute directory of the open document — markdown
  // image references resolve relative to it.
  const docDir = React.useMemo(() => {
    if (!projectPath) return "";
    const rel = dirname(path);
    return rel ? joinPath(projectPath, rel) : projectPath;
  }, [projectPath, path]);
  const docDirRef = React.useRef(docDir);
  React.useEffect(() => {
    docDirRef.current = docDir;
  }, [docDir]);

  // Mount EditorView. Path change tears down + remounts.
  React.useEffect(() => {
    if (!containerRef.current) return;
    savedContentRef.current = initialContent;
    isDirtyRef.current = false;
    onDirtyChangeRef.current(false);
    inImeRef.current = false;

    const updateListener = EditorView.updateListener.of((u) => {
      if (!u.docChanged) return;
      const dirty = u.state.doc.toString() !== savedContentRef.current;
      if (dirty !== isDirtyRef.current) {
        isDirtyRef.current = dirty;
        onDirtyChangeRef.current(dirty);
      }
    });

    const blurAutoSave = EditorView.domEventHandlers({
      blur: () => {
        if (!isDirtyRef.current || inImeRef.current) return false;
        void onSaveRef.current().catch(() => {});
        return false;
      },
      compositionstart: () => {
        inImeRef.current = true;
        return false;
      },
      compositionend: () => {
        inImeRef.current = false;
        return false;
      },
    });

    const extensions: Extension[] = [
      EditorState.allowMultipleSelections.of(true),
      EditorState.tabSize.of(2),
      indentUnit.of("  "),
      vimCompartmentRef.current.of(vimEnabled ? vim({ status: true }) : []),
      lineNumbers(),
      highlightActiveLineGutter(),
      foldGutter(),
      codeFolding(),
      highlightActiveLine(),
      highlightSelectionMatches(),
      highlightSpecialChars(),
      drawSelection(),
      dropCursor(),
      rectangularSelection(),
      crosshairCursor(),
      bracketMatching(),
      closeBrackets(),
      indentOnInput(),
      history(),
      // `addKeymap: false` keeps lang-markdown's own bindings out of
      // the way — `defaultKeymap` covers the basics and we don't want
      // markdown-specific shortcuts shadowing the user's vim mode.
      markdown({ addKeymap: false, codeLanguages: languages }),
      livePreview({
        getDocDir: () => docDirRef.current,
        getProjectPath: () => projectPathRef.current ?? "",
        getFiles: () => projectFilesRef.current as string[],
        getTheme: () => themeRef.current,
        onLinkOpen: (url) => onLinkOpenRef.current(url),
      }),
      imagePasteHandler({
        getProjectPath: () => projectPathRef.current,
        getDocDir: () => docDirRef.current,
        getDocPath: () => path,
        onImageSaved: (rel) => onImageSavedRef.current?.(rel),
      }),
      // Live-preview's markdown highlight wins over the editor theme.
      Prec.high(syntaxHighlighting(markdownHighlightStyle)),
      keymap.of([
        ...closeBracketsKeymap,
        ...defaultKeymap,
        ...historyKeymap,
        ...searchKeymap,
        ...foldKeymap,
        ...buildExtraKeymap(onSaveRef),
      ]),
      search({ top: true }),
      scrollPastEnd(),
      blurAutoSave,
      updateListener,
      readOnlyCompartmentRef.current.of(EditorState.readOnly.of(readOnly)),
      EditorView.lineWrapping,
      themeCompartmentRef.current.of([buildEditorTheme(theme)]),
    ];

    const state = EditorState.create({ doc: initialContent, extensions });
    const view = new EditorView({ state, parent: containerRef.current });
    viewRef.current = view;
    saveHandlers.set(view, () => onSaveRef.current());
    ensureVimWriteRegistered();
    ensureClipboardSyncRegistered();

    return () => {
      saveHandlers.delete(view);
      view.destroy();
      if (viewRef.current === view) viewRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path]);

  // Vim toggle reactivity.
  React.useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    if (!vimEnabled) {
      try {
        Vim.exitInsertMode(
          view as unknown as Parameters<typeof Vim.exitInsertMode>[0],
        );
      } catch {
        /* not in insert mode — ignore */
      }
    }
    view.dispatch({
      effects: vimCompartmentRef.current.reconfigure(
        vimEnabled ? vim({ status: true }) : [],
      ),
    });
  }, [vimEnabled]);

  // Theme reactivity. Reconfigure the chrome theme; the live-preview
  // plugin already reads `themeRef.current` on every update, so the
  // excalidraw widget re-themes automatically once a doc/selection
  // change kicks the view-plugin.
  React.useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    view.dispatch({
      effects: themeCompartmentRef.current.reconfigure([buildEditorTheme(theme)]),
    });
    // Force a no-op state update so the view-plugin's `update`
    // recomputes decorations against the new theme — without this
    // the embedded excalidraw widget keeps the old palette until
    // the user moves the cursor.
    view.dispatch({});
  }, [theme]);

  // Read-only reactivity.
  React.useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    view.dispatch({
      effects: readOnlyCompartmentRef.current.reconfigure(
        EditorState.readOnly.of(readOnly),
      ),
    });
  }, [readOnly]);

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-1 flex-col">
      <div
        ref={containerRef}
        className="min-h-0 min-w-0 flex-1 overflow-hidden"
      />
    </div>
  );
}

export default MarkdownEditor;
