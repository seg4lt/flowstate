/**
 * Inline image widget for Live Preview.
 *
 * A `Decoration.replace` decoration substitutes the raw `![alt](path)`
 * markdown with an `<img>` whenever the cursor is *not* on that line.
 * The user can edit the markdown by clicking through it — placing the
 * caret in the line removes the decoration.
 *
 * Image source resolution:
 *   - `path` may be absolute (`/Users/.../foo.png`), a `file://` URL,
 *     or relative to the document directory.
 *   - Relative paths are resolved against `docDir` (the directory of
 *     the open `.md`).
 *   - Both forms are converted to a webview-safe URL via Tauri's
 *     `convertFileSrc` (the asset protocol). Without this the webview
 *     refuses to load arbitrary file:// URLs.
 */

import { WidgetType } from "@codemirror/view";
import { assetUrl, normalizePath } from "../tauri";

export class ImageWidget extends WidgetType {
  /**
   * @param src       Resolved source, either an absolute filesystem
   *                  path or an `http(s)` URL (we pass the latter
   *                  straight through).
   * @param alt       The markdown alt-text — used for fallback display.
   * @param isLocal   When `true`, `src` is a filesystem path and we
   *                  must run it through `assetUrl()` before assigning
   *                  to `<img src>`.
   */
  constructor(
    private readonly src: string,
    private readonly alt: string,
    private readonly isLocal: boolean,
  ) {
    super();
  }

  eq(other: ImageWidget): boolean {
    return (
      other.src === this.src &&
      other.alt === this.alt &&
      other.isLocal === this.isLocal
    );
  }

  toDOM(): HTMLElement {
    const wrapper = document.createElement("span");
    wrapper.className = "cm-md-image-block";

    const img = document.createElement("img");
    img.src = this.isLocal ? assetUrl(this.src) : this.src;
    img.alt = this.alt;
    img.loading = "lazy";
    img.draggable = false;
    img.onerror = () => {
      const fb = document.createElement("span");
      fb.className = "cm-md-image-fallback";
      fb.textContent = `⚠️ image not found: ${this.alt || this.src}`;
      wrapper.replaceChildren(fb);
    };
    wrapper.appendChild(img);
    return wrapper;
  }

  ignoreEvent(): boolean {
    return false;
  }
}

/**
 * Resolve a markdown image `src` against the document's directory.
 *
 * Returns `{ resolved, isLocal }` where `resolved` is the value to
 * pass to `ImageWidget`. External URLs (`http://…`, `https://…`,
 * `data:…`) are returned as-is with `isLocal: false`.
 */
export function resolveImageSrc(
  src: string,
  docDir: string,
): { resolved: string; isLocal: boolean } {
  if (/^(https?:|data:)/i.test(src)) {
    return { resolved: src, isLocal: false };
  }
  let raw = src;
  try {
    raw = decodeURIComponent(src);
  } catch {
    /* malformed encoding — fall back to literal */
  }
  if (raw.startsWith("file://")) {
    return {
      resolved: normalizePath(raw.slice("file://".length)),
      isLocal: true,
    };
  }
  if (raw.startsWith("/")) {
    return { resolved: normalizePath(raw), isLocal: true };
  }
  return { resolved: normalizePath(`${docDir}/${raw}`), isLocal: true };
}
