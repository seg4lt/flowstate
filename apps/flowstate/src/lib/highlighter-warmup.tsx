import * as React from "react";
import { PatchDiff } from "@pierre/diffs/react";

// Warmup snippets per language. The default @pierre/diffs worker
// uses Shiki's JS regex engine (`createJavaScriptRegexEngine`), and
// V8 JITs each regex pattern lazily on first match. A trivial
// `-a/+b` warmup only JITs a handful of patterns per grammar,
// which leaves most patterns cold until the user opens a real file.
// These snippets exercise the common tokens for each language —
// keywords, strings, numbers, comments, regexes where applicable —
// so the JIT has a realistic warm set when the first real diff lands.
//
// Keep snippets small (≤10 lines) — the warmup runs on every app
// boot and we don't want it to stall low-end machines. Also keep
// them ASCII and free of special chars that might confuse the
// patch parser.
const WARMUP_SNIPPETS: Record<string, { ext: string; before: string; after: string }> = {
  ts: {
    ext: "ts",
    before: `import { foo } from "./x";
function greet(name: string): number {
  const n = 42;
  // comment
  return n;
}`,
    after: `import { foo, bar } from "./x";
function greet(name: string): number {
  const n = 43;
  // updated
  return n + 1;
}`,
  },
  tsx: {
    ext: "tsx",
    before: `export function App({ name }: { name: string }) {
  return <div className="greet">Hello {name}</div>;
}`,
    after: `export function App({ name }: { name: string }) {
  return <div className="greet">Hi {name}!</div>;
}`,
  },
  js: {
    ext: "js",
    before: `const foo = require("./x");
function greet(name) {
  return \`Hello \${name}\`;
}
module.exports = greet;`,
    after: `const foo = require("./x");
function greet(name) {
  return \`Hi \${name}!\`;
}
module.exports = greet;`,
  },
  jsx: {
    ext: "jsx",
    before: `export function App({ name }) {
  return <div className="greet">Hello {name}</div>;
}`,
    after: `export function App({ name }) {
  return <div className="greet">Hi {name}!</div>;
}`,
  },
  python: {
    ext: "py",
    before: `import os

def greet(name: str) -> str:
    # comment
    return f"Hello {name}"`,
    after: `import os
import sys

def greet(name: str) -> str:
    # updated
    return f"Hi {name}!"`,
  },
  rust: {
    ext: "rs",
    before: `use std::collections::HashMap;

pub fn greet(name: &str) -> String {
    let n = 42;
    format!("Hello {}", name)
}`,
    after: `use std::collections::HashMap;

pub fn greet(name: &str) -> String {
    let n = 43;
    format!("Hi {}!", name)
}`,
  },
  go: {
    ext: "go",
    before: `package main

import "fmt"

func Greet(name string) string {
    return fmt.Sprintf("Hello %s", name)
}`,
    after: `package main

import "fmt"

func Greet(name string) string {
    return fmt.Sprintf("Hi %s!", name)
}`,
  },
  json: {
    ext: "json",
    before: `{
  "name": "example",
  "version": "1.0.0",
  "private": true
}`,
    after: `{
  "name": "example",
  "version": "1.0.1",
  "private": true
}`,
  },
  yaml: {
    ext: "yaml",
    before: `name: example
version: 1.0.0
steps:
  - run: build
  - run: test`,
    after: `name: example
version: 1.0.1
steps:
  - run: build
  - run: test
  - run: deploy`,
  },
  bash: {
    ext: "sh",
    before: `#!/usr/bin/env bash
set -euo pipefail

greet() {
  echo "Hello $1"
}
greet "world"`,
    after: `#!/usr/bin/env bash
set -euo pipefail

greet() {
  echo "Hi $1!"
}
greet "world"`,
  },
  md: {
    ext: "md",
    before: `# Title

A paragraph with \`code\` and **bold**.

- item one
- item two`,
    after: `# Title

A paragraph with \`code\`, **bold**, and _italic_.

- item one
- item two
- item three`,
  },
  html: {
    ext: "html",
    before: `<!doctype html>
<html>
  <body><p class="x">Hello</p></body>
</html>`,
    after: `<!doctype html>
<html>
  <body><p class="x">Hi!</p></body>
</html>`,
  },
  css: {
    ext: "css",
    before: `.root {
  color: #333;
  padding: 8px;
}`,
    after: `.root {
  color: #222;
  padding: 12px;
}`,
  },
  toml: {
    ext: "toml",
    before: `[package]
name = "example"
version = "1.0.0"`,
    after: `[package]
name = "example"
version = "1.0.1"`,
  },
  java: {
    ext: "java",
    before: `public class Greeter {
  public String greet(String name) {
    return "Hello " + name;
  }
}`,
    after: `public class Greeter {
  public String greet(String name) {
    return "Hi " + name + "!";
  }
}`,
  },
};

