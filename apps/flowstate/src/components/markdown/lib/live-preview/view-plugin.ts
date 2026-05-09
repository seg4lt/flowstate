/**
 * Live-preview ViewPlugin.
 *
 * Walks the markdown syntax tree under the visible viewport on every
 * doc/selection change and emits CodeMirror `Decoration`s that:
 *
 *   1. Hide markup characters (`#`, `*`, `**`, backticks, link
 *      brackets) on lines where the cursor isn't currently parked.
 *   2. Apply heading line classes (`cm-md-h1` … `cm-md-h6`) so CSS
 *      can size them like Obsidian.
 *   3. Swap `![alt](path)` for an inline image widget when the line
 *      doesn't have the cursor.
 *   4. Mark `[[wikilinks]]` with a clickable class.  The actual click
 *      → navigate behaviour is wired separately in `wikilink.ts` so
 *      the store dispatch lives outside the plugin.
 *
 * The plugin is intentionally read-only — it never edits the document.
 * Saved state is just the current `DecorationSet`, which is recomputed
 * from scratch on each update; for our typical doc sizes (a few KB to
 * a few hundred KB) that's measurably cheaper than tracking diffs.
 */

import { syntaxTree } from "@codemirror/language";
import {
  type EditorState,
  type Range,
  RangeSetBuilder,
} from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  type EditorView,
  ViewPlugin,
  type ViewUpdate,
  WidgetType,
} from "@codemirror/view";
import { isExcalidrawPath } from "../tauri";
import { ExcalidrawImageWidget } from "./excalidraw-image-widget";
import { ImageWidget, resolveImageSrc } from "./image-widget";

class BulletWidget extends WidgetType {
  toDOM(): HTMLElement {
    const span = document.createElement("span");
    span.className = "cm-md-bullet";
    span.textContent = "•";
    return span;
  }
  eq(): boolean {
    return true;
  }
  ignoreEvent(): boolean {
    return false;
  }
}

const WIKILINK_RE = /\[\[([^\[\]\n]+?)\]\]/g;

/**
 * Standard markdown link `[text](url)` — fallback for cases lezer
 * rejects (notably link destinations with literal spaces). Negative
 * look-behind `(?<!!)` skips image syntax `![alt](src)`.
 */
const STD_LINK_RE = /(?<!!)\[([^\[\]\n]+?)\]\(([^()\n]+?)\)/g;

