import * as React from "react";
import type { BundledLanguage, Highlighter } from "shiki";

// Languages preloaded into the highlighter. Adding more is free at
// runtime cost — shiki bundles the grammars statically. Trim if the
// chat shows code in a narrower set in practice.
const PRELOAD_LANGS: BundledLanguage[] = [
  "typescript",
  "tsx",
  "javascript",
  "jsx",
  "rust",
  "python",
  "go",
  "bash",
  "shell",
  "json",
  "html",
  "css",
  "sql",
  "markdown",
  "yaml",
  "toml",
  "c",
  "cpp",
  "java",
  "ruby",
  "php",
];

const THEME = "github-dark";

// Singleton highlighter promise. The first <CodeBlock> on the page
// triggers the dynamic import + grammar load (~100-300ms cold), every
// subsequent block reuses the same instance synchronously.
let highlighterPromise: Promise<Highlighter> | null = null;

function getHighlighter(): Promise<Highlighter> {
  if (!highlighterPromise) {
    highlighterPromise = (async () => {
      const { createHighlighter } = await import("shiki");
      return createHighlighter({
        themes: [THEME],
        langs: PRELOAD_LANGS,
      });
    })();
  }
  return highlighterPromise;
}

interface CodeBlockProps {
  code: string;
  language: string | undefined;
}

function CodeBlockInner({ code, language }: CodeBlockProps) {
  const [html, setHtml] = React.useState<string | null>(null);

  React.useEffect(() => {
    let cancelled = false;
    getHighlighter()
      .then((highlighter) => {
        if (cancelled) return;
        const loaded = highlighter.getLoadedLanguages();
        const lang =
          language && loaded.includes(language as BundledLanguage)
            ? (language as BundledLanguage)
            : ("text" as BundledLanguage);
        try {
          const result = highlighter.codeToHtml(code, {
            lang,
            theme: THEME,
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
  }, [code, language]);

  if (html === null) {
    // Plain fallback while shiki initializes (one-time, ~100-300ms cold)
    // or for unsupported languages. Matches the previous code block look.
    return (
      <pre className="mb-3 overflow-x-auto rounded-md border border-border bg-muted p-3 font-mono text-xs last:mb-0">
        <code className="font-mono">{code}</code>
      </pre>
    );
  }

  // Shiki emits <pre class="shiki ..." style="background-color:...">
  // <code>...</code></pre>. Wrap in our own div for the rounded border
  // and overflow handling, force tighter padding via arbitrary variant.
  return (
    <div
      className="mb-3 overflow-x-auto rounded-md border border-border text-xs last:mb-0 [&>pre]:!p-3 [&>pre]:overflow-x-auto"
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}

// Memoize on (code, language). Each block in a stable markdown doc has
// the same content/language across renders, so re-renders of the parent
// MarkdownContent skip re-highlighting unchanged blocks.
export const CodeBlock = React.memo(
  CodeBlockInner,
  (prev, next) => prev.code === next.code && prev.language === next.language,
);
