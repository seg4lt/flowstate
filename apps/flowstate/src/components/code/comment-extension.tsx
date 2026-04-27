import { createRoot, type Root } from "react-dom/client";
import { Prec, StateEffect, StateField, type Extension } from "@codemirror/state";
import {
  EditorView,
  gutter,
  GutterMarker,
  keymap,
  showTooltip,
  type Tooltip,
  type TooltipView,
  ViewPlugin,
} from "@codemirror/view";
import {
  addComment,
  type DiffCommentAnchor,
} from "@/lib/diff-comments-store";
import { CommentPopup } from "@/components/chat/comment-popup";

// CM6 extension that surfaces the line-comment-to-composer affordance
// (parity with `DiffCommentOverlay`, but built natively on CM6 state
// + tooltip APIs instead of walking the @pierre/diffs shadow DOM).
//
// Two entry points:
//
//   1. Hover **anywhere in the gutter column** (line numbers, fold
//      marker, comment cell) → a "+" appears in the comment gutter
//      cell of that line. Click that cell → popup pinned to the
//      line. Hovering the code content does NOT trigger the "+".
//   2. Mod-Alt-c → opens popup at the cursor's line. If a non-empty
//      selection is active, opens a *range* comment with the
//      selected text attached.
//
// Hover detection:
//
//   A `mousemove` listener on `view.dom` checks whether the event
//   target is inside `.cm-gutters`. If yes, we resolve the line via
//   `view.lineBlockAtHeight(y)` — that returns the visual block at
//   the cursor's y, not a clamped doc position, which sidesteps the
//   off-by-one we hit with `view.posAtCoords` at line boundaries
//   (and the worse 2-3-line skew on backward selections, where the
//   selection-drag layer was perturbing pos resolution).
//
//   Hover state lives in a CM6 `StateField<number | null>`. We do
//   NOT rely on CSS `:hover` to draw the icon, because browsers
//   pin `:hover` on a cell when pointer events get coalesced during
//   fast moves — leaving the "+" stuck visible after the mouse has
//   long since left. Explicit JS state with an unconditional clear
//   on `mouseleave` (and on every mousemove that lands outside the
//   gutters) never gets stuck.
//
//   The line-number / fold gutters are far easier to aim at than a
//   thin comment-only column, so this also fixes the "the gutter is
//   too narrow to hit" complaint without making any column wider.
//
// Anchoring: the popup is a CM6 tooltip pinned to a doc position.
// CM6 remaps the position through change sets and tracks scroll /
// soft-wrap / fold reflow automatically — none of which the
// overlay's pixel positioning could survive.

// ─── effects ─────────────────────────────────────────────────────

const setHoverLineEffect = StateEffect.define<number | null>();

type PopupTrigger =
  | { kind: "line"; line: number; pos: number }
  | {
      kind: "range";
      lineRange: [number, number];
      selectionText: string;
      pos: number;
    };

const setPopupEffect = StateEffect.define<PopupTrigger>();
const clearPopupEffect = StateEffect.define<void>();

// ─── popup state ─────────────────────────────────────────────────

interface PopupState {
  trigger: PopupTrigger;
}

// ─── hover state ─────────────────────────────────────────────────

const hoverLineField = StateField.define<number | null>({
  create: () => null,
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(setHoverLineEffect)) return e.value;
    }
    // Drop hover when the doc changes — the user is typing, not
    // aiming at a row, and old line numbers may not point at the
    // same content anyway.
    if (tr.docChanged) return null;
    return value;
  },
});

// ─── plus gutter marker (stateless, shared across all lines) ────

class PlusGutterMarker extends GutterMarker {
  override eq(other: GutterMarker): boolean {
    return other instanceof PlusGutterMarker;
  }
  override toDOM(): Node {
    // The cell-level CSS `:hover` rule (in the theme block below)
    // toggles this element's opacity — no JS hover state. The
    // click handler lives on the gutter, not the marker, so the
    // marker DOM is purely presentational.
    const span = document.createElement("span");
    span.className =
      "cm-comment-add-btn inline-flex h-3.5 w-3.5 items-center justify-center rounded-full border border-border bg-primary text-primary-foreground shadow-sm";
    span.innerHTML =
      '<svg width="9" height="9" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round"><path d="M12 5v14M5 12h14"/></svg>';
    return span;
  }
}

