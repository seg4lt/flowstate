import { describe, expect, it } from "vitest";
import { fuzzyScore, fuzzyScoreAuto, FUZZY_MIN_QUERY_LEN } from "./fuzzy";

// `fuzzyScore` always takes a lowercase query — the caller (rankFileMatchesFuzzy)
// pre-lowercases. Mirror that contract here so tests reflect real usage.

describe("fuzzyScore — subsequence (existing behavior, regression guard)", () => {
  it("matches every-char-in-order, scattered through the path", () => {
    expect(fuzzyScore("src/views/tabs-view.tsx", "tbsv")).toBeGreaterThan(0);
  });

  it("returns 0 when chars cannot be found in order", () => {
    expect(fuzzyScore("src/FooBar.ts", "xyz")).toBe(0);
  });

  it("returns 0 when query is longer than the path", () => {
    expect(fuzzyScore("a.ts", "abcdef")).toBe(0);
  });

  it("treats an empty query as a trivial match", () => {
    expect(fuzzyScore("anything", "")).toBe(1);
  });

  it("ranks basename-prefix hits above mid-path hits for the same query", () => {
    const prefix = fuzzyScore("src/tabs-view.tsx", "tabs");
    const mid = fuzzyScore("src/stable-tabs-meta.ts", "tabs");
    expect(prefix).toBeGreaterThan(mid);
  });
});

describe("fuzzyScore — IntelliJ/Zed-style acronym matching", () => {
  it("matches a CamelCase basename's word-initials prefix", () => {
    // Foo|Name|Strategy|Controller|ts → initials 'fnsct'.
    const score = fuzzyScore(
      "src/foo/FooNameStrategyController.ts",
      "fnsc",
    );
    expect(score).toBeGreaterThan(0);
  });

  it("ranks a basename-acronym hit above a noisy substring hit", () => {
    // Same query, two competing files: the camel file is the user's intent.
    const camel = fuzzyScore(
      "src/foo/FooNameStrategyController.ts",
      "fnsc",
    );
    const literal = fuzzyScore("src/fnsc-config.ts", "fnsc");
    expect(camel).toBeGreaterThan(literal);
  });

  it("matches kebab-case word initials too (not just camelCase)", () => {
    // tabs|view|tsx → initials 'tvt'. Query 'tv' is a 2-char prefix.
    const score = fuzzyScore("src/views/tabs-view.tsx", "tv");
    expect(score).toBeGreaterThan(0);
    // …and beats any 2-char subsequence on a competing file.
    const subseq = fuzzyScore("src/tooling/very-long.ts", "tv");
    expect(score).toBeGreaterThan(subseq);
  });

  it("falls back to path-level initials when the basename's don't cover the query", () => {
    // components|code|lib|fuzzy|utils|ts → 'cclfut'. 'cclf' matches via path.
    const score = fuzzyScore(
      "components/code/lib/fuzzy-utils.ts",
      "cclf",
    );
    expect(score).toBeGreaterThan(0);
  });

  it("a basename acronym hit outranks a path acronym hit on the same query", () => {
    // Construct paths where one matches via basename initials and the other
    // only via path initials.
    const basenameHit = fuzzyScore("a/b/FooBar.ts", "fb");
    const pathHit = fuzzyScore("foo/bar/x.ts", "fb");
    expect(basenameHit).toBeGreaterThan(pathHit);
  });

  it("requires a *prefix* match — mid-acronym does NOT trigger the acronym tier", () => {
    // 'nsc' is a substring of 'fnsct' but not a prefix → falls through to
    // subsequence scoring, which still matches but at a much lower score.
    const prefix = fuzzyScore("src/FooNameStrategyController.ts", "fnsc");
    const midword = fuzzyScore("src/FooNameStrategyController.ts", "nsc");
    expect(prefix).toBeGreaterThan(midword);
    // Mid-word still > 0 because every char appears in order.
    expect(midword).toBeGreaterThan(0);
  });

  it("longer acronym queries beat shorter ones (score scales with qlen)", () => {
    const four = fuzzyScore("src/FooNameStrategyController.ts", "fnsc");
    const two = fuzzyScore("src/FooNameStrategyController.ts", "fn");
    expect(four).toBeGreaterThan(two);
  });

  it("does not engage the acronym tier for single-char queries", () => {
    // qlen < FUZZY_MIN_QUERY_LEN: the pre-pass is skipped, and we fall
    // through to subsequence which scores a single-char match modestly.
    expect(FUZZY_MIN_QUERY_LEN).toBe(2);
    const score = fuzzyScore("src/FooBar.ts", "f");
    // Subsequence score for one boundary basename-prefix hit ≈ 79; well below
    // any acronym constant (would otherwise be 2000).
    expect(score).toBeGreaterThan(0);
    expect(score).toBeLessThan(500);
  });
});

describe("fuzzyScoreAuto", () => {
  it("lowercases the query so uppercase initials match camelCase basenames", () => {
    expect(fuzzyScoreAuto("src/FooBar.ts", "FB")).toBeGreaterThan(0);
  });

  it("agrees with fuzzyScore on already-lowercase queries", () => {
    expect(fuzzyScoreAuto("src/FooBar.ts", "fb")).toBe(
      fuzzyScore("src/FooBar.ts", "fb"),
    );
  });
});
