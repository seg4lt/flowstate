import * as React from "react";
import { Brain, ChevronDown, ChevronRight } from "lucide-react";
import type { MemoryRecallItem } from "@/lib/types";
import { MarkdownContent } from "./markdown-content";

/**
 * Inline "Recalled from memory" chip. Rendered where the Claude
 * Agent SDK's memory-recall supervisor attached relevant memory
 * files to the turn's context.
 *
 * Two shapes the SDK emits, both handled here:
 *
 * - `mode: 'select'`  — full file bodies. `memories[i].path` is an
 *   absolute file path; `content` is absent (renderers lazy-load
 *   from `path` on demand). We list the filenames + scope chip and
 *   stop there — clicking a row doesn't load the file today; that's
 *   a follow-up when we have a file viewer to route into.
 * - `mode: 'synthesize'` — a Sonnet-authored paragraph distilled
 *   from many tiny memories. `memories[i].path` is a synthesis
 *   sentinel of the form `<synthesis:DIR>`, and `content` holds the
 *   paragraph. We render the paragraph as markdown and fold the
 *   synthesis source directories in as a muted footer.
 *
 * Subtle by design — this is a footnote about WHAT went into the
 * model's context, not a message in the user's conversation.
 */
export function MemoryRecallBlock({
  mode,
  memories,
}: {
  mode: "select" | "synthesize";
  memories: MemoryRecallItem[];
}) {
  const [expanded, setExpanded] = React.useState(false);
  const count = memories.length;

  if (count === 0) return null;

  const label =
    mode === "synthesize"
      ? `Recalled from memory: ${count} synthesized note${count === 1 ? "" : "s"}`
      : `Recalled from memory: ${count} file${count === 1 ? "" : "s"}`;

  return (
    <div className="my-1 flex flex-col gap-1 text-[11px] text-muted-foreground">
      <button
        type="button"
        onClick={() => setExpanded((s) => !s)}
        className="inline-flex w-fit items-center gap-1.5 rounded-md border border-border/50 bg-muted/30 px-2 py-1 hover:text-foreground"
        aria-expanded={expanded}
      >
        <Brain className="h-3 w-3" />
        <span className="font-medium">{label}</span>
        {expanded ? (
          <ChevronDown className="h-3 w-3" />
        ) : (
          <ChevronRight className="h-3 w-3" />
        )}
      </button>

      {expanded && mode === "select" && (
        <ul className="ml-5 flex flex-col gap-0.5">
          {memories.map((m, i) => (
            <li
              key={`${m.path}-${i}`}
              className="flex items-center gap-2 font-mono"
            >
              <span className="truncate">{m.path}</span>
              <ScopeChip scope={m.scope} />
            </li>
          ))}
        </ul>
      )}

      {expanded && mode === "synthesize" && (
        <div className="ml-5 flex flex-col gap-2">
          {memories.map((m, i) => (
            <div key={`${m.path}-${i}`} className="flex flex-col gap-1">
              {m.content && (
                <div className="rounded-md border border-border/50 bg-muted/30 px-3 py-2 text-xs text-foreground">
                  <MarkdownContent content={m.content} />
                </div>
              )}
              <div className="flex items-center gap-2 font-mono">
                <span className="truncate opacity-70">{m.path}</span>
                <ScopeChip scope={m.scope} />
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function ScopeChip({ scope }: { scope: "personal" | "team" }) {
  const tone =
    scope === "team"
      ? "border-sky-500/40 bg-sky-500/5 text-sky-700 dark:text-sky-400"
      : "border-border bg-muted/50";
  return (
    <span
      className={`rounded-full border px-1.5 py-0.5 text-[10px] uppercase tracking-wide ${tone}`}
    >
      {scope}
    </span>
  );
}
