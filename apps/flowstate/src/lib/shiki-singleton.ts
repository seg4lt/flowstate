// Shared Shiki highlighter — single instance for the whole app.
//
// The chat code blocks (`code-block.tsx`) and the file-viewer code
// editor (`code-editor.tsx`) both call `getHighlighter()` and
// reuse the same compiled grammars + Oniguruma WASM state. That
// keeps memory bounded — Shiki's per-language regex compile is the
// expensive part — and gives every code surface in the app the
// same theme palette (pierre-light / pierre-dark).
//
// Used by:
//  * `apps/flowstate/src/components/chat/messages/code-block.tsx`
//  * `apps/flowstate/src/components/code/code-editor.tsx`
//
// Diff rendering still goes through `@pierre/diffs`'s own worker
// pool (separate Shiki instance inside the worker). Only the two
// main-thread surfaces share this singleton.

import type {
  HighlighterCore,
  LanguageRegistration,
  ThemeRegistrationAny,
} from "shiki/core";
import pierreDark from "@pierre/theme/pierre-dark";
import pierreLight from "@pierre/theme/pierre-light";

// Languages preloaded into the highlighter at creation. Kept small
// to keep resident memory low — each grammar adds compiled
// Oniguruma regex state + tokenizer tables (~50-200 KB each). Any
// other language lazily loads on first use via LAZY_LANGS below.
//
// Fine-grained imports (`shiki/langs/<name>.mjs`) instead of
// `import("shiki")` keep the app chunk light: only these specific
// grammars ship in the initial bundle.
const PRELOAD_LANGS = [
  { name: "typescript", load: () => import("shiki/langs/typescript.mjs") },
  { name: "tsx", load: () => import("shiki/langs/tsx.mjs") },
  { name: "java", load: () => import("shiki/langs/java.mjs") },
  { name: "rust", load: () => import("shiki/langs/rust.mjs") },
  { name: "python", load: () => import("shiki/langs/python.mjs") },
  { name: "bash", load: () => import("shiki/langs/bash.mjs") },
  { name: "json", load: () => import("shiki/langs/json.mjs") },
] as const;

// Type of a Shiki language grammar module's default export.
type GrammarModule = { default: LanguageRegistration[] };

// Lazy-loaded grammars. Each entry is a dynamic import — Vite code-
// splits one chunk per unique module specifier, so two entries
// pointing to the same `.mjs` file dedupe automatically. The key
// is whatever the user might type in a code fence (` ```md `,
// ` ```go `, etc.); Shiki's own alias handling covers additional
// forms once the grammar is registered.
//
// Trade-off: a block in one of these langs renders as plain
// uncolored text for ~50-200 ms on first encounter (dynamic import
// + grammar compile), then re-renders colored. Subsequent blocks
// in the same lang are instant. Unseen langs never load — the app
// only pays memory for what the user actually views.
//
// Grow this list when a user commonly hits an uncommon language.
const LAZY_LANGS: Record<string, () => Promise<GrammarModule>> = {
  javascript: () => import("shiki/langs/javascript.mjs"),
  js: () => import("shiki/langs/javascript.mjs"),
  jsx: () => import("shiki/langs/jsx.mjs"),
  go: () => import("shiki/langs/go.mjs"),
  golang: () => import("shiki/langs/go.mjs"),
  markdown: () => import("shiki/langs/markdown.mjs"),
  md: () => import("shiki/langs/markdown.mjs"),
  yaml: () => import("shiki/langs/yaml.mjs"),
  yml: () => import("shiki/langs/yaml.mjs"),
  toml: () => import("shiki/langs/toml.mjs"),
  html: () => import("shiki/langs/html.mjs"),
  css: () => import("shiki/langs/css.mjs"),
  scss: () => import("shiki/langs/scss.mjs"),
  sql: () => import("shiki/langs/sql.mjs"),
  xml: () => import("shiki/langs/xml.mjs"),
  c: () => import("shiki/langs/c.mjs"),
  cpp: () => import("shiki/langs/cpp.mjs"),
  "c++": () => import("shiki/langs/cpp.mjs"),
  csharp: () => import("shiki/langs/csharp.mjs"),
  "c#": () => import("shiki/langs/csharp.mjs"),
  ruby: () => import("shiki/langs/ruby.mjs"),
  rb: () => import("shiki/langs/ruby.mjs"),
  php: () => import("shiki/langs/php.mjs"),
  shell: () => import("shiki/langs/shellscript.mjs"),
  shellscript: () => import("shiki/langs/shellscript.mjs"),
  sh: () => import("shiki/langs/shellscript.mjs"),
  zsh: () => import("shiki/langs/shellscript.mjs"),
  dockerfile: () => import("shiki/langs/dockerfile.mjs"),
  docker: () => import("shiki/langs/dockerfile.mjs"),
  kotlin: () => import("shiki/langs/kotlin.mjs"),
  swift: () => import("shiki/langs/swift.mjs"),
  lua: () => import("shiki/langs/lua.mjs"),
  nix: () => import("shiki/langs/nix.mjs"),
  zig: () => import("shiki/langs/zig.mjs"),
  makefile: () => import("shiki/langs/make.mjs"),
  make: () => import("shiki/langs/make.mjs"),
};

