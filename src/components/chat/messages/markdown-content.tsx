import * as React from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { openUrl } from "@tauri-apps/plugin-opener";

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
  pre({ children }) {
    // Block code container. The nested <code> gets its inline styles
    // stripped via the arbitrary variants so language-tagged and
    // untagged fences both look right.
    return (
      <pre className="mb-3 overflow-x-auto rounded-md border border-border bg-muted p-3 font-mono text-xs last:mb-0 [&>code]:bg-transparent [&>code]:p-0 [&>code]:text-[0.95em]">
        {children}
      </pre>
    );
  },
  code({ className, children }) {
    // This fires for both inline and block code. Block code is always
    // wrapped in <pre> (see override above), which strips our inline
    // styling. Inline code gets the pill look below.
    return (
      <code
        className={
          className ??
          "rounded bg-muted px-1 py-0.5 font-mono text-[0.9em]"
        }
      >
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
