import * as React from "react";
import type { HighlighterCore } from "shiki/core";
import { useTheme } from "@/hooks/use-theme";
import {
  DARK_THEME,
  LIGHT_THEME,
  ensureLanguageLoaded,
  getHighlighter,
} from "@/lib/shiki-singleton";
import { CopyButton } from "./copy-button";

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
    (async () => {
      let highlighter: HighlighterCore;
      try {
        highlighter = await getHighlighter();
      } catch (err) {
        console.error("[shiki] highlighter init failed:", err);
        return;
      }
      if (cancelled) return;

      // Resolve the language: preloaded → tokenize immediately.
      // Lazy-loadable → render plain text now, load grammar in the
      // background, then swap to colored. Unknown → plain text
      // forever for this block.
      const hasLang = language
        ? await ensureLanguageLoaded(highlighter, language)
        : false;
      if (cancelled) return;

      const lang = hasLang && language ? language : "text";
      try {
        const result = highlighter.codeToHtml(code, { lang, theme });
        if (!cancelled) setHtml(result);
      } catch (err) {
        console.error("[shiki] highlight error:", err);
        if (!cancelled) setHtml(null);
      }
    })();
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
