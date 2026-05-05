import * as React from "react";
import { Clock, Pencil, Send, Square, Trash2 } from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import type {
  AttachedImage,
  PermissionMode,
  ProviderKind,
  SessionStatus,
} from "@/lib/types";
import {
  formatSkillInvocation,
  getCompletions,
  isCoreCommand,
  type SlashCommandItem,
} from "@/lib/slash-commands";
import { cn } from "@/lib/utils";
import { toast } from "@/hooks/use-toast";
import { projectFilesQueryOptions } from "@/lib/queries";
import {
  applyMentionPick,
  detectMentionContext,
  rankFileMatches,
  type MentionContext,
} from "@/lib/mention-utils";
import { readFileAsBase64 } from "@/lib/api/fs";
import { SlashCommandPopup } from "./slash-command-popup";
import { MentionPopup } from "./mention-popup";
import { InFluxAttachmentChip } from "./attachment-chip";
import { FileMentionChip } from "./file-mention-chip";
import { ImageLightbox, type LightboxSource } from "./image-lightbox";
import { CommentChip } from "./comment-chip";
import { FOCUS_CHAT_INPUT_EVENT } from "@/lib/keyboard";
import {
  clearComments,
  removeComment,
  serializeCommentsAsPrefix,
  updateComment,
  useSessionComments,
} from "@/lib/diff-comments-store";

interface ChatInputProps {
  /** Dispatch a turn. Returns a promise that resolves once the
   *  daemon has accepted the `send_turn` RPC (or rejects with the
   *  daemon's error message). The drain effect awaits this so a
   *  failed send leaves the queued chip in place rather than
   *  silently popping it; callers therefore MUST throw on
   *  `ServerMessage::Error` rather than swallowing it. */
  onSend: (input: string, images: AttachedImage[]) => Promise<void> | void;
  onInterrupt: () => void;
  /** Atomic steer: interrupt the current turn AND dispatch `input`
   *  as the next turn in a single daemon-side RPC. Used by the
   *  "Send now" affordance on queued chips so there's no
   *  frontend-side interrupt→send race. Same throw-on-error
   *  contract as `onSend`. */
  onSteer: (input: string, images: AttachedImage[]) => Promise<void> | void;
  sessionStatus: SessionStatus | undefined;
  disabled: boolean;
  /** When true, the session's provider has been toggled off in
   *  Settings — the composer is locked read-only until the user
   *  re-enables it. Distinct from `disabled` which is about transient
   *  loading states. */
  providerDisabled?: boolean;
  /** When true, the session is archived and strictly read-only — no
   *  new messages, no unarchive path. Archived threads exist only
   *  for history viewing. */
  archived?: boolean;
  toolbar?: React.ReactNode;
  /** Command metadata for the autocomplete popup. Merged list of
   *  core commands + provider-native commands + user skills + agents. */
  commands?: SlashCommandItem[];
  /** Active session's provider. Drives invocation formatting — Codex
   *  uses `$name`, everyone else uses `/name`. */
  provider?: ProviderKind;
  /** Seed text to restore when the component remounts after a tab
   *  switch. The component is keyed by sessionId so it remounts on
   *  every thread change — this prop lets the parent supply the saved
   *  draft so the user's in-progress message isn't lost. */
  initialValue?: string;
  /** Fires on every keystroke so the parent can persist the draft
   *  text outside this component's lifecycle. */
  onDraftChange?: (value: string) => void;
  /** Seed queue to restore when the component remounts after a tab
   *  switch. Same pattern as initialValue/onDraftChange for draft text. */
  initialQueue?: QueuedMessage[];
  /** Fires whenever the queue changes so the parent can persist it
   *  outside this component's lifecycle. */
  onQueueChange?: (queue: QueuedMessage[]) => void;
  /** Current permission mode — drives the composer tint so the user
   *  can see at rest which mode the next send will use. Plan mode
   *  tints blue, bypass tints orange; default / accept_edits keep
   *  the neutral styling. */
  permissionMode?: PermissionMode;
  /** Provider-predicted next user prompt. Rendered as muted ghost
   *  text in the empty composer. Tab accepts (fills into draft);
   *  Esc or any other keystroke dismisses via
   *  `onPromptSuggestionDismissed`. Only shown when the session's
   *  provider has `features.promptSuggestions` enabled AND the
   *  composer is empty. */
  promptSuggestion?: string | null;
  /** Called by the composer when the user accepts, rejects, or
   *  types past the ghost text. The parent clears its stored
   *  suggestion so it doesn't re-appear after a keystroke. */
  onPromptSuggestionDismissed?: () => void;
  /** Absolute path to the session's project/worktree. Drives the
   *  `@<filename>` mention autocomplete — when null/undefined the
   *  mention popup is disabled (e.g. on threads without a project).
   *  The file list comes from `list_project_files` (ripgrep's
   *  gitignore-aware walker) and is cached forever via
   *  `projectFilesQueryOptions`. */
  projectPath?: string | null;
  /** Active session id. Keys the pending-comments store so the chip
   *  row above the textarea reflects this session's review comments.
   *  When null (new unsaved thread) no comments are shown and the
   *  send path behaves exactly as before. */
  sessionId: string | null;
}

export interface QueuedMessage {
  id: string;
  text: string;
  images: AttachedImage[];
}

/** Per-image cap for clipboard paste — images only. Mirrors the
 *  original Rust `ATTACHMENT_MAX_BYTES` before it was raised for
 *  drag-and-drop media. Kept separate so paste can't silently push a
 *  25 MB screenshot through; drops have their own larger cap below. */
const IMAGE_MAX_BYTES = 5 * 1024 * 1024;
/** Per-media cap for drag-and-drop. Matches the Rust-side
 *  `ATTACHMENT_MAX_BYTES` (50 MB) that the runtime enforces when
 *  writing the attachment to disk. Short audio clips and small
 *  screen recordings fit; anything larger is rejected before the
 *  bytes leave the webview. */
const MEDIA_MAX_BYTES = 50 * 1024 * 1024;
/** Allowed clipboard image MIME types — matches the Rust validator. */
const ALLOWED_IMAGE_MEDIA_TYPES = new Set([
  "image/png",
  "image/jpeg",
  "image/gif",
  "image/webp",
]);
/** MIME-type prefixes treated as "media" for drag-and-drop. Images,
 *  audio, and video all ride through the same base64 → attachment
 *  pipeline as clipboard images; every other dropped file is surfaced
 *  as an `@file` mention chip instead, so the agent reads it via its
 *  `Read` tool rather than us bundling the bytes. */
const MEDIA_MIME_PREFIXES = ["image/", "audio/", "video/"];

/** Stable empty-array sentinel for the `@mention` autocomplete's
 *  ranking memo: keeps the dependency identity steady when the
 *  query hasn't returned yet, so we don't re-rank-and-rerender on
 *  every poll cycle while fff-search's cold scan is still walking. */
const EMPTY_MENTION_FILES: readonly string[] = Object.freeze([]);

/** Soft cap on rows rendered inside the `@mention` popup. Kept small
 *  (the popup's max-height is ~20 rows) but surfaced via a "+N more"
 *  footer when the ranker has more matches — the previous behavior
 *  silently dropped the long tail at 50 with no UI signal. */
const MENTION_POPUP_LIMIT = 50;

