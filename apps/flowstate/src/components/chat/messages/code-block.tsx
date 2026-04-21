import * as React from "react";
import type {
  HighlighterCore,
  ThemeRegistrationAny,
} from "shiki/core";
import pierreDark from "@pierre/theme/pierre-dark";
import pierreLight from "@pierre/theme/pierre-light";
import { useTheme } from "@/hooks/use-theme";
import { CopyButton } from "./copy-button";

// Languages preloaded into the highlighter. We import grammars
// individually via `shiki/langs/<name>` (fine-grained bundle)
// instead of `import("shiki")` (full bundle). Full-bundle pulls
// every grammar Shiki ships — several MB parsed — into the app's
// graph even though we'd only register ~16. Fine-grained means
// the app chunk only carries what's listed here.
//
// This set covers ~95% of code blocks we see in chat. Rarer
// languages (c/cpp/ruby/php/sql) fall back to "text" — the block
// still renders, just without per-token colors. If a user
// consistently hits an uncommon language, add an import below
// and push it into PRELOAD_LANGS.
const PRELOAD_LANGS = [
  { name: "typescript", load: () => import("shiki/langs/typescript.mjs") },
  { name: "tsx", load: () => import("shiki/langs/tsx.mjs") },
  { name: "javascript", load: () => import("shiki/langs/javascript.mjs") },
  { name: "jsx", load: () => import("shiki/langs/jsx.mjs") },
  { name: "rust", load: () => import("shiki/langs/rust.mjs") },
  { name: "python", load: () => import("shiki/langs/python.mjs") },
  { name: "go", load: () => import("shiki/langs/go.mjs") },
  { name: "bash", load: () => import("shiki/langs/bash.mjs") },
  { name: "shell", load: () => import("shiki/langs/shellscript.mjs") },
  { name: "json", load: () => import("shiki/langs/json.mjs") },
  { name: "html", load: () => import("shiki/langs/html.mjs") },
  { name: "css", load: () => import("shiki/langs/css.mjs") },
  { name: "markdown", load: () => import("shiki/langs/markdown.mjs") },
  { name: "yaml", load: () => import("shiki/langs/yaml.mjs") },
  { name: "toml", load: () => import("shiki/langs/toml.mjs") },
  { name: "java", load: () => import("shiki/langs/java.mjs") },
] as const;

// Set of lang names for O(1) "is this language supported?" checks
// inside the render hot path. Built once from PRELOAD_LANGS above
// so drift between the two stays impossible.
const PRELOAD_LANG_NAMES: ReadonlySet<string> = new Set(
  PRELOAD_LANGS.map((l) => l.name),
);

// Tiny snippet used to force the first tokenize + regex JIT pass.
// `createHighlighterCore` registers grammars eagerly, but Oniguruma
// still compiles its regex bytecode lazily on first match. One
// throwaway `codeToHtml` during boot idle time moves that cost off
// the render path so the first real <CodeBlock> render is a cache
// hit.
const PREWARM_SNIPPET = "const x = 1;";
const PREWARM_LANG = "typescript";

// Both themes are bundled into the singleton highlighter so swapping
// between light and dark on theme toggle is a synchronous re-highlight
// (no extra grammar/theme load). Pierre themes are imported as theme
// objects to match the look of diff blocks (@pierre/diffs), which render
// with pierre-light / pierre-dark — so every code surface in the app
// shares one palette.
const LIGHT_THEME = pierreLight as unknown as ThemeRegistrationAny;
const DARK_THEME = pierreDark as unknown as ThemeRegistrationAny;

// Singleton highlighter promise. The first <CodeBlock> on the page
// triggers the dynamic import of shiki/core + the Oniguruma WASM +
// each registered grammar, every subsequent block reuses the same
// instance synchronously.
//
// We use `createHighlighterCore` (not the default `createHighlighter`
// from `shiki`) so only the grammars we actually register land in
// the bundle. The engine is Oniguruma WASM — same engine the worker
// pool uses with `preferredHighlighter: "shiki-wasm"` — so tokenize
// timings are consistent between main-thread code blocks and worker-
// based diffs, and work identically in WKWebView (JavaScriptCore's
// regex JIT is slower than V8's, which is why the default JS regex
// engine underperforms in Tauri).
let highlighterPromise: Promise<HighlighterCore> | null = null;

