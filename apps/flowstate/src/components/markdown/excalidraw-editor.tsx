/**
 * Excalidraw drawing pane.
 *
 * Mounted by `code-view.tsx` instead of the markdown editor when the
 * active tab's path matches `*.excalidraw.svg` / `*.excalidraw.png`.
 * Default export so `React.lazy()` can pick it up — the
 * `@excalidraw/excalidraw` package weighs ~3 MB and we don't want it
 * in the main bundle.
 *
 * Lifecycle:
 *   1. On mount: `fetch(convertFileSrc(absPath))` for the file's
 *      bytes (works for both `.excalidraw.svg` text and
 *      `.excalidraw.png` binary; `loadFromBlob` inspects the MIME
 *      type itself). Empty file → fresh empty scene.
 *   2. The `<Excalidraw>` component drives the canvas. Its `onChange`
 *      hands us `{ elements, appState, files }` which we stash in a
 *      ref so Cmd+S can serialise without forcing React re-renders.
 *   3. First user-driven edit fires `onDirty()`.
 *   4. Cmd+S calls `exportToSvg` (for `.excalidraw.svg`) or
 *      `exportToBlob({ mimeType: "image/png" })` (for `.excalidraw.png`)
 *      with `appState.exportEmbedScene: true`. Result hands off to
 *      `onSave(string | Uint8Array)`.
 */

import {
  Excalidraw,
  exportToBlob,
  exportToSvg,
  loadFromBlob,
} from "@excalidraw/excalidraw";
import "@excalidraw/excalidraw/index.css";
import type {
  ExcalidrawImperativeAPI,
  ExcalidrawInitialDataState,
} from "@excalidraw/excalidraw/types";
import type { OrderedExcalidrawElement } from "@excalidraw/excalidraw/element/types";
import type { AppState, BinaryFiles } from "@excalidraw/excalidraw/types";
import { convertFileSrc } from "@tauri-apps/api/core";
import { Loader2 } from "lucide-react";
import { useEffect, useRef, useState } from "react";

interface ExcalidrawEditorProps {
  /** Absolute path of the open `*.excalidraw.svg` *or* `.png` file.
   *  The trailing extension chooses the exporter. */
  path: string;
  onDirty: () => void;
  onSave: (data: string | Uint8Array) => void;
  theme: "light" | "dark";
}

interface LiveScene {
  elements: readonly OrderedExcalidrawElement[];
  appState: AppState;
  files: BinaryFiles;
}

export default function ExcalidrawEditor({
  path,
  onDirty,
  onSave,
  theme,
}: ExcalidrawEditorProps) {
  const [initialData, setInitialData] = useState<
    ExcalidrawInitialDataState | null | undefined
  >(undefined);
  const [loadError, setLoadError] = useState<string | null>(null);

  const apiRef = useRef<ExcalidrawImperativeAPI | null>(null);
  const sceneRef = useRef<LiveScene | null>(null);
  const dirtiedRef = useRef(false);

  useEffect(() => {
    let cancelled = false;
    dirtiedRef.current = false;
    setLoadError(null);

    async function load() {
      try {
        const res = await fetch(convertFileSrc(path));
        if (cancelled) return;
        if (!res.ok) {
          throw new Error(`fetch ${path}: HTTP ${res.status}`);
        }
        const blob = await res.blob();
        if (cancelled) return;
        if (blob.size === 0) {
          setInitialData(null);
          return;
        }
        const restored = await loadFromBlob(blob, null, null);
        if (cancelled) return;
        setInitialData({
          elements: restored.elements,
          appState: restored.appState,
          files: restored.files,
        });
      } catch (err) {
        if (cancelled) return;
        const message = err instanceof Error ? err.message : String(err);
        console.warn("[excalidraw] loadFromBlob failed", path, err);
        setLoadError(message);
        setInitialData(null);
      }
    }
    void load();
    return () => {
      cancelled = true;
    };
  }, [path]);

  useEffect(() => {
    const isPng = path.toLowerCase().endsWith(".excalidraw.png");
    const handler = async (e: KeyboardEvent) => {
      const isSave =
        (e.metaKey || e.ctrlKey) && !e.altKey && e.key.toLowerCase() === "s";
      if (!isSave) return;
      const scene = sceneRef.current;
      if (!scene) return;
      e.preventDefault();
      e.stopPropagation();
      try {
        const exportAppState = {
          ...scene.appState,
          exportEmbedScene: true,
        };
        if (isPng) {
          const blob = await exportToBlob({
            elements: scene.elements,
            appState: exportAppState,
            files: scene.files,
            mimeType: "image/png",
          });
          const bytes = new Uint8Array(await blob.arrayBuffer());
          onSave(bytes);
        } else {
          const svgEl = await exportToSvg({
            elements: scene.elements,
            appState: exportAppState,
            files: scene.files,
          });
          const svg = new XMLSerializer().serializeToString(svgEl);
          onSave(svg);
        }
      } catch (err) {
        console.error("[excalidraw] export failed", err);
      }
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [onSave, path]);

  if (initialData === undefined) {
    return (
      <div className="flex h-full w-full items-center justify-center text-xs text-muted-foreground">
        <Loader2 className="mr-2 size-4 animate-spin" /> loading drawing…
      </div>
    );
  }

  return (
    <div className="relative h-full w-full" data-tauri-drag-region={false}>
      {loadError ? (
        <div className="absolute left-2 top-2 z-10 rounded border border-destructive/40 bg-destructive/10 px-2 py-1 font-mono text-[10px] text-destructive">
          load: {loadError}
        </div>
      ) : null}
      <Excalidraw
        initialData={initialData}
        excalidrawAPI={(api) => {
          apiRef.current = api;
        }}
        theme={theme}
        onChange={(elements, appState, files) => {
          sceneRef.current = { elements, appState, files };
          if (!dirtiedRef.current) {
            dirtiedRef.current = true;
            onDirty();
          }
        }}
      />
    </div>
  );
}
