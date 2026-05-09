/**
 * Mermaid diagram widget.
 *
 * A `Decoration.replace` decoration with `block: true` swaps the
 * source of a ```` ```mermaid ```` fenced block for a rendered SVG
 * whenever the cursor isn't inside the block. Putting the cursor on
 * any line of the block returns the raw markdown so the user can
 * edit.
 *
 * The mermaid library is heavy (~few MB) so we lazy-load it on first
 * use through a dynamic `import()`. Vite splits this into its own
 * chunk; users with no mermaid blocks pay zero bundle cost.
 *
 * Renders are cached by source-string so re-rendering an unchanged
 * doc (CodeMirror's view-plugin runs on every selection change) is
 * a Map lookup, not an svg recompute.
 */

import { WidgetType } from "@codemirror/view";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { copySvgAsPng } from "../tauri";

type MermaidLib = typeof import("mermaid")["default"];

let mermaidPromise: Promise<MermaidLib> | null = null;

function loadMermaid(): Promise<MermaidLib> {
  if (mermaidPromise) return mermaidPromise;
  mermaidPromise = import("mermaid").then((m) => {
    const lib = m.default;
    const isDark = document.documentElement.classList.contains("dark");
    lib.initialize({
      startOnLoad: false,
      theme: isDark ? "dark" : "default",
      securityLevel: "strict",
      fontFamily:
        "ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, 'Liberation Mono', 'Courier New', monospace",
      // resvg can't render foreignObject content. Native SVG <text>
      // labels are nearly identical visually and round-trip through
      // the Rust rasteriser cleanly.
      htmlLabels: false,
      flowchart: { htmlLabels: false },
      class: { htmlLabels: false },
      state: { htmlLabels: false },
      // Mermaid 11 destructures `config.treemap.tile` during global
      // init even when the doc has no treemap. Seed defaults so that
      // doesn't throw.
      treemap: {
        useMaxWidth: true,
        padding: 1,
        diagramPadding: 8,
        showValues: true,
        valueFontSize: 11,
        labelFontSize: 12,
        valueFormat: ",",
        tile: "squarify",
      },
      kanban: { useMaxWidth: true },
      architecture: { useMaxWidth: true },
    } as Parameters<MermaidLib["initialize"]>[0]);
    return lib;
  });
  return mermaidPromise;
}

const renderCache = new Map<string, string>();
const RENDER_CACHE_LIMIT = 32;

let counter = 0;
function uniqueId(): string {
  counter += 1;
  return `cm-md-mermaid-${counter}`;
}

async function renderMermaid(source: string): Promise<string> {
  const cached = renderCache.get(source);
  if (cached !== undefined) return cached;
  const lib = await loadMermaid();
  const id = uniqueId();
  try {
    const { svg } = await lib.render(id, source);
    renderCache.set(source, svg);
    if (renderCache.size > RENDER_CACHE_LIMIT) {
      const first = renderCache.keys().next().value;
      if (first !== undefined) renderCache.delete(first);
    }
    return svg;
  } finally {
    document.getElementById(id)?.remove();
  }
}

const COPY_ICON_SVG = `
<svg viewBox="0 0 24 24" width="12" height="12" fill="none"
     stroke="currentColor" stroke-width="2"
     stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
  <rect width="14" height="14" x="8" y="8" rx="2" ry="2"></rect>
  <path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"></path>
</svg>`;

function makeToolbar(wrapper: HTMLElement): HTMLElement {
  const toolbar = document.createElement("div");
  toolbar.className = "cm-md-mermaid-toolbar";
  toolbar.dataset.cmMermaidToolbar = "true";
  const swallow = (e: Event) => {
    e.preventDefault();
    e.stopPropagation();
  };
  toolbar.addEventListener("pointerdown", swallow);
  toolbar.addEventListener("mousedown", swallow);
  toolbar.appendChild(
    makeCopyButton({
      label: "SVG",
      title: "Copy as SVG (vector)",
      onClick: () => copyAsSvg(wrapper),
    }),
  );
  toolbar.appendChild(
    makeCopyButton({
      label: "PNG",
      title: "Copy as PNG (raster, 2× scale)",
      onClick: () => copyAsPng(wrapper),
    }),
  );
  return toolbar;
}

interface CopyButtonOpts {
  label: string;
  title: string;
  onClick: () => Promise<boolean>;
}

function makeCopyButton(opts: CopyButtonOpts): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "cm-md-mermaid-copy";
  btn.title = opts.title;
  btn.innerHTML = `${COPY_ICON_SVG}<span>${opts.label}</span>`;
  btn.addEventListener("click", (e) => {
    e.preventDefault();
    e.stopPropagation();
    void opts.onClick().then((ok) => {
      flashLabel(btn, ok ? "Copied" : "Failed");
    });
  });
  return btn;
}

function flashLabel(btn: HTMLButtonElement, text: string): void {
  const original = btn.innerHTML;
  btn.innerHTML = `${COPY_ICON_SVG}<span>${text}</span>`;
  btn.classList.add("cm-md-mermaid-copy-flashing");
  window.setTimeout(() => {
    btn.innerHTML = original;
    btn.classList.remove("cm-md-mermaid-copy-flashing");
  }, 1200);
}

function findSvg(wrapper: HTMLElement): SVGSVGElement | null {
  return wrapper.querySelector<SVGSVGElement>(".cm-md-mermaid-svg svg");
}

async function copyAsSvg(wrapper: HTMLElement): Promise<boolean> {
  const svg = findSvg(wrapper);
  if (!svg) return false;
  try {
    const xml = new XMLSerializer().serializeToString(svg);
    await writeText(xml);
    return true;
  } catch (err) {
    console.warn("[mermaid] copy SVG failed", err);
    return false;
  }
}

async function copyAsPng(wrapper: HTMLElement): Promise<boolean> {
  const svg = findSvg(wrapper);
  if (!svg) return false;
  try {
    const xml = new XMLSerializer().serializeToString(svg);
    await copySvgAsPng(xml, 2);
    return true;
  } catch (err) {
    console.warn("[mermaid] copy PNG failed", err);
    return false;
  }
}

export class MermaidWidget extends WidgetType {
  constructor(private readonly source: string) {
    super();
  }

  eq(other: MermaidWidget): boolean {
    return other.source === this.source;
  }

  toDOM(): HTMLElement {
    const wrapper = document.createElement("div");
    wrapper.className = "cm-md-mermaid";

    const toolbar = makeToolbar(wrapper);
    wrapper.appendChild(toolbar);

    const status = document.createElement("div");
    status.className = "cm-md-mermaid-status";
    status.textContent = "rendering diagram…";
    wrapper.appendChild(status);

    void renderMermaid(this.source.trim())
      .then((svg) => {
        status.remove();
        const div = document.createElement("div");
        div.className = "cm-md-mermaid-svg";
        div.innerHTML = svg;
        wrapper.appendChild(div);
      })
      .catch((err: unknown) => {
        status.remove();
        const message = err instanceof Error ? err.message : String(err);
        const errBox = document.createElement("pre");
        errBox.className = "cm-md-mermaid-error";
        errBox.textContent = `mermaid error\n${message}`;
        wrapper.appendChild(errBox);
        toolbar.style.display = "none";
      });

    return wrapper;
  }

  ignoreEvent(event: Event): boolean {
    const target = event.target;
    if (target instanceof Element) {
      if (target.closest("[data-cm-mermaid-toolbar='true']")) return true;
    }
    return false;
  }
}