// One marker instance reused across every visible line. CM6's gutter
// virtualizes — only visible cells get a marker — so the cost is
// O(viewport), not O(file size).
const PLUS_MARKER = new PlusGutterMarker();

// ─── extension factory ───────────────────────────────────────────

interface CommentExtensionConfig {
  /** Project-relative path of the open file. Used as the comment
   *  anchor's `path`. */
  path: string;
  /** Active chat session. The extension is wired only when a
   *  session exists — caller should pass `[]` (or skip mounting)
   *  when sessionId is null. */
  sessionId: string;
}

export function commentExtension(cfg: CommentExtensionConfig): Extension {
  const { path: filePath, sessionId } = cfg;

  // Build the popup field with a closure-captured Tooltip factory.
  // The factory converts a popup state value into a Tooltip for the
  // showTooltip facet.
  const popupField = StateField.define<PopupState | null>({
    create: () => null,
    update(value, tr) {
      for (const e of tr.effects) {
        if (e.is(setPopupEffect)) return { trigger: e.value };
        if (e.is(clearPopupEffect)) return null;
      }
      if (value && tr.docChanged) {
        // Remap the anchor pos through the change set so the popup
        // tracks edits made above its line (insert a line at the
        // top → popup slides down with the rest of the buffer).
        const newPos = tr.changes.mapPos(value.trigger.pos);
        return {
          trigger: { ...value.trigger, pos: newPos },
        };
      }
      return value;
    },
    provide: (f) =>
      showTooltip.from(f, (state) => {
        if (state === null) return null;
        return makeTooltip(state.trigger, filePath, sessionId);
      }),
  });

  // ─── plus button gutter ─────────────────────────────────────────

  // The comment gutter renders the "+" only on the line currently
  // tracked in `hoverLineField`. The hover field is driven by the
  // ViewPlugin below — set on mousemove inside `.cm-gutters`,
  // cleared on mousemove outside it (or on mouseleave the editor).
  // Click on the gutter cell opens the popup for that exact line
  // (using `line.from` from the gutter callback — authoritative).
  const commentGutter = gutter({
    class: "cm-comment-gutter",
    lineMarker(view, line) {
      const hovered = view.state.field(hoverLineField, /* require */ false);
      if (hovered == null) return null;
      const lineNumber = view.state.doc.lineAt(line.from).number;
      return lineNumber === hovered ? PLUS_MARKER : null;
    },
    // CRITICAL: without `lineMarkerChange`, CM6 only re-runs
    // `lineMarker` on doc / viewport changes — state-field-only
    // updates (which is what every `setHoverLineEffect` produces)
    // would update the field but never refresh the gutter, so the
    // "+" would never appear. We opt in here by returning true
    // whenever the hover line changed between transactions.
    lineMarkerChange(update) {
      return (
        update.startState.field(hoverLineField, false) !==
        update.state.field(hoverLineField, false)
      );
    },
    // Pre-allocate the column so the editor doesn't shift when a
    // "+" first appears under the cursor.
    initialSpacer: () => SPACER_MARKER,
    domEventHandlers: {
      mousedown(view, line, event) {
        const lineNumber = view.state.doc.lineAt(line.from).number;
        view.dispatch({
          effects: [
            setPopupEffect.of({
              kind: "line",
              line: lineNumber,
              pos: line.from,
            }),
            setHoverLineEffect.of(null),
          ],
        });
        // Prevent the editor from grabbing focus / starting a
        // selection on the gutter click — we want the popup's
        // textarea to focus instead (CommentPopup auto-focuses).
        event.preventDefault();
        return true;
      },
    },
  });

  // ─── hover ViewPlugin ──────────────────────────────────────────
  //
  // One mousemove listener on the editor DOM. Branches:
  //
  //   * Target inside `.cm-gutters` → resolve the line via
  //     `view.lineBlockAtHeight(y)` (NOT `posAtCoords` — that
  //     clamps at line boundaries, which is what gave the previous
  //     version its off-by-one).
  //   * Target anywhere else (code area, scrollbar, etc.) → clear
  //     hover. This is what stops the "+" from sticking after a
  //     fast mouse move past the gutter.
  //
  // No RAF throttle: we early-return when the resolved line equals
  // the field's current value, so the dispatch only happens on
  // cell-to-cell transitions — which CM6 absorbs cheaply.
  const hoverPlugin = ViewPlugin.define((view) => {
    function setHover(line: number | null): void {
      const cur = view.state.field(hoverLineField, /* require */ false);
      if (cur === line) return;
      view.dispatch({ effects: setHoverLineEffect.of(line) });
    }

    function onMouseMove(e: MouseEvent): void {
      // Hit-test on the GUTTER COLUMN's bounding rect, not on
      // individual cell DOM elements. Reason: the comment gutter
      // only emits a cell DOM for the *hovered* line; as the user
      // moves the mouse from a line number toward the "+" in the
      // comment gutter, their pointer can briefly land on the
      // gutter container itself (no cell descendant), at which
      // point `target.closest('.cm-gutterElement')` returns null
      // and the previous code cleared hover — making the "+"
      // disappear right as the user was reaching for it.
      //
      // The fix is to ignore the event target entirely and ask
      // "is the cursor's x inside the gutters column, and which
      // line block does its y fall in?". Both are facts about the
      // editor's layout, independent of what cells are or aren't
      // currently rendered.
      const guttersEl = view.dom.querySelector(
        ".cm-gutters",
      ) as HTMLElement | null;
      if (!guttersEl) {
        setHover(null);
        return;
      }
      const guttersRect = guttersEl.getBoundingClientRect();
      if (
        e.clientX < guttersRect.left ||
        e.clientX > guttersRect.right ||
        e.clientY < guttersRect.top ||
        e.clientY > guttersRect.bottom
      ) {
        setHover(null);
        return;
      }
      // Resolve the line by iterating the viewport's line blocks
      // and finding the one whose viewport y-range contains
      // `clientY`. Each block's `coords.top` / `coords.bottom`
      // come from CM6's heightmap — same source CM6 uses to lay
      // out content — so we're guaranteed to match the line the
      // user visually sees.
      let foundLine: number | null = null;
      for (const block of view.viewportLineBlocks) {
        const coords = view.coordsAtPos(block.from);
        if (!coords) continue;
        // Strict less-than on the bottom so the boundary pixel
        // belongs to exactly one block.
        if (e.clientY >= coords.top && e.clientY < coords.bottom) {
          foundLine = view.state.doc.lineAt(block.from).number;
          break;
        }
      }
      if (foundLine == null) {
        setHover(null);
        return;
      }
      setHover(foundLine);
    }

    function onMouseLeave(): void {
      setHover(null);
    }

    view.dom.addEventListener("mousemove", onMouseMove);
    view.dom.addEventListener("mouseleave", onMouseLeave);

    return {
      destroy(): void {
        view.dom.removeEventListener("mousemove", onMouseMove);
        view.dom.removeEventListener("mouseleave", onMouseLeave);
      },
    };
  });

  // ─── keymap: Mod-Alt-c opens popup at cursor / selection ─────────

  const commentKeymap = keymap.of([
    {
      key: "Mod-Alt-c",
      preventDefault: true,
      run(view) {
        const sel = view.state.selection.main;
        const doc = view.state.doc;
        if (sel.from !== sel.to) {
          // Range comment: build lineRange + selectionText.
          const fromLine = doc.lineAt(sel.from).number;
          const toLine = doc.lineAt(sel.to).number;
          const text = view.state.sliceDoc(sel.from, sel.to);
          view.dispatch({
            effects: setPopupEffect.of({
              kind: "range",
              lineRange: [fromLine, toLine],
              selectionText: text,
              pos: sel.from,
            }),
          });
        } else {
          // Line comment at the cursor's line.
          const line = doc.lineAt(sel.head);
          view.dispatch({
            effects: setPopupEffect.of({
              kind: "line",
              line: line.number,
              pos: line.from,
            }),
          });
        }
        return true;
      },
    },
  ]);

  // ─── theme overrides ────────────────────────────────────────────

  const themeOverrides = EditorView.theme({
    ".cm-comment-gutter": {
      width: "16px",
      padding: "0",
    },
    // No padding / minHeight here on purpose. CM6 sets cell heights
    // inline (`style.height = "<lineHeight>px"`) to match the
    // content's blocks; adding `padding: 1px` to the cell with
    // default `box-sizing: content-box` would push the cell's
    // outer height past the content line height by 2px each, and
    // the cumulative drift across many lines was a ~one-line
    // misalignment that broke hover line attribution.
    ".cm-comment-gutter .cm-gutterElement": {
      display: "flex",
      alignItems: "center",
      justifyContent: "center",
      cursor: "pointer",
    },
    // Marker visibility is controlled by JS (the gutter only emits
    // a marker for the line in `hoverLineField`). No CSS opacity
    // animation: that was the source of the "icon stays after fast
    // mouse move" bug — the icon was mid-fade-out when the next
    // mouseleave fired and never finished disappearing.
    // Defang CM6's default `.cm-tooltip` border + bg so our popup
    // (which has its own bg-popover + shadow + border) doesn't
    // render double-bordered. Targeted via `:has` so other tooltips
    // (autocomplete, lint, etc.) keep their default styling.
    ".cm-tooltip:has(.cm-comment-tooltip-host)": {
      border: "none !important",
      backgroundColor: "transparent !important",
      padding: "0 !important",
      boxShadow: "none !important",
    },
  });

  // ─── precedence wrapping ────────────────────────────────────────

  // Boost the keymap so vim's NORMAL-mode keymap doesn't swallow
  // Mod-Alt-c on the way down. Mod-Alt-* aren't bound by
  // @replit/codemirror-vim today, but Prec.high makes that
  // resilient to future plugin additions.
  return [
    hoverLineField,
    popupField,
    commentGutter,
    hoverPlugin,
    Prec.high(commentKeymap),
    themeOverrides,
  ];
}

