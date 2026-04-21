import * as React from "react";

/** Anchor that identifies what a pending review comment is attached to.
 *  `line` is set when the comment was triggered by a hover on a single
 *  diff line. `lineRange` (inclusive start/end, 1-based) is set when the
 *  comment was triggered by a text selection spanning one or more lines
 *  — selections on a single line still use `lineRange` so the renderer
 *  can show the selected text verbatim. Exactly one of `line` /
 *  `lineRange` will be present.
 *
 *  `surface` distinguishes the diff panel from the content-search
 *  multibuffer; it's not used in the default serialization but lets UIs
 *  (and future telemetry) tell the two entry points apart. */
export interface DiffCommentAnchor {
  path: string;
  /** Where the comment was created. `diff` = chat-side diff panel,
   *  `search` = content-search multibuffer, `code` = an opened file
   *  in the code-view tab pane. Not used in the default serializer —
   *  it's metadata for future surface-aware rendering or telemetry. */
  surface: "diff" | "search" | "code";
  line?: number;
  lineRange?: [number, number];
  selectionText?: string;
}

export interface DiffComment {
  id: string;
  anchor: DiffCommentAnchor;
  text: string;
  createdAt: number;
}

// Per-session pending comments. Module-level so it survives
// ChatInput / DiffPanel remounts on thread switches, mirroring the
// sessionDrafts / sessionQueues pattern in chat-view.tsx.
const commentsBySession = new Map<string, DiffComment[]>();

// External store shape for useSyncExternalStore. Subscribers are keyed
// by sessionId so a re-render only fires for the session whose list
// actually changed (switching tabs doesn't rerender every chat input).
type Listener = () => void;
const listeners = new Map<string, Set<Listener>>();

function notify(sessionId: string): void {
  const subs = listeners.get(sessionId);
  if (!subs) return;
  // Copy before iterating: a listener might unsubscribe during its
  // own callback, which would otherwise mutate the set mid-iteration.
  for (const listener of [...subs]) {
    listener();
  }
}

function subscribe(sessionId: string, listener: Listener): () => void {
  let subs = listeners.get(sessionId);
  if (!subs) {
    subs = new Set();
    listeners.set(sessionId, subs);
  }
  subs.add(listener);
  return () => {
    const s = listeners.get(sessionId);
    if (!s) return;
    s.delete(listener);
    if (s.size === 0) listeners.delete(sessionId);
  };
}

// Stable empty-array sentinel so sessions with no comments return the
// same reference across renders. Without this useSyncExternalStore
// would see a fresh [] every render and enter an infinite update loop.
// Double-cast through unknown because Object.freeze returns a readonly
// type but the hook's contract (and the downstream .map calls) expects
// a plain DiffComment[] — the array is never mutated in practice so
// the widening is a type-system concession, not a behavioural one.
const EMPTY: DiffComment[] = Object.freeze([]) as unknown as DiffComment[];

function getSnapshot(sessionId: string): DiffComment[] {
  return commentsBySession.get(sessionId) ?? EMPTY;
}

function newCommentId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return `c-${Math.random().toString(36).slice(2)}-${Date.now()}`;
}

/** Append a new comment to a session's pending list. Returns the new
 *  comment's id so the caller (e.g. the overlay popup) can immediately
 *  focus-target or scroll to the fresh chip if it wants. */
export function addComment(
  sessionId: string,
  input: Omit<DiffComment, "id" | "createdAt">,
): string {
  const id = newCommentId();
  const next: DiffComment = { ...input, id, createdAt: Date.now() };
  const prev = commentsBySession.get(sessionId) ?? [];
  commentsBySession.set(sessionId, [...prev, next]);
  notify(sessionId);
  return id;
}

export function updateComment(
  sessionId: string,
  id: string,
  text: string,
): void {
  const prev = commentsBySession.get(sessionId);
  if (!prev) return;
  const idx = prev.findIndex((c) => c.id === id);
  if (idx === -1) return;
  const next = [...prev];
  next[idx] = { ...prev[idx]!, text };
  commentsBySession.set(sessionId, next);
  notify(sessionId);
}

export function removeComment(sessionId: string, id: string): void {
  const prev = commentsBySession.get(sessionId);
  if (!prev) return;
  const next = prev.filter((c) => c.id !== id);
  if (next.length === prev.length) return;
  if (next.length === 0) {
    commentsBySession.delete(sessionId);
  } else {
    commentsBySession.set(sessionId, next);
  }
  notify(sessionId);
}

/** Drop every pending comment for a session. Called after a successful
 *  send — the comments are now baked into the sent message text, so the
 *  chip row should clear the same way the draft textarea does. */
export function clearComments(sessionId: string): void {
  if (!commentsBySession.has(sessionId)) return;
  commentsBySession.delete(sessionId);
  notify(sessionId);
}

/** React hook: subscribe to a session's pending comments. When
 *  `sessionId` is null (e.g. a brand-new thread that hasn't been
 *  persisted yet), returns a stable empty array and no subscription is
 *  registered. */
export function useSessionComments(sessionId: string | null): DiffComment[] {
  const subscribeFn = React.useCallback(
    (listener: Listener): (() => void) => {
      if (!sessionId) return () => {};
      return subscribe(sessionId, listener);
    },
    [sessionId],
  );
  const getSnapshotFn = React.useCallback(
    () => (sessionId ? getSnapshot(sessionId) : EMPTY),
    [sessionId],
  );
  return React.useSyncExternalStore(subscribeFn, getSnapshotFn, getSnapshotFn);
}

/** Truncate a selection snippet so a chatty selection doesn't bloat the
 *  chip list or the outgoing message. The cap matches what the overlay
 *  already applies, but re-asserting here keeps the serializer safe to
 *  use with comments built from other sources (tests, future imports). */
const SELECTION_MAX_CHARS = 400;

function formatAnchor(anchor: DiffCommentAnchor): string {
  if (anchor.lineRange) {
    const [start, end] = anchor.lineRange;
    return start === end
      ? `${anchor.path}:${start}`
      : `${anchor.path}:${start}-${end}`;
  }
  if (typeof anchor.line === "number") {
    return `${anchor.path}:${anchor.line}`;
  }
  return anchor.path;
}

function truncate(text: string): string {
  if (text.length <= SELECTION_MAX_CHARS) return text;
  return text.slice(0, SELECTION_MAX_CHARS) + "…";
}

/** Render a comments list as a plain-text prefix suitable for
 *  prepending to the outgoing message. The format is deliberately
 *  lightweight Markdown (bullet list) so the model sees a human-readable
 *  review block and can address each point in turn. Returns an empty
 *  string when there are no comments — callers can unconditionally
 *  prepend + "\n" to the user's draft. */
export function serializeCommentsAsPrefix(comments: DiffComment[]): string {
  if (comments.length === 0) return "";
  const lines: string[] = ["Review comments:"];
  for (const c of comments) {
    const anchor = formatAnchor(c.anchor);
    // Collapse whitespace in the comment text so a multi-line chip
    // renders on a single bullet. Users who really want multi-line
    // bodies can still send them via the free-form draft area.
    const body = c.text.replace(/\s+/g, " ").trim();
    lines.push(`- ${anchor} — ${body}`);
    if (c.anchor.selectionText) {
      // Indent the quoted selection so it visually nests under the
      // bullet without colliding with Markdown list rules.
      const quoted = truncate(c.anchor.selectionText)
        .split("\n")
        .map((l) => `    > ${l}`)
        .join("\n");
      lines.push(quoted);
    }
  }
  return lines.join("\n");
}

export const SELECTION_TEXT_MAX_CHARS = SELECTION_MAX_CHARS;