/** Resize a textarea to fit its content, capped by:
 *    - an absolute ceiling (`hardCap`, default 200px) so very long
 *      drafts still produce a scrollable textarea rather than
 *      swallowing the chat list, and
 *    - the room actually available below the textarea's top edge in
 *      its nearest positioned ancestor, so the composer never grows
 *      past the bottom of the viewport / popout window.
 *  This replaces the previous hard `Math.min(scrollHeight, 200)`
 *  formula which ignored available space — when the window (or
 *  popout) was short the composer would punch out the bottom of
 *  ChatView's `overflow-hidden` clip and lose its toolbar / send
 *  button. */
function autosizeTextarea(el: HTMLTextAreaElement, hardCap = 200) {
  el.style.height = "auto";
  // `offsetParent` is the nearest positioned ancestor; for the
  // composer that's the `relative flex-1` wrapper around the
  // textarea, whose own bottom is bounded by the composer's
  // max-h. Falls back to viewport height during initial layout
  // (offsetParent can be null when the element is detached or
  // inside `display: none` — both transient). The `40` floor
  // matches `min-h-10` so we never report a negative budget.
  const parent = el.offsetParent as HTMLElement | null;
  const available = parent
    ? Math.max(40, parent.clientHeight - el.offsetTop - 4)
    : window.innerHeight;
  el.style.height = `${Math.min(el.scrollHeight, hardCap, available)}px`;
}

/** Does `mediaType` classify as drag-and-drop media (image/audio/video)? */
function isMediaMimeType(mediaType: string): boolean {
  return MEDIA_MIME_PREFIXES.some((prefix) => mediaType.startsWith(prefix));
}

function suggestedFilename(mediaType: string): string {
  switch (mediaType) {
    case "image/png":
      return "image.png";
    case "image/jpeg":
      return "image.jpg";
    case "image/gif":
      return "image.gif";
    case "image/webp":
      return "image.webp";
    default:
      return "image";
  }
}

/** Read a Blob as a base64 string (no `data:` prefix). */
function blobToBase64(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error ?? new Error("FileReader failed"));
    reader.onload = () => {
      const result = reader.result;
      if (typeof result !== "string") {
        reject(new Error("expected base64 data URL"));
        return;
      }
      const comma = result.indexOf(",");
      resolve(comma >= 0 ? result.slice(comma + 1) : result);
    };
    reader.readAsDataURL(blob);
  });
}

function newQueueId(): string {
  // crypto.randomUUID() is available in modern browsers and the Tauri
  // webview. The Math.random fallback only runs in test environments
  // that stub crypto out.
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return `q-${Math.random().toString(36).slice(2)}-${Date.now()}`;
}