function getHighlighter(): Promise<HighlighterCore> {
  if (!highlighterPromise) {
    highlighterPromise = (async () => {
      const [{ createHighlighterCore }, { createOnigurumaEngine }, ...grammars] =
        await Promise.all([
          import("shiki/core"),
          import("shiki/engine/oniguruma"),
          ...PRELOAD_LANGS.map(({ load }) => load()),
        ]);
      return createHighlighterCore({
        themes: [LIGHT_THEME, DARK_THEME],
        // Each grammar module default-exports a `LanguageRegistration[]`
        // which is exactly what `langs` expects — no unwrapping.
        langs: grammars.map((g) => g.default),
        engine: createOnigurumaEngine(import("shiki/wasm")),
      });
    })();
  }
  return highlighterPromise;
}

// Kick off the highlighter init + one tokenize pass. Safe to call
// multiple times; the singleton promise dedupes the real work and
// the prewarm step is a no-op after the first success. Intended to
// be called from main.tsx at app boot on an idle callback so the
// first real <CodeBlock> render doesn't pay the cold-start cost.
//
// Swallows errors — if prewarm fails (e.g. the dynamic import
// 404s in a dev environment), the lazy path in CodeBlockInner will
// surface the error when the user actually hits a code block.
let prewarmedOnce = false;
export function prewarmCodeBlockHighlighter(): void {
  if (prewarmedOnce) return;
  prewarmedOnce = true;
  getHighlighter()
    .then((highlighter) => {
      try {
        highlighter.codeToHtml(PREWARM_SNIPPET, {
          lang: PREWARM_LANG,
          theme: DARK_THEME,
        });
      } catch {
        /* prewarm is best-effort */
      }
    })
    .catch(() => {
      // Reset so a later real render can retry the import.
      prewarmedOnce = false;
    });
}

interface CodeBlockProps {
  code: string;
  language: string | undefined;
}

function CodeBlockInner({ code, language }: CodeBlockProps) {
  const { resolvedTheme } = useTheme();
  const [html, setHtml] = React.useState<string | null>(null);

  React.useEffect(() => {
    let cancelled = false;
    const theme = resolvedTheme === "dark" ? DARK_THEME : LIGHT_THEME;
    getHighlighter()
      .then((highlighter) => {
        if (cancelled) return;
        // `getLoadedLanguages()` would also work but allocates an
        // array every render; the PRELOAD_LANG_NAMES Set is O(1)
        // and allocated once at module init. Unsupported or unset
        // language → fall back to "text" and render uncolored — the
        // block still appears, just without grammar colors.
        const lang =
          language && PRELOAD_LANG_NAMES.has(language) ? language : "text";
        try {
          const result = highlighter.codeToHtml(code, {
            lang,
            theme,
          });
          setHtml(result);
        } catch (err) {
          console.error("[shiki] highlight error:", err);
          setHtml(null);
        }
      })
      .catch((err) => {
        console.error("[shiki] highlighter init failed:", err);
      });
    return () => {
      cancelled = true;
    };
  }, [code, language, resolvedTheme]);

  // Copy button floats in the top-right of the block. Background +
  // backdrop-blur so it stays legible on both the muted fallback
  // and shiki's theme-specific block colors without needing to
  // branch on theme.
  const copyButton = (
    <CopyButton
      text={code}
      title="Copy code"
      label="Copied code"
      className="absolute right-1.5 top-1.5 z-10 bg-background/70 opacity-0 backdrop-blur transition-opacity group-hover:opacity-100 focus-visible:opacity-100"
    />
  );

  if (html === null) {
    // Plain fallback while shiki initializes (one-time, ~100-300ms cold)
    // or for unsupported languages. Matches the previous code block look.
    return (
      <div className="group relative mb-3 last:mb-0">
        {copyButton}
        <pre className="overflow-x-auto rounded-md border border-border bg-muted p-3 font-mono text-xs">
          <code className="font-mono">{code}</code>
        </pre>
      </div>
    );
  }

  // Shiki emits <pre class="shiki ..." style="background-color:...">
  // <code>...</code></pre>. Wrap in our own div for the rounded border
  // and overflow handling, force tighter padding via arbitrary variant.
  return (
    <div className="group relative mb-3 overflow-hidden rounded-md border border-border text-xs last:mb-0">
      {copyButton}
      <div
        className="overflow-x-auto [&>pre]:!p-3 [&>pre]:overflow-x-auto"
        dangerouslySetInnerHTML={{ __html: html }}
      />
    </div>
  );
}

// Memoize on (code, language). Each block in a stable markdown doc has
// the same content/language across renders, so re-renders of the parent
// MarkdownContent skip re-highlighting unchanged blocks.
export const CodeBlock = React.memo(
  CodeBlockInner,
  (prev, next) => prev.code === next.code && prev.language === next.language,
);
