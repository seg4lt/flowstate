/**
 * Theme-reactive widget for embedded `*.excalidraw.svg` / `.png`
 * images.
 *
 * Background: when the user saves an excalidraw drawing, the resulting
 * SVG/PNG has its colours **baked in** at the theme that was active at
 * that moment. The widget extracts the embedded scene at runtime and
 * re-exports it under the *current* app theme so a markdown embed
 * `![](foo.excalidraw.svg)` flips correctly when the user toggles
 * dark mode.
 *
 * The widget's `eq` returns `false` when the *theme* changes, which
 * triggers CodeMirror to re-mount the widget on a theme toggle and
 * runs the async render again with the new theme.
 */

import { WidgetType } from "@codemirror/view";
import { assetUrl } from "../tauri";

export class ExcalidrawImageWidget extends WidgetType {
  /**
   * @param src    Absolute filesystem path to the `*.excalidraw.svg`
   *               (or `*.excalidraw.png`).
   * @param alt    Markdown alt-text — used by the fallback `<img>`.
   * @param theme  Current app theme — re-export target.
   */
  constructor(
    private readonly src: string,
    private readonly alt: string,
    private readonly theme: "light" | "dark",
  ) {
    super();
  }

  eq(other: ExcalidrawImageWidget): boolean {
    return (
      other.src === this.src &&
      other.alt === this.alt &&
      other.theme === this.theme
    );
  }

  toDOM(): HTMLElement {
    const wrapper = document.createElement("span");
    wrapper.className = "cm-md-image-block cm-md-excalidraw-block";

    // Paint the on-disk SVG immediately so the user sees the diagram
    // before excalidraw's chunk loads + re-themes.
    const fallback = document.createElement("img");
    fallback.src = assetUrl(this.src);
    fallback.alt = this.alt;
    fallback.loading = "lazy";
    fallback.draggable = false;
    fallback.onerror = () => {
      const fb = document.createElement("span");
      fb.className = "cm-md-image-fallback";
      fb.textContent = `⚠️ image not found: ${this.alt || this.src}`;
      wrapper.replaceChildren(fb);
    };
    wrapper.appendChild(fallback);

    void this.renderThemed(wrapper);
    return wrapper;
  }

  private async renderThemed(wrapper: HTMLElement): Promise<void> {
    try {
      const res = await fetch(assetUrl(this.src));
      if (!res.ok) return;
      const blob = await res.blob();
      const { loadFromBlob, exportToSvg } = await import(
        "@excalidraw/excalidraw"
      );
      const restored = await loadFromBlob(blob, null, null);
      const svgEl = await exportToSvg({
        elements: restored.elements,
        appState: {
          ...restored.appState,
          theme: this.theme,
          viewBackgroundColor:
            this.theme === "dark" ? "#121212" : "#ffffff",
          exportEmbedScene: false,
        },
        files: restored.files,
      });
      svgEl.classList.add("cm-md-excalidraw-svg");
      wrapper.replaceChildren(svgEl);
    } catch (err) {
      console.debug(
        "[markdown] excalidraw re-theme skipped; using static image",
        err,
      );
    }
  }

  ignoreEvent(): boolean {
    return false;
  }
}
