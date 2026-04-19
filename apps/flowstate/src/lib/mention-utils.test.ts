import { describe, expect, it } from "vitest";
import {
  applyMentionPick,
  detectMentionContext,
  rankFileMatches,
} from "./mention-utils";

describe("detectMentionContext", () => {
  it("detects a mention at the start of the value", () => {
    expect(detectMentionContext("@foo", 4)).toEqual({ atIndex: 0, query: "foo" });
  });

  it("detects a mention after whitespace", () => {
    expect(detectMentionContext("hello @sr", 9)).toEqual({
      atIndex: 6,
      query: "sr",
    });
  });

  it("returns null when the caret is past the token (space after)", () => {
    expect(detectMentionContext("@foo bar", 8)).toBeNull();
  });

  it("returns null when caret is at 0", () => {
    expect(detectMentionContext("@foo", 0)).toBeNull();
  });

  it("returns an empty query for bare `@`", () => {
    expect(detectMentionContext("@", 1)).toEqual({ atIndex: 0, query: "" });
  });

  it("allows `@` mid-word (per 'trigger anywhere' design)", () => {
    // Per design decision: no email/handle guard. `foo@bar` still
    // triggers on `bar`.
    expect(detectMentionContext("foo@bar", 7)).toEqual({
      atIndex: 3,
      query: "bar",
    });
  });

  it("handles caret in the middle of a token", () => {
    // Value "hello @src/fo" with caret after "fo".
    expect(detectMentionContext("hello @src/fo", 13)).toEqual({
      atIndex: 6,
      query: "src/fo",
    });
  });

  it("returns null when the token doesn't start with @", () => {
    expect(detectMentionContext("hello world", 11)).toBeNull();
  });
});

describe("rankFileMatches", () => {
  const files = [
    "apps/flowstate/src/components/chat/chat-input.tsx",
    "apps/flowstate/src/components/chat/chat-view.tsx",
    "apps/flowstate/src/lib/chat-utils.ts",
    "apps/flowstate/src/routes/chat.tsx",
    "apps/flowstate/src/lib/mention-utils.ts",
    "docs/readme.md",
  ];

  it("returns the first entries when query is empty", () => {
    expect(rankFileMatches(files, "")).toEqual(files);
  });

  it("caps results at 50", () => {
    const many = Array.from({ length: 100 }, (_, i) => `dir/file-${i}.ts`);
    expect(rankFileMatches(many, "").length).toBe(50);
  });

  it("prefers basename matches over path-only matches", () => {
    const input = [
      "zzz/foo-dir/readme.md", // path segment starts with "foo" → score 2
      "a/b/foo.ts", // basename starts with "foo" → score 1
      "a/b/nothing.ts", // no match
    ];
    const result = rankFileMatches(input, "foo");
    // The basename-prefix match (score 1) must come before the
    // path-prefix match (score 2), regardless of input order.
    expect(result).toEqual(["a/b/foo.ts", "zzz/foo-dir/readme.md"]);
  });

  it("is case-insensitive", () => {
    expect(rankFileMatches(files, "CHAT-INPUT")[0]).toBe(
      "apps/flowstate/src/components/chat/chat-input.tsx",
    );
  });

  it("matches on path segments", () => {
    const result = rankFileMatches(files, "mention");
    expect(result).toContain("apps/flowstate/src/lib/mention-utils.ts");
  });

  it("drops non-matches", () => {
    expect(rankFileMatches(files, "zzzzzz")).toEqual([]);
  });
});

describe("applyMentionPick", () => {
  it("replaces a bare `@` with the picked path + trailing space", () => {
    expect(applyMentionPick("@", 0, 1, "src/foo.ts")).toEqual({
      value: "@src/foo.ts ",
      caret: 12,
    });
  });

  it("replaces a partial mention at end of string", () => {
    const out = applyMentionPick("hello @sr", 6, 9, "src/lib/foo.ts");
    expect(out.value).toBe("hello @src/lib/foo.ts ");
    expect(out.caret).toBe(22);
  });

  it("preserves trailing content after the token and inserts a space", () => {
    // Value "pre@sr" with caret at end (no trailing whitespace) — we
    // should inject a trailing space so the user can keep typing.
    const out = applyMentionPick("pre@sr", 3, 6, "src/foo.ts");
    expect(out.value).toBe("pre@src/foo.ts ");
    // "pre" (3) + "@src/foo.ts " (12) = 15.
    expect(out.caret).toBe(15);
  });

  it("does not double-insert a space when one is already present", () => {
    // Caret just after "@sr", next char is a space already.
    const out = applyMentionPick("hello @sr bye", 6, 9, "src/foo.ts");
    // Token replaced but no extra space injected before "bye".
    expect(out.value).toBe("hello @src/foo.ts bye");
    // Caret lands right after the path (before the existing space).
    // "hello " (6) + "@src/foo.ts" (11) = 17.
    expect(out.caret).toBe(17);
  });
});
