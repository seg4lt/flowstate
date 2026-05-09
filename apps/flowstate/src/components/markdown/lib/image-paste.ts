/**
 * Clipboard-paste-image → write-to-disk → insert-markdown handler.
 *
 * Listens for `paste` events on the editor's DOM. When the clipboard
 * carries an image (file MIME `image/*`), the handler:
 *
 *   1. Generates a name based on the open document's basename + a
 *      timestamp suffix.
 *   2. Calls `markdown_save_pasted_image` on the Rust side, which
 *      sandboxes the write to a `pasted/` subfolder inside the
 *      project root and dedupes filename collisions.
 *   3. Inserts `![alt](pasted/<name>.<ext>)` at the caret.
 *   4. Fires `onImageSaved(rel)` so the host can invalidate the
 *      file-tree query and the project-file index.
 *
 * Project-root sandbox: the Rust handler refuses any `targetDir` that
 * resolves outside the absolute project root. Without an open project
 * the handler bails early — no filesystem access without a sandbox.
 */

import { EditorView } from "@codemirror/view";
import { basenameNoExt, dirname, slugify, savePastedImage } from "./tauri";

const MIME_EXT: Record<string, string> = {
  "image/png": "png",
  "image/jpeg": "jpg",
  "image/jpg": "jpg",
  "image/gif": "gif",
  "image/webp": "webp",
  "image/avif": "avif",
  "image/svg+xml": "svg",
};

export interface ImagePasteOptions {
  /** Returns the absolute project root, or `null` when none. */
  getProjectPath: () => string | null;
  /** Returns the absolute directory of the open document. */
  getDocDir: () => string;
  /** Returns the project-relative path of the open document — used
   *  to seed the pasted image's filename stem. */
  getDocPath: () => string;
  /** Fired after a successful save, with the path relative to the
   *  document directory (the same string we just inserted into the
   *  buffer). The host invalidates the file tree + file index. */
  onImageSaved?: (relPath: string) => void;
}

export function imagePasteHandler(opts: ImagePasteOptions) {
  return EditorView.domEventHandlers({
    paste(event, view) {
      const items = event.clipboardData?.items;
      if (!items || items.length === 0) return;

      let imageItem: DataTransferItem | null = null;
      for (let i = 0; i < items.length; i++) {
        const it = items[i];
        if (it.kind === "file" && it.type.startsWith("image/")) {
          imageItem = it;
          break;
        }
      }
      if (!imageItem) return;

      const projectPath = opts.getProjectPath();
      if (!projectPath) {
        console.warn(
          "[markdown] image paste ignored — no open project to sandbox into",
        );
        return;
      }
      const targetDir = opts.getDocDir();
      if (!targetDir) {
        console.warn("[markdown] image paste ignored — no document directory");
        return;
      }

      const file = imageItem.getAsFile();
      if (!file) return;

      event.preventDefault();

      const ext = MIME_EXT[file.type] ?? "png";
      const docPath = opts.getDocPath();
      const stem = slugify(basenameNoExt(docPath || "image")) || "image";
      // Treat dirname(docPath) as a hint for the slug — we don't
      // actually need it now, but keeping the call documents intent.
      void dirname(docPath);
      const fileName = `${stem}-${Date.now()}.${ext}`;

      void (async () => {
        try {
          const buf = await file.arrayBuffer();
          const bytes = new Uint8Array(buf);
          const written = await savePastedImage(
            projectPath,
            targetDir,
            fileName,
            bytes,
          );
          const altText = written.split("/").pop() ?? written;
          const insert = `![${altText}](${written})`;
          const pos = view.state.selection.main.head;
          view.dispatch({
            changes: { from: pos, insert },
            selection: { anchor: pos + insert.length },
          });
          opts.onImageSaved?.(written);
        } catch (err) {
          console.error("[markdown] image paste failed", err);
        }
      })();
    },
  });
}
