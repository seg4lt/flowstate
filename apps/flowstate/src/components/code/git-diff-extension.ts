import {
  Range,
  StateEffect,
  StateField,
  type Extension,
} from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  gutter,
  GutterMarker,
} from "@codemirror/view";

// CodeMirror 6 extension that paints **gutter markers + faint line
// backgrounds** on lines that have been added or modified relative
// to the file's HEAD version. Removed lines are *not* rendered as
// ghost widgets — that path needs CM line widgets, recomputes on
// every edit, and gets expensive on large files. The user explicitly
// asked for the cheap version. If we ever want red ghosts, that's
// an additive change to this same StateField.
//
// Three pieces:
//   1. `gitDiffField` — a `StateField<DecorationSet>` of
//      `Decoration.line({ class })` ranges. Mapped through change
//      sets so existing markers stay anchored to their lines while
//      the user types, until the next `setGitDiffEffect` replaces
//      them.
//   2. `gitDiffGutter` — a CM6 gutter that paints a 2px coloured
//      strip on the same lines (computed by hit-testing the
//      RangeSet). Gutter markers are pure DOM diffing on visible
//      lines, so cost is O(viewport).
//   3. Theme block (folded into `gitDiffExtension`) — defines
//      `.cm-git-added` / `.cm-git-modified` colours, plus the
//      gutter strip widths.
//
// The owner of the editor (CodeView → CodeEditor) is responsible
// for fetching the {before, after} pair via `getGitDiffFile`,
// computing line numbers, and dispatching `setGitDiffEffect` once
// per file open. We do **not** recompute on keystrokes — the
// existing diff panel already operates on the same "snapshot at
// open" model, and the marker set is mapped through edits so
// existing markers stay visually anchored.

export interface GitDiffLines {
  /** 0-indexed line numbers (in the *current* / "after" file) that
   *  are net-new compared to HEAD. */
  added: number[];
  /** 0-indexed line numbers that exist in both HEAD and the current
   *  file but with different contents. */
  modified: number[];
}

export const setGitDiffEffect = StateEffect.define<GitDiffLines>();
export const clearGitDiffEffect = StateEffect.define<void>();

const addedLineDeco = Decoration.line({
  attributes: { class: "cm-git-added" },
});
const modifiedLineDeco = Decoration.line({
  attributes: { class: "cm-git-modified" },
});

function buildDecorations(
  doc: { lines: number; line(n: number): { from: number } },
  lines: GitDiffLines,
): DecorationSet {
  const ranges: Range<Decoration>[] = [];
  const total = doc.lines;
  // We sort + walk the line numbers so the decoration set stays
  // sorted (a CM6 invariant — `Decoration.set(ranges, true)` will
  // sort, but pre-sorting is essentially free here).
  const seen = new Set<number>();
  const merged: Array<{ line: number; kind: "added" | "modified" }> = [];
  for (const l of lines.added) {
    if (l < 0 || l >= total || seen.has(l)) continue;
    seen.add(l);
    merged.push({ line: l, kind: "added" });
  }
  for (const l of lines.modified) {
    if (l < 0 || l >= total || seen.has(l)) continue;
    seen.add(l);
    merged.push({ line: l, kind: "modified" });
  }
  merged.sort((a, b) => a.line - b.line);
  for (const { line, kind } of merged) {
    const from = doc.line(line + 1).from;
    ranges.push((kind === "added" ? addedLineDeco : modifiedLineDeco).range(from));
  }
  return Decoration.set(ranges);
}

export const gitDiffField = StateField.define<DecorationSet>({
  create() {
    return Decoration.none;
  },
  update(deco, tr) {
    // Map through any text changes so existing markers stay
    // anchored as the user types around them.
    let next = deco.map(tr.changes);
    for (const e of tr.effects) {
      if (e.is(setGitDiffEffect)) {
        next = buildDecorations(tr.state.doc, e.value);
      } else if (e.is(clearGitDiffEffect)) {
        next = Decoration.none;
      }
    }
    return next;
  },
  provide(f) {
    return EditorView.decorations.from(f);
  },
});

// ── gutter strip ────────────────────────────────────────────────

class GitDiffGutterMarker extends GutterMarker {
  constructor(private readonly kind: "added" | "modified") {
    super();
  }
  override eq(other: GutterMarker): boolean {
    return other instanceof GitDiffGutterMarker && other.kind === this.kind;
  }
  override toDOM(): Node {
    const span = document.createElement("span");
    span.className =
      this.kind === "added"
        ? "cm-git-gutter cm-git-gutter-added"
        : "cm-git-gutter cm-git-gutter-modified";
    return span;
  }
}

const ADDED_MARKER = new GitDiffGutterMarker("added");
const MODIFIED_MARKER = new GitDiffGutterMarker("modified");

const gitDiffGutter = gutter({
  class: "cm-git-diff-gutter",
  lineMarker(view, line) {
    const decos = view.state.field(gitDiffField, /* require */ false);
    if (!decos || decos.size === 0) return null;
    let marker: GutterMarker | null = null;
    decos.between(line.from, line.from, (_from, _to, value) => {
      // The `class` is buried in the spec; cheaper to read it back
      // off the attribute we set when constructing the deco.
      const cls = (value.spec.attributes as { class?: string } | undefined)
        ?.class;
      if (cls === "cm-git-added") {
        marker = ADDED_MARKER;
        return false;
      }
      if (cls === "cm-git-modified") {
        marker = MODIFIED_MARKER;
        return false;
      }
      return undefined;
    });
    return marker;
  },
  initialSpacer: () => ADDED_MARKER,
});