export function ChatInput({
  onSend,
  onInterrupt,
  onSteer,
  sessionStatus,
  disabled,
  providerDisabled = false,
  archived = false,
  toolbar,
  commands,
  provider,
  initialValue = "",
  onDraftChange,
  initialQueue,
  onQueueChange,
  permissionMode,
  promptSuggestion,
  onPromptSuggestionDismissed,
  projectPath,
  sessionId,
}: ChatInputProps) {
  // Pending review comments for this session — rendered as chips
  // above the textarea and serialized into the outgoing message on
  // send. Populated by DiffCommentOverlay when the user adds a
  // comment via the diff panel or search multibuffer.
  const comments = useSessionComments(sessionId);
  const [value, setValueRaw] = React.useState(initialValue);
  // Notify the parent of every draft change so it can persist the text
  // across component remounts (tab switches). The ref avoids stale
  // closure issues and keeps the wrapper allocation-free.
  const onDraftChangeRef = React.useRef(onDraftChange);
  onDraftChangeRef.current = onDraftChange;
  const setValue = React.useCallback((next: React.SetStateAction<string>) => {
    setValueRaw((prev) => {
      const resolved = typeof next === "function" ? next(prev) : next;
      onDraftChangeRef.current?.(resolved);
      return resolved;
    });
  }, []);
  const [queued, setQueuedRaw] = React.useState<QueuedMessage[]>(initialQueue ?? []);
  // Mirror the setValue / onDraftChange pattern: notify the parent on
  // every queue mutation so it can persist the queue outside this
  // component's lifecycle (survives remounts on thread switch).
  const onQueueChangeRef = React.useRef(onQueueChange);
  onQueueChangeRef.current = onQueueChange;
  const setQueued = React.useCallback(
    (next: React.SetStateAction<QueuedMessage[]>) => {
      setQueuedRaw((prev) => {
        const resolved = typeof next === "function" ? next(prev) : next;
        onQueueChangeRef.current?.(resolved);
        return resolved;
      });
    },
    [],
  );
  const [popupIndex, setPopupIndex] = React.useState(0);
  const [attachedImages, setAttachedImages] = React.useState<AttachedImage[]>([]);
  // --- `@<filename>` mention state ---
  // `attachedFiles` is the chip list (dedup'd, preserves insertion
  // order). The source of truth for what the agent sees is the raw
  // `@<path>` text tokens in `value` — these chips are a UI hint.
  const [attachedFiles, setAttachedFiles] = React.useState<string[]>([]);
  // `mentionCtx` is the lexical context at the caret. Recomputed on
  // every onChange / onKeyUp / onSelect so caret moves also update
  // the popup. Null means "no open mention right now".
  const [mentionCtx, setMentionCtx] =
    React.useState<MentionContext | null>(null);
  const [mentionIndex, setMentionIndex] = React.useState(0);
  const [lightboxSource, setLightboxSource] = React.useState<LightboxSource | null>(
    null,
  );
  const [editingId, setEditingId] = React.useState<string | null>(null);
  const [editText, setEditText] = React.useState("");
  // `isDragOver` toggles a subtle outline on the composer while the
  // user is mid-drag over the app window. Cleared by the `leave` /
  // `drop` Tauri drag events — drops fire regardless of the cursor
  // position, so we don't do any hit-testing against the composer
  // rect; any drop anywhere in the window is routed to this composer
  // while it's mounted and interactive.
  const [isDragOver, setIsDragOver] = React.useState(false);
  const textareaRef = React.useRef<HTMLTextAreaElement>(null);
  const editTextareaRef = React.useRef<HTMLTextAreaElement>(null);

  // Land focus in the composer on every mount *and* whenever the
  // composer transitions from non-interactive → interactive. ChatView
  // keys this component by sessionId, so a thread switch remounts and
  // re-fires this effect. For newly created threads the session query
  // is still loading on first mount (disabled=true); once the query
  // resolves the deps change and focus is applied, so the user can
  // start typing immediately.
  React.useEffect(() => {
    if (disabled || providerDisabled || archived) return;
    const el = textareaRef.current;
    if (!el) return;
    // Avoid re-focusing while the user is already typing in this textarea.
    // `disabled` flips false when the session query resolves — if that
    // happens mid-keystroke, calling el.focus() resets the browser caret
    // and the next characters land at the wrong offset. Only steal focus
    // when something else currently has it (or nothing does).
    if (document.activeElement !== el) {
      el.focus();
    }
    // When restoring a saved draft the textarea starts at rows=1;
    // auto-size it so the full draft is visible on mount.
    if (el.value.length > 0) {
      autosizeTextarea(el);
    }
  }, [disabled, providerDisabled, archived]);

  // Bridge for the model / effort selectors (and any future toolbar
  // picker) to hand focus back to the composer when they close. Radix
  // Popover / DropdownMenu return focus to the trigger button by
  // default — fine for mouse users, but a regression for the keyboard
  // path: ⌘⇧M / ⌘⇧E + arrows + Enter would otherwise leave focus on
  // the toolbar chip, forcing a mouse trip back to the textarea before
  // typing can resume. The pickers `preventDefault()` on
  // `onCloseAutoFocus` and dispatch this event so the composer (the
  // only component holding `textareaRef`) does the focus call itself.
  React.useEffect(() => {
    function onFocusRequest() {
      const el = textareaRef.current;
      if (!el) return;
      if (disabled || providerDisabled || archived) return;
      el.focus();
    }
    window.addEventListener(FOCUS_CHAT_INPUT_EVENT, onFocusRequest);
    return () =>
      window.removeEventListener(FOCUS_CHAT_INPUT_EVENT, onFocusRequest);
  }, [disabled, providerDisabled, archived]);

  // Auto-focus the inline edit textarea when entering edit mode.
  React.useEffect(() => {
    if (editingId && editTextareaRef.current) {
      const el = editTextareaRef.current;
      el.focus();
      el.selectionStart = el.selectionEnd = el.value.length;
      autosizeTextarea(el);
    }
  }, [editingId]);

  const isRunning = sessionStatus === "running";

  // --- Slash command autocomplete ---
  // Show popup when the input starts with `/` (all providers) or `$`
  // (Codex skill invocations). We intentionally allow the popup while
  // a turn is running: the user can still compose (messages queue),
  // and skill invocations need to be selectable so they queue like any
  // other text. Core app commands like `/flowstate-clear` fire
  // immediately on select; ChatView.handleSend rejects those mid-run
  // with a toast, so the popup staying visible doesn't break anything.
  const inputToken = value.trim().split(/\s/)[0] ?? "";
  const showPopup =
    (inputToken.startsWith("/") || inputToken.startsWith("$")) && !disabled;
  const matches: SlashCommandItem[] = showPopup
    ? getCompletions(inputToken, commands)
    : [];

  // Reset the highlighted index when the match list changes.
  React.useEffect(() => {
    setPopupIndex(0);
  }, [matches.length, inputToken]);

  // --- `@<filename>` mention autocomplete ---
  // The project file list comes from fff-search's per-worktree
  // index via `projectFilesQueryOptions`. The query returns a
  // `ProjectFileListing` (full list under `.files`, plus an
  // `indexing` flag). While the cold scan is still running React
  // Query re-polls every 750 ms so newly-indexed files appear in
  // the popup live. When `projectPath` is null (unlikely but
  // possible for degenerate sessions) the query short-circuits and
  // we get an empty list, which turns the popup off naturally.
  const filesQuery = useQuery(projectFilesQueryOptions(projectPath ?? null));
  const mentionFiles = filesQuery.data?.files ?? EMPTY_MENTION_FILES;
  // Rank with no cap then slice client-side so we can render a
  // "+N more" footer in `MentionPopup` when the cap is biting. The
  // previous code passed the default `MAX_RESULTS = 50` cap straight
  // into `rankFileMatches`, silently hiding the long tail with no UI
  // signal — same class of silent-truncation bug as the picker's old
  // 20k-file walker. The limit itself stays at 50 (small popup, must
  // be scannable) but the user now sees that more matches exist.
  const mentionDisplayLimit = MENTION_POPUP_LIMIT;
  const mentionMatch = React.useMemo<{
    rows: string[];
    total: number;
  }>(() => {
    if (!mentionCtx) return { rows: [], total: 0 };
    const ranked = rankFileMatches(mentionFiles, mentionCtx.query, Infinity);
    return {
      rows: ranked.slice(0, mentionDisplayLimit),
      total: ranked.length,
    };
  }, [mentionCtx, mentionFiles, mentionDisplayLimit]);
  const mentionMatches = mentionMatch.rows;
  const mentionTotalMatches = mentionMatch.total;
  // The slash-command popup wins if both could render on the same
  // draft — in practice they can't (different prefixes) but keep the
  // guard so a future edit can't cross-trigger them.
  const showMentionPopup =
    !!mentionCtx && mentionMatches.length > 0 && !showPopup && !disabled;

  // Reset the highlighted mention row when the list shape changes.
  React.useEffect(() => {
    setMentionIndex(0);
  }, [mentionMatches.length, mentionCtx?.query]);

  // Track the last skill token we pre-filled into the composer from a
  // popup select. When the user hits Enter on that exact token (no
  // extra args appended), we send it straight through instead of
  // re-entering handlePopupSelect and looping forever.
  const lastSelectedSkillTokenRef = React.useRef<string | null>(null);

  function resetHeight() {
    if (textareaRef.current) {
      textareaRef.current.style.height = "auto";
    }
  }

  // Drain the queue when the current turn ends. We watch for the
  // running -> ready transition specifically (via a prevStatus ref)
  // rather than firing whenever sessionStatus === "ready", because
  // after we send the first queued message we'll re-enter this effect
  // with status still "ready" until the new turn flips it back to
  // "running" -- without the transition guard we'd drain the entire
  // queue in one synchronous burst and the runtime would reject
  // overlapping send_turn calls. Both the normal completion
  // (running -> ready) and an explicit interrupt (running -> interrupted)
  // drain the head of the queue, because after a stop there is no
  // in-flight send_turn to race against.
  //
  // Steer (the "Send now" button on a queued chip) is an atomic
  // daemon-side RPC (`steer_turn`) that handles its own
  // interrupt-then-send sequencing. BUT the daemon's interrupt phase
  // still flips the session through running -> interrupted before the
  // steered turn starts, which would re-enter this effect and drain
  // the head of the *remaining* queue as a regular send_turn, racing
  // the steer. The `steerInFlightRef` below suppresses exactly that
  // one transition; the drain resumes naturally when the steered
  // turn completes (running -> ready).
  const prevStatusRef = React.useRef(
    // If mounting with a non-empty queue and the session already
    // finished, pretend the previous status was "running" so the
    // drain effect fires on this render and picks up where we left off.
    // This handles the case where the user switched threads, the turn
    // completed while they were away, and they switched back.
    initialQueue && initialQueue.length > 0 &&
    (sessionStatus === "ready" || sessionStatus === "interrupted")
      ? "running"
      : sessionStatus,
  );
  // Set when a steer (per-chip "Send now") is in flight. The daemon's
  // steer_turn produces a running -> interrupted transition before the
  // steered message starts; without this guard the drain effect would
  // fire on that transition and pop the remaining queue head as a
  // regular send_turn, racing the steer. We clear the ref the first
  // time we observe the steer's interrupt/ready transition.
  const steerInFlightRef = React.useRef(false);
  /** Watchdog that clears `steerInFlightRef` if the expected
   *  running→interrupted/ready transition never arrives — without
   *  this the flag could pin true forever (e.g. the steer's
   *  `sendMessage` rejected async, the daemon was killed mid-RPC,
   *  the original turn was already finishing when the user clicked
   *  "Send now") and silently swallow every subsequent legitimate
   *  drain. 10s is comfortably longer than any real
   *  interrupt→finalize round-trip (the daemon's own bound is 10s)
   *  but short enough that a wedged flag self-heals within one
   *  user-visible delay. */
  const steerWatchdogRef = React.useRef<ReturnType<typeof setTimeout> | null>(
    null,
  );
  // Best-effort cleanup on unmount. ChatInput is keyed by sessionId
  // so this fires on every thread switch — not strictly necessary
  // (a fresh component starts with a fresh ref) but keeps the timer
  // accounting tidy in dev tools.
  React.useEffect(() => {
    return () => {
      if (steerWatchdogRef.current !== null) {
        clearTimeout(steerWatchdogRef.current);
        steerWatchdogRef.current = null;
      }
    };
  }, []);
  /** Set while a drain's `onSend` is awaiting daemon acknowledgement.
   *  Suppresses re-entry from the effect re-running when `onSend`'s
   *  identity changes mid-await (the parent rebuilds `handleSend`
   *  on every render) so we don't fire two `send_turn` RPCs for the
   *  same head. */
  const drainInFlightRef = React.useRef(false);
  React.useEffect(() => {
    const wasRunning = prevStatusRef.current === "running";
    const nowReady = sessionStatus === "ready" || sessionStatus === "interrupted";
    prevStatusRef.current = sessionStatus;
    if (!wasRunning || !nowReady) return;

    // A steer is responsible for this transition — don't drain. The
    // daemon will flip status back to running for the steered turn,
    // then to ready when it completes; the drain resumes on that
    // natural completion.
    if (steerInFlightRef.current) {
      steerInFlightRef.current = false;
      if (steerWatchdogRef.current !== null) {
        clearTimeout(steerWatchdogRef.current);
        steerWatchdogRef.current = null;
      }
      return;
    }

    // A previous drain hasn't resolved yet (we awaited `onSend` and
    // the daemon hasn't acked). Don't fire a second `send_turn` for
    // the same head — wait for the await to settle and the natural
    // re-render to re-enter the effect with the popped queue.
    if (drainInFlightRef.current) return;

    // Normal drain — pop the head of the queue.
    if (queued.length === 0) return;
    const [first, ...rest] = queued;
    // Clear editing state if the drained message was being edited.
    if (editingId === first.id) {
      setEditingId(null);
      setEditText("");
    }
    // Drain the head of the queue. Carry its images along with the
    // text — the pasted attachments rode in the queued chip and need
    // to fire when the queued text fires.
    //
    // Critical: we await `onSend` BEFORE popping the head from the
    // queue. The previous version popped optimistically (`setQueued(
    // rest)` ran unconditionally) and called `onSend` fire-and-
    // forget — so a daemon error (e.g. `ServerMessage::Error`,
    // session-being-torn-down race, archived-mid-await) silently
    // dropped the message: chip vanished, no toast, no new turn.
    // The new contract (see the prop docstring) is that `onSend`
    // throws on daemon error; on a throw we leave the head in the
    // queue so the user can see the chip is still pending and
    // optionally retry / steer / clear.
    drainInFlightRef.current = true;
    void (async () => {
      try {
        await onSend(first.text, first.images);
        // Success: pop the head. Object URLs revoked AFTER the send
        // succeeded so the chip thumbnail stays visible until the
        // message actually leaves; on failure the chip stays and
        // the URLs survive for an eventual retry.
        for (const img of first.images) {
          URL.revokeObjectURL(img.previewUrl);
        }
        setQueued(rest);
      } catch (err) {
        // Surface the daemon's reason to the user. The chip remains
        // in the queue so retrying is just "send another message",
        // which re-fires the drain after the next `running → ready`
        // transition (or directly, if the queue is now allowed to
        // bypass — see the dispatch logic).
        const message = err instanceof Error ? err.message : String(err);
        toast({
          description: `Couldn't send queued message: ${message}`,
          duration: 5000,
        });
      } finally {
        drainInFlightRef.current = false;
      }
    })();
  }, [sessionStatus, queued, onSend, editingId]);

  function enqueue(text: string, images: AttachedImage[]) {
    setQueued((q) => [...q, { id: newQueueId(), text, images }]);
  }

  function removeQueued(id: string) {
    if (editingId === id) {
      setEditingId(null);
      setEditText("");
    }
    setQueued((q) => {
      const target = q.find((item) => item.id === id);
      if (target) {
        for (const img of target.images) {
          URL.revokeObjectURL(img.previewUrl);
        }
      }
      return q.filter((item) => item.id !== id);
    });
  }

  /** Steer: cooperatively interrupt the current turn and dispatch a
   *  specific queued message as the next turn — both in a single
   *  daemon-side RPC. The daemon serialises interrupt →
   *  wait-for-finalize → send so we don't have to dance with status
   *  transitions on the client. Remaining queued items drain on the
   *  steered turn's natural completion via the drain effect. */
  function steerMessage(id: string) {
    if (sessionStatus !== "running") return;
    const target = queued.find((item) => item.id === id);
    if (!target) return;
    if (editingId === id) {
      setEditingId(null);
      setEditText("");
    }
    // Pluck from the queue before firing — on a successful steer the
    // chip is gone immediately; on a surfaced error the message is
    // lost from the queue, which is the same contract as a normal
    // send that returns an error.
    setQueued((q) => q.filter((item) => item.id !== id));
    // Mark the steer as in flight so the upcoming
    // running -> interrupted transition (issued by the daemon as part
    // of steer_turn's interrupt-then-send sequence) does not trip
    // the drain effect and ship the rest of the queue alongside the
    // steered message. The flag is cleared by the drain effect on
    // that next non-running tick.
    steerInFlightRef.current = true;
    // Arm the watchdog: if no transition arrives within 10s — e.g.
    // the steer's `sendMessage` rejected async and the daemon never
    // acted, or the original turn finished naturally before our
    // steer was processed and the daemon skipped the interrupt —
    // self-clear the flag so the next legitimate drain isn't
    // silently swallowed. Without this the previous code path could
    // wedge the queue forever.
    if (steerWatchdogRef.current !== null) {
      clearTimeout(steerWatchdogRef.current);
    }
    steerWatchdogRef.current = setTimeout(() => {
      steerInFlightRef.current = false;
      steerWatchdogRef.current = null;
    }, 10_000);
    // `onSteer` may be async; we don't await it here because the
    // chip pluck and the steer are intentionally decoupled (the
    // chip is gone the moment the user clicks). An async error in
    // `onSteer` will be surfaced by the parent's own toast/throw
    // path; the watchdog above is what ensures `steerInFlightRef`
    // doesn't pin true if no transition follows.
    try {
      const ret = onSteer(target.text, target.images);
      // If `onSteer` returned a Promise, attach a rejection handler
      // so an async failure clears the flag promptly (don't wait
      // for the watchdog) and surfaces a toast.
      if (ret && typeof (ret as Promise<unknown>).then === "function") {
        (ret as Promise<unknown>).catch((err) => {
          steerInFlightRef.current = false;
          if (steerWatchdogRef.current !== null) {
            clearTimeout(steerWatchdogRef.current);
            steerWatchdogRef.current = null;
          }
          const message = err instanceof Error ? err.message : String(err);
          toast({
            description: `Couldn't steer message: ${message}`,
            duration: 5000,
          });
        });
      }
    } catch (err) {
      // Synchronous throw: no transition will occur, so clear the
      // flag now to avoid silently swallowing a future legitimate
      // drain.
      steerInFlightRef.current = false;
      if (steerWatchdogRef.current !== null) {
        clearTimeout(steerWatchdogRef.current);
        steerWatchdogRef.current = null;
      }
      throw err;
    }
    // Revoke the attached image preview URLs now that we've handed
    // the encoded payload off to the daemon. Mirrors the cleanup
    // the drain effect does for naturally-drained messages.
    for (const img of target.images) {
      URL.revokeObjectURL(img.previewUrl);
    }
  }

  function startEditQueued(id: string, currentText: string) {
    setEditingId(id);
    setEditText(currentText);
  }

  function saveEditQueued() {
    if (editingId === null) return;
    const trimmed = editText.trim();
    if (trimmed.length === 0) {
      removeQueued(editingId);
    } else {
      setQueued((q) =>
        q.map((item) =>
          item.id === editingId ? { ...item, text: trimmed } : item,
        ),
      );
    }
    setEditingId(null);
    setEditText("");
  }

  function cancelEditQueued() {
    setEditingId(null);
    setEditText("");
  }

  function handleEditKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      saveEditQueued();
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation(); // prevent ChatView's Escape-to-interrupt
      cancelEditQueued();
    }
  }

  function handleEditInput() {
    const el = editTextareaRef.current;
    if (!el) return;
    autosizeTextarea(el);
  }

  function removeAttachedImage(id: string) {
    setAttachedImages((prev) => {
      const target = prev.find((img) => img.id === id);
      if (target) {
        URL.revokeObjectURL(target.previewUrl);
      }
      return prev.filter((img) => img.id !== id);
    });
  }

  /** Recompute the `@`-mention context from the live textarea at the
   *  current caret position. Shared by onChange / onKeyUp / onSelect
   *  so every interaction that can move the caret keeps the popup
   *  in sync. Passing in `nextValue` lets onChange feed the value
   *  it's about to commit (the textarea's own `value` is one render
   *  behind at that point). */
  function refreshMentionCtx(nextValue?: string) {
    const el = textareaRef.current;
    const v = nextValue ?? el?.value ?? value;
    const caret = el?.selectionStart ?? v.length;
    setMentionCtx(detectMentionContext(v, caret));
  }

  /** Accept the currently-highlighted mention: splice the picked
   *  path into the draft at the token position, add the file to
   *  the chip list (deduped), close the popup, and restore focus
   *  one past the inserted trailing space. */
  function acceptMention(path: string) {
    const el = textareaRef.current;
    if (!el || !mentionCtx) return;
    const caret = el.selectionStart ?? value.length;
    const { value: next, caret: nextCaret } = applyMentionPick(
      value,
      mentionCtx.atIndex,
      caret,
      path,
    );
    setValue(next);
    setAttachedFiles((prev) => (prev.includes(path) ? prev : [...prev, path]));
    setMentionCtx(null);
    requestAnimationFrame(() => {
      const node = textareaRef.current;
      if (!node) return;
      node.focus();
      node.selectionStart = node.selectionEnd = nextCaret;
      // The insertion may have grown the textarea past its current
      // row count — mirror `handleInput`'s autosize math.
      autosizeTextarea(node);
    });
  }

  /** Drop a file chip AND strip any matching `@<path>` tokens from
   *  the draft. The regex only targets standalone tokens (bordered
   *  by whitespace or string edges) so it can't accidentally chew
   *  into substrings of unrelated words. */
  function removeAttachedFile(path: string) {
    setAttachedFiles((prev) => prev.filter((p) => p !== path));
    const escaped = path.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const tokenRe = new RegExp(`(^|\\s)@${escaped}(?=\\s|$)`, "g");
    setValue((v) =>
      v
        .replace(tokenRe, "$1")
        // Collapse any whitespace-only runs that were adjacent to
        // the removed token back to a single space.
        .replace(/[ \t]{2,}/g, " ")
        .replace(/[ \t]+$/g, ""),
    );
  }

  /** Append an `@<path>` token to the draft text, de-duplicating
   *  against the existing chip list. Used by the drop handler for
   *  non-media files — the path is surfaced to the agent verbatim so
   *  it can invoke its `Read` tool against the absolute path.
   *
   *  Unlike `acceptMention`, this doesn't assume there's an open
   *  partial `@query` at the caret — drops come from outside the
   *  textarea. We just append at the end (with a leading space) so
   *  whatever the user was typing is preserved. */
  function appendPathMention(absPath: string) {
    setAttachedFiles((prev) =>
      prev.includes(absPath) ? prev : [...prev, absPath],
    );
    setValue((v) => {
      const needsSpace = v.length > 0 && !/\s$/.test(v);
      return `${v}${needsSpace ? " " : ""}@${absPath} `;
    });
    requestAnimationFrame(() => {
      const node = textareaRef.current;
      if (!node) return;
      node.focus();
      node.selectionStart = node.selectionEnd = node.value.length;
      autosizeTextarea(node);
    });
  }

  /** Handle a batch of absolute paths dropped onto the window. Media
   *  (image / audio / video) files are read into base64 via the Rust
   *  `read_file_as_base64` command and attached as `AttachedImage`
   *  chips so they ride through the same pipeline as clipboard
   *  images. Every other file type is surfaced as an `@file` mention
   *  chip containing the absolute path — the agent sees the path in
   *  the message text and can `Read` it.
   *
   *  Any-path policy: dropped files don't have to live inside the
   *  session's project root. This is a local-only desktop app; the
   *  user explicitly dragged the file into the composer, so we pass
   *  the absolute path straight through. */
  async function handleDroppedPaths(paths: string[]) {
    if (providerDisabled || archived || disabled) return;
    for (const absPath of paths) {
      try {
        const payload = await readFileAsBase64(absPath);
        if (payload.mediaType && isMediaMimeType(payload.mediaType)) {
          if (payload.sizeBytes > MEDIA_MAX_BYTES) {
            toast({
              description: `${payload.name} exceeds ${Math.round(
                MEDIA_MAX_BYTES / (1024 * 1024),
              )} MB, skipping.`,
              duration: 3000,
            });
            continue;
          }
          // Rebuild a blob so we get an object URL for the chip
          // thumbnail — base64 → bytes → Blob → URL. Only images get a
          // visible preview; audio/video chips fall back to the
          // generic icon path in `InFluxAttachmentChip`.
          let previewUrl = "";
          if (payload.mediaType.startsWith("image/")) {
            try {
              const bin = atob(payload.dataBase64);
              const bytes = new Uint8Array(bin.length);
              for (let i = 0; i < bin.length; i++) {
                bytes[i] = bin.charCodeAt(i);
              }
              const blob = new Blob([bytes], { type: payload.mediaType });
              previewUrl = URL.createObjectURL(blob);
            } catch {
              // Preview is best-effort — a failed decode just means
              // the chip renders the icon fallback.
              previewUrl = "";
            }
          }
          setAttachedImages((prev) => [
            ...prev,
            {
              id: newQueueId(),
              mediaType: payload.mediaType,
              dataBase64: payload.dataBase64,
              name: payload.name,
              previewUrl,
            },
          ]);
        } else {
          // Non-media file — route through the `@file` mention
          // system so the agent can `Read` the absolute path.
          appendPathMention(absPath);
        }
      } catch (err) {
        // Fallback: if Rust couldn't read the bytes for whatever
        // reason (permissions, binary fs handle, too big), we can
        // still surface the path as a mention so the user isn't left
        // with nothing. The Read tool will hit the same error later,
        // but at least the user sees the path they dropped.
        toast({
          description: `Couldn't attach ${absPath}: ${String(err)}`,
          duration: 4000,
        });
        appendPathMention(absPath);
      }
    }
  }

  // Wire the window-wide Tauri drag/drop listener. Tauri 2 only fires
  // these events when `dragDropEnabled: true` is set on the window
  // (see `tauri.conf.json`). We intentionally don't hit-test against
  // the composer's bounding rect — dropping anywhere in the app is
  // treated as "attach to the next message", which matches the
  // familiar Slack / Discord / iMessage UX.
  React.useEffect(() => {
    if (providerDisabled || archived) return;
    const webview = getCurrentWebviewWindow();
    let disposed = false;
    const unlistenPromise = webview.onDragDropEvent((event) => {
      if (disposed) return;
      const payload = event.payload;
      if (payload.type === "enter" || payload.type === "over") {
        setIsDragOver(true);
      } else if (payload.type === "leave") {
        setIsDragOver(false);
      } else if (payload.type === "drop") {
        setIsDragOver(false);
        const paths = (payload as unknown as { paths?: string[] }).paths ?? [];
        if (paths.length > 0) void handleDroppedPaths(paths);
      }
    });
    return () => {
      disposed = true;
      unlistenPromise.then((un) => un()).catch(() => {});
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [providerDisabled, archived, disabled]);

  /** Paste handler — picks up clipboard images and turns them into
   * `AttachedImage` chips. Falls through to the default text paste
   * when the clipboard contains no image entries. */
  async function handlePaste(e: React.ClipboardEvent<HTMLTextAreaElement>) {
    const items = e.clipboardData ? Array.from(e.clipboardData.items) : [];
    const imageItems = items.filter((it) => it.type.startsWith("image/"));
    if (imageItems.length === 0) return; // default text paste
    e.preventDefault();
    for (const item of imageItems) {
      const blob = item.getAsFile();
      if (!blob) continue;
      if (!ALLOWED_IMAGE_MEDIA_TYPES.has(blob.type)) {
        toast({
          description: `Unsupported image type: ${blob.type}`,
          duration: 3000,
        });
        continue;
      }
      if (blob.size > IMAGE_MAX_BYTES) {
        toast({
          description: `Image exceeds 5 MB, skipping.`,
          duration: 3000,
        });
        continue;
      }
      try {
        const dataBase64 = await blobToBase64(blob);
        const previewUrl = URL.createObjectURL(blob);
        const file = blob as File;
        setAttachedImages((prev) => [
          ...prev,
          {
            id: newQueueId(),
            mediaType: blob.type,
            dataBase64,
            name: file.name && file.name.length > 0 ? file.name : suggestedFilename(blob.type),
            previewUrl,
          },
        ]);
      } catch (err) {
        toast({
          description: `Could not read pasted image: ${(err as Error).message}`,
          duration: 4000,
        });
      }
    }
  }

  function handleSubmit() {
    if (providerDisabled || archived) return;
    const trimmed = value.trim();
    // Review comments can stand in for a non-empty message too — a user
    // might queue a few "please rename this" notes and hit Send with no
    // free-form text. Comments are snapshotted from the store (not
    // state) so use the closed-over `comments` value, then clear the
    // store once the composed text has been committed to a queue entry
    // or direct send — mirrors how attachedImages is snapshotted.
    if (!trimmed && attachedImages.length === 0 && comments.length === 0) return;
    // Snapshot images then clear state — we hand the snapshot off to
    // either the queue or onSend, so the chip row clears immediately.
    const imagesToSend = attachedImages;
    setAttachedImages([]);
    // Images-only sends need a non-empty text block: the Claude Agent
    // SDK bridge unconditionally emits `{ type: 'text', text: prompt }`
    // alongside the image blocks, and the Messages API rejects empty
    // text blocks ("text content blocks must be non-empty"). Supply a
    // neutral default so the user's "just analyze this screenshot"
    // intent goes through. Normalize here (before the queue/direct
    // branch) so drained messages get the same treatment.
    const baseText =
      !trimmed && imagesToSend.length > 0 && comments.length === 0
        ? "Analyze image"
        : trimmed;
    // Prepend the serialized review-comments block so queued/direct
    // paths both carry the review context. serializeCommentsAsPrefix
    // returns "" when the list is empty, so no-comments sends are
    // byte-identical to the pre-feature behavior.
    const prefix = serializeCommentsAsPrefix(comments);
    const textToSend = prefix
      ? baseText
        ? `${prefix}\n\n${baseText}`
        : prefix
      : baseText;
    // Clear the store now that the text has been materialized —
    // whether it goes to the queue or straight through, the chips
    // should disappear at the same moment the user hit Send. Scoped
    // by sessionId so comments for other threads are untouched.
    if (sessionId && comments.length > 0) {
      clearComments(sessionId);
    }
    // While a turn is running OR earlier messages are still queued,
    // append this one to the queue. Clearing the textarea immediately
    // mirrors what the user just did ("send"), and the queued chip
    // above the input shows what's pending. The "queued.length > 0"
    // clause is a race guard against the tiny window between onSend
    // firing and turn_started flipping sessionStatus back to
    // "running" — without it, a fast user could fire two concurrent
    // send_turn calls which the runtime rejects. That guard is
    // deliberately scoped to non-interrupted state: after a stop,
    // there is no in-flight send_turn to race against, so we let the
    // next message fire directly. The existing drain effect picks up
    // whatever was already queued once the new turn completes, which
    // is how the user's "send one more to drain" workflow above works.
    if (isRunning || (queued.length > 0 && sessionStatus !== "interrupted")) {
      enqueue(textToSend, imagesToSend);
      setValue("");
      setAttachedFiles([]);
      setMentionCtx(null);
      resetHeight();
      return;
    }
    onSend(textToSend, imagesToSend);
    // Object URLs revoked AFTER onSend so the renderer can still
    // paint the (now removed) chip's thumbnail this frame, then they
    // get freed.
    for (const img of imagesToSend) {
      URL.revokeObjectURL(img.previewUrl);
    }
    setValue("");
    setAttachedFiles([]);
    setMentionCtx(null);
    resetHeight();
  }

  function handlePopupSelect(name: string) {
    // Core app commands (e.g. /flowstate-clear) fire immediately —
    // they don't take arguments. Everything else (provider built-ins,
    // user skills, agents) pre-fills the composer so the user can
    // optionally append args before pressing Enter again.
    if (isCoreCommand(name)) {
      const cmd = `/${name}`;
      lastSelectedSkillTokenRef.current = null;
      setValue("");
      resetHeight();
      onSend(cmd, []);
      return;
    }
    const invocation = formatSkillInvocation(name, provider);
    const token = `${invocation} `;
    lastSelectedSkillTokenRef.current = invocation;
    setValue(token);
    requestAnimationFrame(() => {
      const el = textareaRef.current;
      if (!el) return;
      el.focus();
      el.selectionStart = el.selectionEnd = el.value.length;
      autosizeTextarea(el);
    });
  }

  function handleKeyDown(e: React.KeyboardEvent) {
    // --- Prompt-suggestion ghost text ---
    // Only active when the composer is empty AND a suggestion
    // exists. Tab accepts, Esc dismisses. Any other printable key
    // dismisses the ghost so the user's typing doesn't collide
    // with a stale prediction; the onChange below handles that
    // path (this block only handles non-character keys like Tab /
    // Esc that don't route through onChange).
    const hasSuggestion =
      !!promptSuggestion && value.length === 0 && !disabled;
    if (hasSuggestion && e.key === "Tab" && !showPopup) {
      e.preventDefault();
      setValue(promptSuggestion!);
      onPromptSuggestionDismissed?.();
      return;
    }
    if (hasSuggestion && e.key === "Escape" && !showPopup) {
      e.preventDefault();
      onPromptSuggestionDismissed?.();
      return;
    }

    // --- Mention autocomplete keyboard navigation ---
    // Sits before the slash-popup branch. The two are mutually
    // exclusive via `showMentionPopup`'s `!showPopup` guard, but
    // putting mention first keeps the flow obvious: if a mention
    // is live, the arrow/Enter/Tab keys belong to it.
    if (showMentionPopup) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setMentionIndex((i) => (i + 1) % mentionMatches.length);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        setMentionIndex(
          (i) => (i - 1 + mentionMatches.length) % mentionMatches.length,
        );
        return;
      }
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        const pick = mentionMatches[mentionIndex];
        if (pick) acceptMention(pick);
        return;
      }
      if (e.key === "Tab") {
        e.preventDefault();
        const pick = mentionMatches[mentionIndex];
        if (pick) acceptMention(pick);
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        // Stop propagation so ChatView's Esc-to-interrupt doesn't
        // fire on the same keystroke.
        e.stopPropagation();
        setMentionCtx(null);
        return;
      }
    }

    // --- Autocomplete keyboard navigation ---
    if (showPopup && matches.length > 0) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setPopupIndex((i) => (i + 1) % matches.length);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        setPopupIndex((i) => (i - 1 + matches.length) % matches.length);
        return;
      }
      if (e.key === "Tab") {
        // Tab fills the command name into the textarea (user can append args).
        e.preventDefault();
        const selected = matches[popupIndex];
        if (selected) {
          setValue(`/${selected.name} `);
        }
        return;
      }
      if (e.key === "Escape") {
        // Close the popup by clearing the slash prefix.
        e.preventDefault();
        e.stopPropagation(); // prevent ChatView's Escape-to-interrupt
        setValue("");
        resetHeight();
        return;
      }
      // Enter with popup open — submit the highlighted command.
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        // If the user already picked a non-core command (via an
        // earlier Enter or click) and is just pressing Enter again to
        // send the pre-filled invocation, treat this as a submit rather
        // than re-selecting and looping forever on the same item.
        const pending = lastSelectedSkillTokenRef.current;
        if (pending && value.trim().startsWith(pending)) {
          lastSelectedSkillTokenRef.current = null;
          handleSubmit();
          return;
        }
        const selected = matches[popupIndex];
        if (selected) {
          handlePopupSelect(selected.name);
        }
        return;
      }
    }

    // --- Default Enter handling ---
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSubmit();
    }
  }

  function handleInput() {
    const el = textareaRef.current;
    if (!el) return;
    autosizeTextarea(el);
  }

  const hasText = value.trim().length > 0;
  const hasAttachments = attachedImages.length > 0;
  // Comments behave like attachments for the send-affordance: they
  // make the Send button active and suppress the Stop-while-idle
  // swap, because the outgoing message would be non-empty (the
  // serialized comments prefix) even when the textarea is empty.
  const hasContent = hasText || hasAttachments || comments.length > 0;
  // Stop button shows whenever the turn is running and the user isn't
  // mid-compose. Queued chips are intentionally NOT a precondition --
  // interrupting only stops the current turn and leaves the queue
  // intact, so the user can always reach the stop affordance.
  const showStop = isRunning && !hasContent && !providerDisabled && !archived;
  const sendDisabled =
    !hasContent || disabled || providerDisabled || archived;

  return (
    // Queued chips live OUTSIDE the bordered composer so they float above
    // the divider in the chat area, not inside the composer box. When the
    // queue is empty the extra wrapper collapses and the composer renders
    // exactly as it did before.
    //
    // The wrapper is a bounded flex column rather than `shrink-0` so the
    // composer yields when its parent's column runs out of room — without
    // that, the composer punches out the bottom of ChatView's clip
    // boundary in short windows / shrunk popouts and loses its toolbar.
    // `max-h-[50vh]` is generous (a normal composer is ~40–280 px) but
    // stops a runaway chip stack from eating the entire window.
    <div className="flex min-h-0 shrink flex-col" style={{ maxHeight: "50vh" }}>
      {queued.length > 0 && (
        <div
          className="shrink-0 overflow-y-auto px-3 pb-1 pt-2"
          style={{ maxHeight: "30vh" }}
        >
          <div className="space-y-1">
            {queued.map((item, idx) => (
              <div
                key={item.id}
                className="flex items-start gap-2 rounded-md border border-border bg-muted/40 px-2.5 py-1.5 text-xs"
              >
                <Clock className="mt-0.5 h-3 w-3 shrink-0 text-muted-foreground" />
                <div className="min-w-0 flex-1">
                  <div className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
                    Queued{queued.length > 1 ? ` · ${idx + 1} of ${queued.length}` : ""}
                  </div>
                  {editingId === item.id ? (
                    <textarea
                      ref={editTextareaRef}
                      value={editText}
                      onChange={(e) => setEditText(e.target.value)}
                      onKeyDown={handleEditKeyDown}
                      onBlur={() => requestAnimationFrame(() => saveEditQueued())}
                      onInput={handleEditInput}
                      rows={1}
                      className="mt-0.5 w-full resize-none rounded border border-input bg-background px-1.5 py-1 text-xs text-foreground/85 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
                    />
                  ) : (
                    <div className="mt-0.5 break-words whitespace-pre-wrap text-foreground/85">
                      {item.text}
                    </div>
                  )}
                </div>
                {editingId === item.id ? null : (
                  <>
                    {isRunning && (
                      <button
                        type="button"
                        onClick={() => steerMessage(item.id)}
                        className="mt-0.5 shrink-0 rounded p-0.5 text-muted-foreground hover:bg-primary/10 hover:text-primary"
                        title="Send now (interrupts current turn)"
                      >
                        <Send className="h-3 w-3" />
                      </button>
                    )}
                    <button
                      type="button"
                      onClick={() => startEditQueued(item.id, item.text)}
                      className="mt-0.5 shrink-0 rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-accent-foreground"
                      title="Edit queued message"
                    >
                      <Pencil className="h-3 w-3" />
                    </button>
                    <button
                      type="button"
                      onClick={() => removeQueued(item.id)}
                      className="mt-0.5 shrink-0 rounded p-0.5 text-muted-foreground hover:bg-destructive/10 hover:text-destructive"
                      title="Remove from queue"
                    >
                      <Trash2 className="h-3 w-3" />
                    </button>
                  </>
                )}
              </div>
            ))}
          </div>
        </div>
      )}
      <div
        className={cn(
          "flex min-h-0 flex-1 flex-col border-t border-border px-3 pb-2 pt-3 transition-colors",
          // While a drag is over the window we paint a subtle
          // primary-tinted tint + border over the composer surface
          // to signal "drop here to attach". Cleared on leave/drop
          // via the Tauri `onDragDropEvent` listener above.
          isDragOver &&
            "border-primary/70 bg-primary/5 ring-1 ring-primary/40",
        )}
      >
        <div className="flex min-h-0 flex-1 flex-col">
          {(attachedImages.length > 0 ||
            attachedFiles.length > 0 ||
            comments.length > 0) && (
            <div className="mb-2 flex max-h-20 shrink-0 flex-wrap gap-1 overflow-y-auto">
              {attachedImages.map((img) => (
                <InFluxAttachmentChip
                  key={img.id}
                  image={img}
                  onRemove={() => removeAttachedImage(img.id)}
                  onOpen={() =>
                    setLightboxSource({ kind: "inflight", image: img })
                  }
                />
              ))}
              {attachedFiles.map((p) => (
                <FileMentionChip
                  key={p}
                  path={p}
                  onRemove={() => removeAttachedFile(p)}
                />
              ))}
              {/* Review-comment chips seeded by DiffCommentOverlay.
                  Update/remove are scoped to sessionId so tab-switch
                  clean-up is implicit: chips vanish when the
                  composer re-keys onto a different session. */}
              {sessionId &&
                comments.map((c) => (
                  <CommentChip
                    key={c.id}
                    comment={c}
                    onUpdate={(text) => updateComment(sessionId, c.id, text)}
                    onRemove={() => removeComment(sessionId, c.id)}
                  />
                ))}
            </div>
          )}
          <div className="relative flex min-h-0 flex-1 items-end gap-2">
            {/* Autocomplete popup — positioned above the textarea */}
            {showPopup && matches.length > 0 && (
              <SlashCommandPopup
                matches={matches}
                selectedIndex={popupIndex}
                onSelect={handlePopupSelect}
              />
            )}
            {showMentionPopup && (
              <MentionPopup
                matches={mentionMatches}
                totalMatches={mentionTotalMatches}
                selectedIndex={mentionIndex}
                onSelect={acceptMention}
              />
            )}

            <div className="relative flex min-h-0 flex-1">
              <textarea
                ref={textareaRef}
                value={value}
                onChange={(e) => {
                  // Any keystroke that writes to the textarea
                  // dismisses the ghost-text suggestion. The
                  // user is clearly going a different direction;
                  // keeping the prediction visible would just
                  // fight with their typing.
                  if (promptSuggestion) {
                    onPromptSuggestionDismissed?.();
                  }
                  setValue(e.target.value);
                  // Recompute the `@`-mention context against the
                  // about-to-be-committed value. onChange fires before
                  // the state flush, so we can't read `value` here —
                  // pass the fresh string in directly.
                  refreshMentionCtx(e.target.value);
                }}
                onKeyDown={handleKeyDown}
                // Arrow keys / Home / End / mouse clicks move the
                // caret without firing onChange. Re-detect on
                // keyUp + select so the popup tracks caret position
                // even when the text is unchanged.
                onKeyUp={() => refreshMentionCtx()}
                onSelect={() => refreshMentionCtx()}
                onInput={handleInput}
                onPaste={handlePaste}
                placeholder={
                  archived
                    ? "Archived thread — read-only"
                    : providerDisabled
                      ? "Provider disabled — re-enable it in Settings to send"
                      : promptSuggestion && value.length === 0 && !disabled
                        ? ""
                        : queued.length > 0
                          ? "Compose another message…"
                          : "Send a message..."
                }
                disabled={disabled || providerDisabled || archived}
                rows={1}
                className={cn(
                  // No fixed `h-10` here on purpose — the JS autosizer
                  // (`autosizeTextarea`) is the sole source of truth
                  // for the height. A class-set height fights the
                  // inline `style.height` until the first input event,
                  // producing a one-frame jump on draft restore.
                  // `min-h-10` keeps the empty-state floor; `w-full`
                  // makes the textarea fill the flex wrapper around
                  // it (which carries the width).
                  "block min-h-10 w-full resize-none rounded-lg border px-3 py-2 text-sm leading-5 ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 disabled:cursor-not-allowed disabled:opacity-50",
                  // Mode tint. Plan, bypass, and auto are the modes
                  // where the next send behaves *differently* from the
                  // defaults, so they each get a coloured border and a
                  // subtle L→R fade matching the WorkingIndicator's
                  // spinner tone (see `toneForMode` / BrailleSpinner).
                  // Default / accept_edits keep the neutral look so the
                  // tint only draws the eye when it matters.
                  permissionMode === "plan"
                    ? "border-blue-500/60 bg-gradient-to-r from-blue-500/10 to-transparent focus-visible:ring-blue-500/60"
                    : permissionMode === "bypass"
                      ? "border-orange-500/60 bg-gradient-to-r from-orange-500/10 to-transparent focus-visible:ring-orange-500/60"
                      : permissionMode === "auto"
                        ? "border-green-500/60 bg-gradient-to-r from-green-500/10 to-transparent focus-visible:ring-green-500/60"
                        : "border-input bg-background focus-visible:ring-ring",
                )}
              />
              {/* Ghost-text overlay for prompt-suggestion. Only
                  shown when the composer is empty and a suggestion
                  exists. Absolutely positioned over the textarea
                  so it sits where typed text would appear; the
                  pointer-events-none + muted tone + Tab hint make
                  it unambiguously a preview rather than real
                  content. */}
              {promptSuggestion &&
                value.length === 0 &&
                !disabled &&
                !providerDisabled &&
                !archived && (
                  <div
                    aria-hidden
                    className="pointer-events-none absolute inset-0 flex items-start px-3 py-2 text-sm text-muted-foreground/50"
                  >
                    <span className="truncate">
                      {promptSuggestion}
                      <span className="ml-2 rounded border border-border/50 bg-muted/60 px-1 py-0.5 text-[10px] font-medium text-muted-foreground/80">
                        Tab
                      </span>
                    </span>
                  </div>
                )}
            </div>

            {showStop ? (
              <button
                type="button"
                onClick={onInterrupt}
                className="inline-flex h-10 w-10 shrink-0 items-center justify-center rounded-lg bg-destructive text-destructive-foreground hover:bg-destructive/90"
                title="Interrupt (Esc Esc)"
              >
                <Square className="h-4 w-4" />
              </button>
            ) : (
              <button
                type="button"
                onClick={handleSubmit}
                disabled={sendDisabled}
                className="inline-flex h-10 w-10 shrink-0 items-center justify-center rounded-lg bg-primary text-primary-foreground hover:bg-primary/90 disabled:pointer-events-none disabled:opacity-50"
                title={
                  isRunning || queued.length > 0
                    ? "Add to queue (fires when current turn ends)"
                    : "Send"
                }
              >
                <Send className="h-4 w-4" />
              </button>
            )}
          </div>
          {/* Indent the toolbar so its left edge lines up with the
              textarea's *text* (textarea = rounded border + px-3
              inside, so text sits 13px inside the textarea wrapper).
              Right padding mirrors the left so `-- / --` stays inside
              the composer outline rather than bleeding past the send
              button. Inline style, not a Tailwind arbitrary value,
              so the class is guaranteed to ship even if JIT fails. */}
          {toolbar && (
            <div
              className="mt-1.5"
              style={{ paddingLeft: 13, paddingRight: 13 }}
            >
              {toolbar}
            </div>
          )}
        </div>
      </div>
      {lightboxSource && (
        <ImageLightbox
          source={lightboxSource}
          onClose={() => setLightboxSource(null)}
          onRemove={
            lightboxSource.kind === "inflight"
              ? () => removeAttachedImage(lightboxSource.image.id)
              : undefined
          }
        />
      )}
    </div>
  );
}