// Both themes are bundled into the singleton highlighter so swapping
// between light and dark on theme toggle is a synchronous re-highlight
// (no extra grammar/theme load). Pierre themes are imported as theme
// objects to match the look of diff blocks (@pierre/diffs), which render
// with pierre-light / pierre-dark — so every code surface in the app
// shares one palette.
export const LIGHT_THEME = pierreLight as unknown as ThemeRegistrationAny;
export const DARK_THEME = pierreDark as unknown as ThemeRegistrationAny;

// Singleton highlighter promise. The first caller (chat block or
// editor) triggers the dynamic import of shiki/core + the Oniguruma
// WASM + each preloaded grammar; every subsequent caller reuses the
// same instance synchronously.
//
// We use `createHighlighterCore` (not the default `createHighlighter`
// from `shiki`) so only the grammars we actually register land in
// the bundle. The engine is Oniguruma WASM — same engine the worker
// pool used to use with `preferredHighlighter: "shiki-wasm"` — giving
// consistent perf in WKWebView where JSC's regex JIT is weaker than V8's.
let highlighterPromise: Promise<HighlighterCore> | null = null;

export function getHighlighter(): Promise<HighlighterCore> {
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

// In-flight / completed lazy-load promises, keyed by the lookup
// name the caller used. Dedupes concurrent requests for the same
// language and makes repeat calls a cheap no-op after success.
const lazyLoadPromises = new Map<string, Promise<void>>();

// Load a grammar into the highlighter if it isn't already there.
// Resolves to `true` if the language can be tokenized afterward,
// `false` if it's truly unsupported (not preloaded, not in
// LAZY_LANGS) — caller should fall back to plain-text rendering.
export async function ensureLanguageLoaded(
  highlighter: HighlighterCore,
  lang: string,
): Promise<boolean> {
  // Preloaded grammars + any lang we've already lazy-loaded show up
  // in getLoadedLanguages() (including Shiki's own aliases like
  // "md" for markdown). Cheap Array.includes — the list stays
  // small for our setup.
  if (highlighter.getLoadedLanguages().includes(lang)) return true;

  const loader = LAZY_LANGS[lang];
  if (!loader) return false;

  let p = lazyLoadPromises.get(lang);
  if (!p) {
    p = loader()
      .then((mod) => highlighter.loadLanguage(mod.default))
      .catch((err) => {
        // Drop the cache entry so a later retry can attempt again.
        lazyLoadPromises.delete(lang);
        throw err;
      });
    lazyLoadPromises.set(lang, p);
  }
  try {
    await p;
    return true;
  } catch (err) {
    console.error(`[shiki] lazy-load failed for "${lang}":`, err);
    return false;
  }
}