// ─── tooltip + react root plumbing ───────────────────────────────

function makeTooltip(
  trigger: PopupTrigger,
  filePath: string,
  sessionId: string,
): Tooltip {
  return {
    pos: trigger.pos,
    above: false,
    arrow: false,
    create(view): TooltipView {
      const dom = document.createElement("div");
      // Marker class so the theme override can find this specific
      // tooltip (vs. autocomplete/lint tooltips that should keep
      // their default border + background).
      dom.className = "cm-comment-tooltip-host";

      const root: Root = createRoot(dom);

      function close(): void {
        view.dispatch({ effects: clearPopupEffect.of() });
      }

      function submit(text: string): void {
        const trimmed = text.trim();
        if (trimmed.length > 0) {
          const anchor: DiffCommentAnchor =
            trigger.kind === "line"
              ? { path: filePath, surface: "code", line: trigger.line }
              : {
                  path: filePath,
                  surface: "code",
                  lineRange: trigger.lineRange,
                  selectionText: trigger.selectionText,
                };
          addComment(sessionId, { anchor, text: trimmed });
        }
        close();
      }

      root.render(
        <CommentPopup
          anchorLabel={formatAnchor(filePath, trigger)}
          onSubmit={submit}
          onCancel={close}
        />,
      );

      return {
        dom,
        // Defer one microtask so React doesn't warn about an
        // unmount that lands inside CM6's transaction processing.
        destroy(): void {
          queueMicrotask(() => root.unmount());
        },
      };
    },
  };
}