// Build a minimal unified patch for a snippet. Counts lines exactly
// so PatchDiff's parser accepts the hunk header as valid. Lines are
// emitted as a sequence of "-" (old) followed by "+" (new), which
// maximises tokenization coverage: every line in the snippet is
// tokenized (once for the old side, once for the new), exercising
// the full set of token patterns in the grammar.
function buildPatch(name: string, ext: string, before: string, after: string): string {
  const beforeLines = before.split("\n");
  const afterLines = after.split("\n");
  return [
    `--- a/${name}.${ext}`,
    `+++ b/${name}.${ext}`,
    `@@ -1,${beforeLines.length} +1,${afterLines.length} @@`,
    ...beforeLines.map((l) => `-${l}`),
    ...afterLines.map((l) => `+${l}`),
  ].join("\n");
}

// Repeat count. The pool has up to 8 workers and dispatch is
// load-balanced rather than strictly round-robin, so we render each
// snippet multiple times to blanket the pool. 3× langs × ~16 langs
// = 48 tasks, which tends to touch every worker and JIT the most
// common regex paths in each grammar. Memory cost is trivial and
// short-lived — warmup unmounts after the timeout below.
const WARMUP_REPEATS = 3;

// Offscreen container. `position: fixed` keeps it out of flow, zero
// size + `overflow: hidden` prevents any pixels from painting, and
// `pointer-events: none` + `aria-hidden` keep it out of both the hit
// test and the a11y tree. The cache lives in the worker pool, not
// in this React subtree, so unmounting doesn't lose anything.
const HIDDEN_STYLE: React.CSSProperties = {
  position: "fixed",
  top: 0,
  left: 0,
  width: 0,
  height: 0,
  overflow: "hidden",
  pointerEvents: "none",
  opacity: 0,
  visibility: "hidden",
};

// Keep the warmup subtree mounted long enough for every dispatched
// task to complete inside a worker. On a cold machine each task
// takes ~50-200 ms, and with ~48 tasks across 8 workers the wall
// clock is ~1-2 s. 8 s is comfortable headroom — the component
// self-unmounts after to stop sitting in the React tree.
const WARMUP_VISIBLE_MS = 8_000;

export function HighlighterWarmup() {
  const [active, setActive] = React.useState(true);

  React.useEffect(() => {
    // Wait for the browser to settle first paint before firing off
    // the warmup — we don't want to steal main-thread cycles from
    // the initial route render. `requestIdleCallback` when
    // available, `setTimeout` as the fallback for Safari/WebKit-
    // based Tauri webviews on older macOS.
    type IdleWindow = Window & {
      requestIdleCallback?: (cb: () => void) => number;
    };
    const w = window as IdleWindow;
    let timer: number | null = null;
    let idleHandle: number | null = null;

    const startUnmountTimer = () => {
      timer = window.setTimeout(() => {
        setActive(false);
      }, WARMUP_VISIBLE_MS);
    };

    if (typeof w.requestIdleCallback === "function") {
      idleHandle = w.requestIdleCallback(() => startUnmountTimer());
    } else {
      // 250ms after mount — long enough for the route to paint,
      // short enough that the warmup still lands before the user
      // is likely to open a diff.
      timer = window.setTimeout(() => {
        startUnmountTimer();
      }, 250);
    }

    return () => {
      if (timer !== null) window.clearTimeout(timer);
      if (idleHandle !== null) {
        type CancelIdle = Window & {
          cancelIdleCallback?: (h: number) => void;
        };
        (window as CancelIdle).cancelIdleCallback?.(idleHandle);
      }
    };
  }, []);

  // Memoise the patch list so a re-render of <HighlighterWarmup />
  // (shouldn't happen — the parent is stable — but StrictMode
  // double-invokes effects in dev) doesn't rebuild the patches or
  // remount the PatchDiff children.
  const patches = React.useMemo(() => {
    const out: Array<{ key: string; patch: string }> = [];
    for (let rep = 0; rep < WARMUP_REPEATS; rep++) {
      for (const [lang, s] of Object.entries(WARMUP_SNIPPETS)) {
        out.push({
          key: `${lang}-${rep}`,
          patch: buildPatch(`warmup-${rep}`, s.ext, s.before, s.after),
        });
      }
    }
    return out;
  }, []);

  if (!active) return null;

  return (
    <div aria-hidden style={HIDDEN_STYLE}>
      {patches.map(({ key, patch }) => (
        <PatchDiff
          key={key}
          patch={patch}
          options={{
            diffStyle: "unified",
            theme: { dark: "pierre-dark", light: "pierre-light" },
            themeType: "dark",
            diffIndicators: "classic",
            overflow: "scroll",
            disableFileHeader: true,
            maxLineDiffLength: 2_000,
            tokenizeMaxLineLength: 5_000,
          }}
        />
      ))}
    </div>
  );
}
