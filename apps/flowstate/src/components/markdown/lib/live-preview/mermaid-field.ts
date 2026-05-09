/**
 * `StateField` that emits **block-level** `Decoration.replace`
 * widgets for ```` ```mermaid ```` fenced blocks.
 *
 * CodeMirror only allows block decorations to come from state fields
 * (not from ViewPlugins), so this is split out from the rest of the
 * Live-Preview decorations.
 */

import { syntaxTree } from "@codemirror/language";
import {
  type EditorState,
  type Extension,
  type Range,
  RangeSetBuilder,
  StateField,
} from "@codemirror/state";
import { Decoration, type DecorationSet, EditorView } from "@codemirror/view";
import { MermaidWidget } from "./mermaid-widget";

function buildMermaidDecorations(state: EditorState): DecorationSet {
  const ranges: Range<Decoration>[] = [];
  const tree = syntaxTree(state);
  const cursor = state.selection.main.head;

  tree.iterate({
    from: 0,
    to: state.doc.length,
    enter: (node) => {
      if (node.type.name !== "FencedCode") return;
      let info: string | null = null;
      const cur = node.node.cursor();
      if (cur.firstChild()) {
        do {
          if (cur.type.name === "CodeInfo") {
            info = state.doc
              .sliceString(cur.from, cur.to)
              .toLowerCase()
              .trim();
            break;
          }
        } while (cur.nextSibling());
      }
      if (info !== "mermaid") return;
      if (cursor >= node.from && cursor <= node.to) return;
      const text = state.doc.sliceString(node.from, node.to);
      const firstNl = text.indexOf("\n");
      const lastNl = text.lastIndexOf("\n");
      if (firstNl === -1 || firstNl === lastNl) return;
      const source = text.slice(firstNl + 1, lastNl);
      if (source.trim().length === 0) return;
      ranges.push(
        Decoration.replace({
          widget: new MermaidWidget(source),
          block: true,
        }).range(node.from, node.to),
      );
    },
  });

  ranges.sort((a, b) => a.from - b.from || a.to - b.to);
  const builder = new RangeSetBuilder<Decoration>();
  for (const r of ranges) builder.add(r.from, r.to, r.value);
  return builder.finish();
}

export function mermaidField(): Extension {
  return StateField.define<DecorationSet>({
    create(state) {
      return buildMermaidDecorations(state);
    },
    update(value, tr) {
      if (tr.docChanged || tr.selection) {
        return buildMermaidDecorations(tr.state);
      }
      return value;
    },
    provide: (f) => EditorView.decorations.from(f),
  });
}