function formatAnchor(filePath: string, trigger: PopupTrigger): string {
  const slash = filePath.lastIndexOf("/");
  const basename = slash >= 0 ? filePath.slice(slash + 1) : filePath;
  if (trigger.kind === "line") {
    return `${basename}:${trigger.line}`;
  }
  const [start, end] = trigger.lineRange;
  return start === end
    ? `${basename}:${start}`
    : `${basename}:${start}-${end}`;
}

// ─── shared spacer ───────────────────────────────────────────────
//
// A no-op marker used as the gutter's `initialSpacer` so the column
// has a stable width even when no line is hovered. Without it the
// editor visually jumps the first time a "+" appears under the
// cursor, since that's when CM6 first measures a non-empty cell.

class SpacerGutterMarker extends GutterMarker {
  override eq(other: GutterMarker): boolean {
    return other instanceof SpacerGutterMarker;
  }
  override toDOM(): Node {
    const span = document.createElement("span");
    span.style.display = "inline-block";
    span.style.width = "14px";
    span.style.height = "14px";
    return span;
  }
}

const SPACER_MARKER = new SpacerGutterMarker();

// Re-export effects for callers that want to drive the popup
// programmatically (e.g. tests or future "comment on selection
// from a context menu" surfaces).
export const _internal = {
  setPopupEffect,
  clearPopupEffect,
  setHoverLineEffect,
  hoverLineField,
};