// ── theme ───────────────────────────────────────────────────────

// Soft backgrounds + saturated 2px gutter strip. Same colours work
// in both light and dark themes because they're alpha-blended.
const gitDiffTheme = EditorView.theme({
  ".cm-git-added": {
    backgroundColor: "rgba(34, 197, 94, 0.10)",
  },
  ".cm-git-modified": {
    backgroundColor: "rgba(234, 179, 8, 0.10)",
  },
  ".cm-git-diff-gutter": {
    width: "3px",
    padding: "0",
  },
  ".cm-git-diff-gutter .cm-gutterElement": {
    padding: "0",
    width: "3px",
  },
  ".cm-git-gutter": {
    display: "block",
    width: "3px",
    height: "100%",
  },
  ".cm-git-gutter-added": {
    backgroundColor: "rgb(34, 197, 94)",
  },
  ".cm-git-gutter-modified": {
    backgroundColor: "rgb(234, 179, 8)",
  },
});

// Public extension array. Mount via a Compartment in the editor so
// flipping git mode off reconfigures it to `[]` — zero overhead
// when not in use.
export function gitDiffExtension(): Extension {
  return [gitDiffField, gitDiffGutter, gitDiffTheme];
}

// ── line-diff helper ────────────────────────────────────────────

// Cheap line-diff that produces the GitDiffLines payload. Uses an
// LCS-table walk on lines (Hunt-Szymanski-ish — a dynamic-program
// over line equality, not character equality). For per-file diffs
// of typical source files (a few hundred to a few thousand lines)
// this is well under a millisecond. Files with > LINE_LIMIT lines
// fall back to "all lines modified" — we'd rather skip the cost
// than block the editor.
//
// The output line numbers are 0-indexed against `after` (the
// current buffer content), which is what `setGitDiffEffect`
// expects.
const LINE_LIMIT = 5000;

export function diffLines(before: string, after: string): GitDiffLines {
  const beforeLines = before.length === 0 ? [] : before.split("\n");
  const afterLines = after.length === 0 ? [] : after.split("\n");
  const m = beforeLines.length;
  const n = afterLines.length;

  if (m === 0 && n === 0) return { added: [], modified: [] };
  if (m === 0) {
    // Brand-new file (or one that was empty at HEAD): every line
    // is added.
    return {
      added: Array.from({ length: n }, (_, i) => i),
      modified: [],
    };
  }
  // Bail-out for huge files — paint everything as modified, which
  // visually flags "this file changed a lot" without the LCS cost.
  if (m > LINE_LIMIT || n > LINE_LIMIT) {
    return {
      added: [],
      modified: Array.from({ length: n }, (_, i) => i),
    };
  }

  // LCS table on line equality. Allocates an (m+1) x (n+1) Uint16
  // matrix — 5000^2 * 2 bytes = 50 MB worst case, which we already
  // capped at LINE_LIMIT above. Typical files are tiny.
  const dp: Uint16Array = new Uint16Array((m + 1) * (n + 1));
  const idx = (i: number, j: number) => i * (n + 1) + j;
  for (let i = m - 1; i >= 0; i--) {
    for (let j = n - 1; j >= 0; j--) {
      if (beforeLines[i] === afterLines[j]) {
        dp[idx(i, j)] = dp[idx(i + 1, j + 1)] + 1;
      } else {
        const a = dp[idx(i + 1, j)];
        const b = dp[idx(i, j + 1)];
        dp[idx(i, j)] = a > b ? a : b;
      }
    }
  }

  // Walk the table to produce a per-after-line classification:
  //   "same"     — line is in the LCS, untouched
  //   "added"    — line is in `after` but not in `before`
  //   "modified" — `after` line corresponds to a `before` line that
  //                was replaced (the diff is a delete+insert pair)
  // We collapse adjacent delete+insert into "modified" so the
  // user sees a yellow block on the line they changed rather than
  // a green-on-nothing.
  type Op = "same" | "add" | "del";
  const ops: Op[] = [];
  let i = 0;
  let j = 0;
  while (i < m && j < n) {
    if (beforeLines[i] === afterLines[j]) {
      ops.push("same");
      i++;
      j++;
    } else if (dp[idx(i + 1, j)] >= dp[idx(i, j + 1)]) {
      ops.push("del");
      i++;
    } else {
      ops.push("add");
      j++;
    }
  }
  while (i < m) {
    ops.push("del");
    i++;
  }
  while (j < n) {
    ops.push("add");
    j++;
  }

  const added: number[] = [];
  const modified: number[] = [];
  let afterIdx = 0;
  for (let k = 0; k < ops.length; k++) {
    const op = ops[k];
    if (op === "same") {
      afterIdx++;
    } else if (op === "add") {
      // Look back: if the previous op was a delete (or run of
      // deletes), classify this insert as a "modified" line on
      // the same visual row instead of a green add. Better UX
      // for typical edits where the user replaced a line.
      const prev = ops[k - 1];
      if (prev === "del") {
        modified.push(afterIdx);
      } else {
        added.push(afterIdx);
      }
      afterIdx++;
    }
    // "del" — line vanished from `after`, no row to mark on the
    // current buffer. We deliberately drop it (no red ghost).
  }
  return { added, modified };
}
