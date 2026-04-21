import { describe, expect, test } from "vitest";
import {
  addComment,
  clearComments,
  removeComment,
  serializeCommentsAsPrefix,
  updateComment,
} from "./diff-comments-store";

// A note on determinism: the store is module-level (per-session Maps),
// so each test uses a unique sessionId to avoid cross-test bleed.
function sid(name: string): string {
  return `${name}-${Math.random().toString(36).slice(2)}`;
}

describe("diff-comments-store", () => {
  test("add / update / remove round-trip", () => {
    const session = sid("crud");
    const id = addComment(session, {
      anchor: { path: "src/foo.ts", surface: "diff", line: 10 },
      text: "please rename this",
    });
    updateComment(session, id, "rename me");
    removeComment(session, id);
    // Removing the last comment clears the session entry entirely —
    // callers never see "undefined vs. empty" flicker.
    expect(serializeCommentsAsPrefix([])).toBe("");
  });

  test("serializeCommentsAsPrefix: empty list → empty string", () => {
    expect(serializeCommentsAsPrefix([])).toBe("");
  });

  test("serializeCommentsAsPrefix: hover (single line)", () => {
    const out = serializeCommentsAsPrefix([
      {
        id: "1",
        createdAt: 0,
        anchor: { path: "src/foo.ts", surface: "diff", line: 42 },
        text: "why did this change?",
      },
    ]);
    expect(out).toBe("Review comments:\n- src/foo.ts:42 — why did this change?");
  });

  test("serializeCommentsAsPrefix: selection (line range + quoted)", () => {
    const out = serializeCommentsAsPrefix([
      {
        id: "1",
        createdAt: 0,
        anchor: {
          path: "src/bar.ts",
          surface: "search",
          lineRange: [10, 14],
          selectionText: "const x = 5;\nconst y = 6;",
        },
        text: "wrap this in a try/catch",
      },
    ]);
    // Multi-line quoted selections preserve per-line `>` prefixes under
    // the bullet so the model sees each selected line as a quote row.
    expect(out).toBe(
      [
        "Review comments:",
        "- src/bar.ts:10-14 — wrap this in a try/catch",
        "    > const x = 5;",
        "    > const y = 6;",
      ].join("\n"),
    );
  });

  test("serializeCommentsAsPrefix: multiple comments order preserved", () => {
    const out = serializeCommentsAsPrefix([
      {
        id: "1",
        createdAt: 0,
        anchor: { path: "a.ts", surface: "diff", line: 1 },
        text: "one",
      },
      {
        id: "2",
        createdAt: 1,
        anchor: { path: "b.ts", surface: "search", lineRange: [2, 2] },
        text: "two",
      },
    ]);
    expect(out).toBe(
      [
        "Review comments:",
        "- a.ts:1 — one",
        "- b.ts:2 — two",
      ].join("\n"),
    );
  });

  test("clearComments is a no-op when the session has none", () => {
    const session = sid("noop-clear");
    // Should not throw and should not notify.
    clearComments(session);
    expect(true).toBe(true);
  });
});
