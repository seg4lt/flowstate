import { describe, expect, it } from "vitest";
import {
  globToRegex,
  matchesPickerQuery,
  parsePickerQuery,
} from "./glob";

describe("globToRegex", () => {
  it("anchors by default", () => {
    const re = globToRegex("*.ts");
    expect(re.test("foo.ts")).toBe(true);
    expect(re.test("foo.ts.bak")).toBe(false);
  });

  it("supports `**` for cross-segment matching", () => {
    const re = globToRegex("src/**/foo.ts");
    expect(re.test("src/foo.ts")).toBe(true);
    expect(re.test("src/a/b/foo.ts")).toBe(true);
    expect(re.test("other/foo.ts")).toBe(false);
  });

  it("`anchored: false` matches anywhere in the string", () => {
    const re = globToRegex("src/**/code", { anchored: false });
    expect(re.test("apps/flowstate/src/components/code")).toBe(true);
    expect(re.test("apps/lib/code")).toBe(false); // no `src/` segment before
  });

  it("escapes regex metacharacters in literal segments", () => {
    const re = globToRegex("foo+bar.baz");
    expect(re.test("foo+bar.baz")).toBe(true);
    expect(re.test("foo+barXbaz")).toBe(false);
  });
});

describe("parsePickerQuery + matchesPickerQuery", () => {
  function match(q: string, path: string): boolean {
    return matchesPickerQuery(path, parsePickerQuery(q));
  }

  it("single-token query is substring against the full path", () => {
    expect(match("tabs", "src/components/tabs.tsx")).toBe(true);
    expect(match("tabs", "src/header.tsx")).toBe(false);
  });

  it("space splits into folder + file (basename) filters", () => {
    expect(match("src tabs.ts", "apps/flowstate/src/components/tabs.tsx")).toBe(true);
    expect(match("src tabs.ts", "apps/flowstate/src/header.tsx")).toBe(false);
    expect(match("src tabs", "apps/lib/tabs.ts")).toBe(false); // no `src` folder
  });

  it("folder pattern matches anywhere in the directory portion", () => {
    expect(match("lib/api git.ts", "apps/foo/lib/api/git.ts")).toBe(true);
    expect(match("lib/api git.ts", "apps/foo/lib/git.ts")).toBe(false);
  });

  it("supports glob in the folder filter (unanchored)", () => {
    expect(match("**/code Header.tsx", "apps/src/components/code/Header.tsx")).toBe(true);
    expect(match("**/code Header.tsx", "apps/src/components/Header.tsx")).toBe(false);
  });

  it("supports glob in the file filter (anchored to basename)", () => {
    expect(match("src *.tsx", "apps/src/components/tabs.tsx")).toBe(true);
    expect(match("src *.tsx", "apps/src/components/tabs.ts")).toBe(false);
  });

  it("comma-joined alternatives are OR", () => {
    expect(match("src tabs.ts, lib header", "apps/lib/components/header.ts")).toBe(true);
    expect(match("src tabs.ts, lib header", "apps/src/components/tabs.ts")).toBe(true);
    expect(match("src tabs.ts, lib header", "apps/util/foo.ts")).toBe(false);
  });

  it("file at the project root has empty dir, so folder-scoped queries skip it", () => {
    expect(match("src tabs", "tabs.ts")).toBe(false);
    expect(match("tabs", "tabs.ts")).toBe(true);
  });

  it("treats trailing / leading whitespace and blank chunks as no-ops", () => {
    const parsed = parsePickerQuery("  ,  , ,  ");
    expect(parsed.alternatives).toHaveLength(0);
    expect(matchesPickerQuery("anything", parsed)).toBe(true);
  });
});
