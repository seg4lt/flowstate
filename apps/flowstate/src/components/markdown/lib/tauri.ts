/**
 * Markdown-editor Tauri wrappers.
 *
 * Thin typed `invoke` wrappers around the small set of Rust commands
 * the markdown live-preview / paste / mermaid features need. Distinct
 * from the broader `@/lib/api` surface so the markdown module is self-
 * contained — every IPC call it makes flows through this file.
 */

import { invoke, convertFileSrc } from "@tauri-apps/api/core";

/**
 * Save a clipboard-pasted image into a `pasted/` subfolder under the
 * directory of the open `.md`. Backend creates the subfolder if
 * missing and dedupes the filename when collisions occur. Returns the
 * **path relative to `targetDir`** that the editor should embed in
 * `![…](…)`.
 *
 * `projectPath` is the absolute project root used by the Rust side to
 * sandbox the write — refusing any `targetDir` that resolves outside
 * the project keeps a malformed editor state from writing anywhere on
 * the host filesystem.
 */
export function savePastedImage(
  projectPath: string,
  targetDir: string,
  fileName: string,
  bytes: Uint8Array,
): Promise<string> {
  return invoke<string>("markdown_save_pasted_image", {
    projectPath,
    targetDir,
    fileName,
    // Tauri serialises `Uint8Array` as a JSON number array on the
    // wire — mirrors the existing `read_file_as_base64` contract.
    bytes: Array.from(bytes),
  });
}

/** Write raw bytes to a project-relative path. Used by the Excalidraw
 *  editor's `.excalidraw.png` save. The string-content `.svg` save
 *  goes through the existing `writeProjectFile` — no need for a
 *  binary equivalent for that flow. */
export function writeProjectFileBytes(
  projectPath: string,
  file: string,
  bytes: Uint8Array,
): Promise<void> {
  return invoke<void>("write_project_file_bytes", {
    path: projectPath,
    file,
    bytes: Array.from(bytes),
  });
}

/** Rasterise an SVG to PNG and write the bitmap onto the OS clipboard.
 *  Used by the mermaid widget's "Copy as PNG" button. WKWebView's
 *  `<canvas>.toBlob` taints when an SVG is drawn into it, so we
 *  rasterise on the Rust side via `resvg` and push the bytes through
 *  the clipboard-manager plugin. */
export function copySvgAsPng(svg: string, scale = 2): Promise<void> {
  return invoke<void>("markdown_copy_svg_as_png", { svg, scale });
}

/**
 * Resolve a local file path to a webview-loadable URL. Tauri's asset
 * protocol exposes whitelisted folders to the webview; callers embed
 * this URL in `<img src=…>` to render images during Live Preview.
 */
export function assetUrl(path: string): string {
  return convertFileSrc(path);
}

// Re-export the path helpers so consumers (image-widget,
// link-autocomplete, image-paste, view-plugin) only need to import
// from this single module.
export {
  basename,
  basenameNoExt,
  dirname,
  joinPath,
  normalizePath,
  posixRelative,
  slugify,
} from "@/lib/paths";

// Editor-kind helpers also re-exported here so the live-preview
// internals don't reach across the codebase for them.
export {
  isExcalidrawPath,
  isMarkdownPath,
} from "@/lib/language-from-path";
