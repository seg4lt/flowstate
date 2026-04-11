import * as React from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { openUrl } from "@tauri-apps/plugin-opener";
import { CodeBlock } from "./code-block";
import { DiffCodeBlock } from "./diff-code-block";

interface MarkdownContentProps {
  content: string;
}

// Open links in the user's default browser via the Tauri opener plugin
// instead of letting them navigate the webview (which is the app shell).
// Falls back to window.open when running in a plain browser (vite dev).
function handleLinkClick(
  event: React.MouseEvent<HTMLAnchorElement>,
  href: string | undefined,
) {
  if (!href) return;
  event.preventDefault();
  if (typeof window !== "undefined" && "__TAURI_INTERNALS__" in window) {
    void openUrl(href);
  } else {
    window.open(href, "_blank", "noopener,noreferrer");
  }
}

const components: Components = {
  p({ children }) {
    return <p className="mb-3 leading-relaxed last:mb-0">{children}</p>;
  },
  h1({ children }) {
    return (
      <h1 className="mt-6 mb-3 text-xl font-semibold first:mt-0">{children}</h1>
    );
  },
  h2({ children }) {
    return (
      <h2 className="mt-5 mb-2 text-lg font-semibold first:mt-0">{children}</h2>
    );
  },
  h3({ children }) {
    return (
      <h3 className="mt-4 mb-2 text-base font-semibold first:mt-0">
        {children}
      </h3>
    );
  },
  h4({ children }) {
    return (
      <h4 className="mt-3 mb-2 text-sm font-semibold first:mt-0">{children}</h4>
    );
  },
  h5({ children }) {
    return (
      <h5 className="mt-3 mb-1 text-sm font-semibold first:mt-0">{children}</h5>
    );
  },
  h6({ children }) {
    return (
      <h6 className="mt-3 mb-1 text-xs font-semibold uppercase tracking-wide first:mt-0">
        {children}
      </h6>
    );
  },
  ul({ children }) {
    return <ul className="mb-3 list-disc space-y-1 pl-6 last:mb-0">{children}</ul>;
  },
  ol({ children }) {
    return (
      <ol className="mb-3 list-decimal space-y-1 pl-6 last:mb-0">{children}</ol>
    );
  },
  li({ children }) {
    return <li className="leading-relaxed">{children}</li>;
  },
  hr() {
    return <hr className="my-4 border-border" />;
  },
  blockquote({ children }) {
    return (
      <blockquote className="mb-3 border-l-2 border-border pl-4 italic text-muted-foreground last:mb-0">
        {children}
      </blockquote>
    );
  },
  strong({ children }) {
    return <strong className="font-semibold">{children}</strong>;
  },
  em({ children }) {
    return <em className="italic">{children}</em>;
  },
  del({ children }) {
    return <del className="line-through opacity-75">{children}</del>;
  },
  a({ href, children }) {
    return (
      <a
        href={href}
        onClick={(e) => handleLinkClick(e, href)}
        className="text-primary underline underline-offset-2 hover:no-underline"
      >
        {children}
      </a>
    );
  },
  pre({ node, children }) {
    // Block code: read the code text and language from the ORIGINAL
    // hast AST node (the `node` prop), not from `children`. By the time
    // pre's `children` is built, the `code` override below has already
    // replaced the inner element's className with our pill style and
    // wrapped the text — so reading `children.props.className` would
    // see "rounded bg-muted ..." instead of "language-rust", and every
    // code block would fall back to plain text. The hast `node` is the
    // pre-render AST and still has the original language-* class.
    const codeNode = node?.children?.[0];
    if (
      codeNode &&
      codeNode.type === "element" &&
      codeNode.tagName === "code"
    ) {
      const classNames =
        (codeNode.properties?.className as string[] | undefined) ?? [];
      const langClass = classNames.find((c) => c.startsWith("language-"));
      const language = langClass?.slice("language-".length);
      const code = (codeNode.children ?? [])
        .map((c) => (c.type === "text" ? c.value : ""))
        .join("")
        .replace(/\n$/, "");
      if (language === "diff" || language?.startsWith("diff-")) {
        // Support ```diff-tsx, ```diff-python, etc. to specify the
        // underlying language for syntax highlighting inside diffs.
        const diffLang = language?.startsWith("diff-")
          ? language.slice("diff-".length)
          : undefined;
        return <DiffCodeBlock code={code} language={diffLang} />;
      }
      return <CodeBlock code={code} language={language} />;
    }
    // Defensive fallback: unexpected pre structure, render plain.
    return (
      <pre className="mb-3 overflow-x-auto rounded-md border border-border bg-muted p-3 font-mono text-xs last:mb-0">
        {children}
      </pre>
    );
  },
  code({ children }) {
    // Only fires for inline code now — block code is intercepted at
    // the <pre> level above and rendered via <CodeBlock>.
    return (
      <code className="rounded bg-muted px-1 py-0.5 font-mono text-[0.9em]">
        {children}
      </code>
    );
  },
  table({ children }) {
    return (
      <div className="mb-3 overflow-x-auto last:mb-0">
        <table className="w-full table-auto border-collapse text-xs">
          {children}
        </table>
      </div>
    );
  },
  thead({ children }) {
    return <thead className="bg-muted">{children}</thead>;
  },
  th({ children }) {
    return (
      <th className="border border-border px-2 py-1 text-left font-semibold">
        {children}
      </th>
    );
  },
  td({ children }) {
    return <td className="border border-border px-2 py-1">{children}</td>;
  },
  img({ src, alt }) {
    return (
      <img
        src={src}
        alt={alt ?? ""}
        className="mb-3 max-w-full rounded-md last:mb-0"
      />
    );
  },
};

function MarkdownContentInner({ content }: MarkdownContentProps) {
  return (
    <ReactMarkdown remarkPlugins={[remarkGfm]} components={components}>
      {content}
    </ReactMarkdown>
  );
}

// Memoize on content identity — completed turns have immutable output
// strings, so the markdown AST parses exactly once per turn.
export const MarkdownContent = React.memo(
  MarkdownContentInner,
  (prev, next) => prev.content === next.content,
);
