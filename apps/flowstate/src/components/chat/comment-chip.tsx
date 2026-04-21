import * as React from "react";
import { MessageSquare, Pencil, X } from "lucide-react";
import type { DiffComment } from "@/lib/diff-comments-store";
import { cn } from "@/lib/utils";

/** Pre-send chip for a pending review comment. Sits in the chip row
 *  above the chat textarea alongside `InFluxAttachmentChip` /
 *  `FileMentionChip`, matching their pill styling so the row reads as
 *  a homogeneous list of "things attached to the next send".
 *
 *  Inline editing replaces the body text with a small textarea. Enter
 *  commits, Escape (or blur-with-empty) cancels. Esc stops propagation
 *  so ChatView's Esc-to-interrupt doesn't fire on the same keystroke —
 *  same escape-hygiene pattern as the queued-message editor. */
export function CommentChip({
  comment,
  onUpdate,
  onRemove,
}: {
  comment: DiffComment;
  onUpdate: (text: string) => void;
  onRemove: () => void;
}) {
  const [editing, setEditing] = React.useState(false);
  const [draft, setDraft] = React.useState(comment.text);
  const inputRef = React.useRef<HTMLTextAreaElement>(null);

  React.useEffect(() => {
    if (editing && inputRef.current) {
      const el = inputRef.current;
      el.focus();
      el.selectionStart = el.selectionEnd = el.value.length;
      el.style.height = "auto";
      el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
    }
  }, [editing]);

  function commit() {
    const trimmed = draft.trim();
    if (trimmed.length === 0) {
      // Empty commit removes the chip — same contract as the queued
      // message editor. Users with stale "just cancel" intent should
      // press Escape; an empty body has no value to keep.
      onRemove();
      return;
    }
    if (trimmed !== comment.text) onUpdate(trimmed);
    setEditing(false);
  }

  function cancel() {
    setDraft(comment.text);
    setEditing(false);
  }

  function onKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      commit();
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      cancel();
    }
  }

  const anchor = formatAnchor(comment);

  if (editing) {
    return (
      <span
        className={cn(
          "group relative inline-flex max-w-full items-start gap-1.5 rounded-md border border-border bg-muted/40 py-1 pl-2 pr-2 text-xs",
        )}
      >
        <MessageSquare className="mt-0.5 h-3 w-3 shrink-0 text-muted-foreground" />
        <span className="flex min-w-0 flex-col gap-0.5">
          <span
            className="truncate font-mono text-[10px] text-muted-foreground"
            title={anchor}
          >
            {anchor}
          </span>
          <textarea
            ref={inputRef}
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={onKeyDown}
            onBlur={() => requestAnimationFrame(commit)}
            onInput={(e) => {
              const el = e.currentTarget;
              el.style.height = "auto";
              el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
            }}
            rows={1}
            className="w-64 max-w-[400px] resize-none rounded border border-input bg-background px-1.5 py-1 text-xs text-foreground/85 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
          />
        </span>
      </span>
    );
  }

  return (
    <span
      className={cn(
        "group relative inline-flex max-w-full items-center gap-1.5 rounded-md border border-border bg-muted/40 py-0.5 pl-2 pr-12 text-xs hover:bg-muted",
      )}
      title={`${anchor} — ${comment.text}`}
    >
      <MessageSquare className="h-3 w-3 shrink-0 text-muted-foreground" />
      <span className="max-w-[120px] truncate font-mono text-[10px] text-muted-foreground">
        {anchor}
      </span>
      <span className="max-w-[240px] truncate text-foreground/85">
        {comment.text}
      </span>
      <span
        role="button"
        aria-label="Edit comment"
        onMouseDown={(e) => {
          // Same mouseDown-preventDefault-stopPropagation dance as
          // FileMentionChip — keeps the chat textarea focused and
          // prevents parent hover handlers from eating the event.
          e.preventDefault();
          e.stopPropagation();
          setEditing(true);
        }}
        className="absolute right-6 top-1/2 -translate-y-1/2 hidden rounded-full p-0.5 text-muted-foreground hover:bg-accent hover:text-accent-foreground group-hover:inline-flex"
      >
        <Pencil className="h-3 w-3" />
      </span>
      <span
        role="button"
        aria-label="Remove comment"
        onMouseDown={(e) => {
          e.preventDefault();
          e.stopPropagation();
          onRemove();
        }}
        className="absolute right-1 top-1/2 -translate-y-1/2 hidden rounded-full p-0.5 text-muted-foreground hover:bg-destructive/10 hover:text-destructive group-hover:inline-flex"
      >
        <X className="h-3 w-3" />
      </span>
    </span>
  );
}

function formatAnchor(comment: DiffComment): string {
  const { path, line, lineRange } = comment.anchor;
  const slash = path.lastIndexOf("/");
  const basename = slash >= 0 ? path.slice(slash + 1) : path;
  if (lineRange) {
    const [start, end] = lineRange;
    return start === end ? `${basename}:${start}` : `${basename}:${start}-${end}`;
  }
  if (typeof line === "number") {
    return `${basename}:${line}`;
  }
  return basename;
}