function buildDecorations(
  state: EditorState,
  docDir: string,
  theme: "light" | "dark",
): DecorationSet {
  const decorations: Range<Decoration>[] = [];
  const lineDecorations: Range<Decoration>[] = [];
  const cursor = state.selection.main.head;
  const cursorLine = state.doc.lineAt(cursor).number;
  const tree = syntaxTree(state);
  const lezerLinkRanges: Array<[number, number]> = [];

  tree.iterate({
    from: 0,
    to: state.doc.length,
    enter: (node) => {
      const type = node.type.name;

      if (type.startsWith("ATXHeading")) {
        const level = parseInt(type.slice("ATXHeading".length), 10);
        if (level >= 1 && level <= 6) {
          const line = state.doc.lineAt(node.from);
          lineDecorations.push(
            Decoration.line({ class: `cm-md-h${level}` }).range(line.from),
          );
        }
        return;
      }

      if (type === "Blockquote") {
        let pos = node.from;
        while (pos <= node.to) {
          const line = state.doc.lineAt(pos);
          lineDecorations.push(
            Decoration.line({ class: "cm-md-blockquote" }).range(line.from),
          );
          if (line.to >= node.to) break;
          pos = line.to + 1;
        }
        return;
      }

      if (type === "FencedCode") {
        let pos = node.from;
        while (pos <= node.to) {
          const line = state.doc.lineAt(pos);
          lineDecorations.push(
            Decoration.line({ class: "cm-md-fenced-line" }).range(line.from),
          );
          if (line.to >= node.to) break;
          pos = line.to + 1;
        }
      }

      if (type === "Image") {
        const line = state.doc.lineAt(node.from);
        if (line.number === cursorLine) return;
        const text = state.doc.sliceString(node.from, node.to);
        const m = /^!\[([^\]]*)\]\(([^)]+)\)$/.exec(text);
        if (m) {
          const [, alt, src] = m;
          const { resolved, isLocal } = resolveImageSrc(src, docDir);
          const isExcalidraw = isLocal && isExcalidrawPath(resolved);
          const widget = isExcalidraw
            ? new ExcalidrawImageWidget(resolved, alt, theme)
            : new ImageWidget(resolved, alt, isLocal);
          decorations.push(
            Decoration.replace({ widget }).range(node.from, node.to),
          );
        }
        return;
      }

      if (type === "InlineCode") {
        decorations.push(
          Decoration.mark({ class: "cm-md-inline-code" }).range(
            node.from,
            node.to,
          ),
        );
        return;
      }

      if (type === "StrongEmphasis") {
        decorations.push(
          Decoration.mark({ class: "cm-md-bold" }).range(node.from, node.to),
        );
        return;
      }
      if (type === "Emphasis") {
        decorations.push(
          Decoration.mark({ class: "cm-md-italic" }).range(node.from, node.to),
        );
        return;
      }
      if (type === "Strikethrough") {
        decorations.push(
          Decoration.mark({ class: "cm-md-strikethrough" }).range(
            node.from,
            node.to,
          ),
        );
        return;
      }

      if (type === "Link") {
        let url = "";
        let urlFrom = -1;
        let urlTo = -1;
        const cur = node.node.cursor();
        if (cur.firstChild()) {
          do {
            if (cur.type.name === "URL") {
              urlFrom = cur.from;
              urlTo = cur.to;
              url = state.doc.sliceString(cur.from, cur.to).trim();
              break;
            }
          } while (cur.nextSibling());
        }
        decorations.push(
          Decoration.mark({
            class: "cm-md-link",
            attributes: url ? { "data-link-url": url } : {},
          }).range(node.from, node.to),
        );
        const linkLine = state.doc.lineAt(node.from);
        if (
          urlFrom >= 0 &&
          urlTo > urlFrom &&
          linkLine.number !== cursorLine
        ) {
          decorations.push(Decoration.replace({}).range(urlFrom, urlTo));
        }
        lezerLinkRanges.push([node.from, node.to]);
        return;
      }

      if (type === "ListMark") {
        const line = state.doc.lineAt(node.from);
        if (line.number === cursorLine) return;
        if (node.to === node.from) return;
        const text = state.doc.sliceString(node.from, node.to);
        if (/^[-*+]$/.test(text)) {
          decorations.push(
            Decoration.replace({ widget: new BulletWidget() }).range(
              node.from,
              node.to,
            ),
          );
        }
        return;
      }

      const isMarkup =
        type === "HeaderMark" ||
        type === "EmphasisMark" ||
        type === "CodeMark" ||
        type === "LinkMark" ||
        type === "QuoteMark";
      if (isMarkup) {
        if (type === "CodeMark") {
          const parent = node.node.parent;
          if (parent && parent.type.name === "FencedCode") return;
        }
        const line = state.doc.lineAt(node.from);
        if (line.number === cursorLine) return;
        if (node.to === node.from) return;
        decorations.push(Decoration.replace({}).range(node.from, node.to));
      }
    },
  });

  // Pass 2a: fallback `[text](url)` scan for spaces in destinations
  const docText = state.doc.toString();
  STD_LINK_RE.lastIndex = 0;
  let stdLinkMatch: RegExpExecArray | null;
  while ((stdLinkMatch = STD_LINK_RE.exec(docText)) !== null) {
    const start = stdLinkMatch.index;
    const end = start + stdLinkMatch[0].length;
    const alreadyHandled = lezerLinkRanges.some(
      ([from, to]) => start >= from && end <= to,
    );
    if (alreadyHandled) continue;
    const label = stdLinkMatch[1];
    const url = stdLinkMatch[2];
    const closeBracketAt = start + 1 + label.length;
    const openParenAt = closeBracketAt + 1;
    const urlStart = openParenAt + 1;
    const urlEnd = urlStart + url.length;
    const stdLinkLine = state.doc.lineAt(start);
    if (stdLinkLine.number === cursorLine) {
      decorations.push(
        Decoration.mark({
          class: "cm-md-link",
          attributes: { "data-link-url": url },
        }).range(start, end),
      );
    } else {
      decorations.push(
        Decoration.mark({
          class: "cm-md-link",
          attributes: { "data-link-url": url },
        }).range(start, end),
      );
      decorations.push(Decoration.replace({}).range(start, start + 1));
      decorations.push(
        Decoration.replace({}).range(closeBracketAt, urlStart),
      );
      decorations.push(Decoration.replace({}).range(urlStart, urlEnd));
      decorations.push(Decoration.replace({}).range(urlEnd, end));
    }
  }

  // Pass 2b: wikilink scan
  WIKILINK_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = WIKILINK_RE.exec(docText)) !== null) {
    const start = m.index;
    const end = start + m[0].length;
    const line = state.doc.lineAt(start);
    if (line.number === cursorLine) {
      decorations.push(
        Decoration.mark({ class: "cm-md-wikilink" }).range(start, end),
      );
    } else {
      decorations.push(Decoration.replace({}).range(start, start + 2));
      decorations.push(
        Decoration.mark({
          class: "cm-md-wikilink",
          attributes: { "data-wikilink": m[1] },
        }).range(start + 2, end - 2),
      );
      decorations.push(Decoration.replace({}).range(end - 2, end));
    }
  }

  decorations.sort((a, b) => a.from - b.from || a.to - b.to);
  lineDecorations.sort((a, b) => a.from - b.from);

  const out = new RangeSetBuilder<Decoration>();
  let i = 0;
  let j = 0;
  while (i < lineDecorations.length || j < decorations.length) {
    const next =
      i < lineDecorations.length &&
      (j >= decorations.length ||
        lineDecorations[i].from <= decorations[j].from)
        ? lineDecorations[i++]
        : decorations[j++];
    out.add(next.from, next.to, next.value);
  }
  return out.finish();
}

/**
 * Public extension factory.  `getDocDir()` is a callback so the plugin
 * can re-resolve image paths whenever the open document changes
 * without needing a `Compartment` reconfigure.
 */
export function livePreviewPlugin(
  getDocDir: () => string,
  getTheme: () => "light" | "dark",
) {
  return ViewPlugin.fromClass(
    class {
      decorations: DecorationSet;

      constructor(view: EditorView) {
        this.decorations = buildDecorations(
          view.state,
          getDocDir(),
          getTheme(),
        );
      }

      update(u: ViewUpdate) {
        if (u.docChanged || u.selectionSet || u.viewportChanged) {
          this.decorations = buildDecorations(
            u.state,
            getDocDir(),
            getTheme(),
          );
        }
      }
    },
    {
      decorations: (v) => v.decorations,
    },
  );
}
