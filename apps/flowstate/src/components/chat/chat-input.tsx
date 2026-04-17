import * as React from "react";
import { Clock, Pencil, Send, Square, Trash2 } from "lucide-react";
import type { AttachedImage, ProviderKind, SessionStatus } from "@/lib/types";
import {
  formatSkillInvocation,
  getCompletions,
  isCoreCommand,
  type SlashCommandItem,
} from "@/lib/slash-commands";
import { toast } from "@/hooks/use-toast";
import { SlashCommandPopup } from "./slash-command-popup";
import { InFluxAttachmentChip } from "./attachment-chip";
import { ImageLightbox, type LightboxSource } from "./image-lightbox";

interface ChatInputProps {
  onSend: (input: string, images: AttachedImage[]) => void;
  onInterrupt: () => void;
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
}

export interface QueuedMessage {
  id: string;
  text: string;
  images: AttachedImage[];
}

/** Per-image cap, mirrors `ATTACHMENT_MAX_BYTES` on the Rust side. */
const IMAGE_MAX_BYTES = 5 * 1024 * 1024;
/** Allowed clipboard image MIME types — matches the Rust validator. */
const ALLOWED_IMAGE_MEDIA_TYPES = new Set([
  "image/png",
  "image/jpeg",
  "image/gif",
  "image/webp",
]);

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
}: ChatInputProps) {
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
  const [lightboxSource, setLightboxSource] = React.useState<LightboxSource | null>(
    null,
  );
  const [editingId, setEditingId] = React.useState<string | null>(null);
  const [editText, setEditText] = React.useState("");
  const textareaRef = React.useRef<HTMLTextAreaElement>(null);
  const editTextareaRef = React.useRef<HTMLTextAreaElement>(null);
  const steerRef = React.useRef<QueuedMessage | null>(null);

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
    el.focus();
    // When restoring a saved draft the textarea starts at rows=1;
    // auto-size it so the full draft is visible on mount.
    if (el.value.length > 0) {
      el.style.height = "auto";
      el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
    }
  }, [disabled, providerDisabled, archived]);

  // Auto-focus the inline edit textarea when entering edit mode.
  React.useEffect(() => {
    if (editingId && editTextareaRef.current) {
      const el = editTextareaRef.current;
      el.focus();
      el.selectionStart = el.selectionEnd = el.value.length;
      el.style.height = "auto";
      el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
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
  React.useEffect(() => {
    const wasRunning = prevStatusRef.current === "running";
    const nowReady = sessionStatus === "ready" || sessionStatus === "interrupted";
    prevStatusRef.current = sessionStatus;
    if (!wasRunning || !nowReady) return;

    // Steer takes priority over normal queue drain. When the user
    // clicked "Send now" on a specific queued message, steerRef holds
    // that message — send it instead of the queue head, then return so
    // the remaining queue items drain on subsequent turn completions.
    const steered = steerRef.current;
    if (steered) {
      steerRef.current = null;
      if (editingId === steered.id) {
        setEditingId(null);
        setEditText("");
      }
      onSend(steered.text, steered.images);
      for (const img of steered.images) {
        URL.revokeObjectURL(img.previewUrl);
      }
      return;
    }

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
    // to fire when the queued text fires. Object URLs are revoked
    // here, after the send goes out, so the chip thumbnail stays
    // visible until the moment the message actually leaves.
    onSend(first.text, first.images);
    for (const img of first.images) {
      URL.revokeObjectURL(img.previewUrl);
    }
    setQueued(rest);
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

  /** Steer: interrupt the current turn and send a specific queued
   *  message immediately, plucking it from the queue while preserving
   *  the order of remaining items. The drain effect picks up the
   *  steered message (via steerRef) on the running→interrupted
   *  transition instead of popping the queue head. */
  function steerMessage(id: string) {
    if (sessionStatus !== "running") return;
    const target = queued.find((item) => item.id === id);
    if (!target) return;
    if (editingId === id) {
      setEditingId(null);
      setEditText("");
    }
    steerRef.current = target;
    // Remove from queue WITHOUT revoking image URLs — the drain
    // effect will revoke them after sending the steered message.
    setQueued((q) => q.filter((item) => item.id !== id));
    onInterrupt();
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
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
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
    if (!trimmed && attachedImages.length === 0) return;
    // Snapshot images then clear state — we hand the snapshot off to
    // either the queue or onSend, so the chip row clears immediately.
    const imagesToSend = attachedImages;
    setAttachedImages([]);
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
      enqueue(trimmed, imagesToSend);
      setValue("");
      resetHeight();
      return;
    }
    onSend(trimmed, imagesToSend);
    // Object URLs revoked AFTER onSend so the renderer can still
    // paint the (now removed) chip's thumbnail this frame, then they
    // get freed.
    for (const img of imagesToSend) {
      URL.revokeObjectURL(img.previewUrl);
    }
    setValue("");
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
      el.style.height = "auto";
      el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
    });
  }

  function handleKeyDown(e: React.KeyboardEvent) {
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
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
  }

  const hasText = value.trim().length > 0;
  const hasAttachments = attachedImages.length > 0;
  const hasContent = hasText || hasAttachments;
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
    <div className="shrink-0">
      {queued.length > 0 && (
        <div className="px-3 pb-1 pt-2">
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
      <div className="border-t border-border px-3 pb-2 pt-3">
        <div>
          {attachedImages.length > 0 && (
            <div className="mb-2 flex flex-wrap gap-1">
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
            </div>
          )}
          <div className="relative flex items-end gap-2">
            {/* Autocomplete popup — positioned above the textarea */}
            {showPopup && matches.length > 0 && (
              <SlashCommandPopup
                matches={matches}
                selectedIndex={popupIndex}
                onSelect={handlePopupSelect}
              />
            )}

            <textarea
              ref={textareaRef}
              value={value}
              onChange={(e) => setValue(e.target.value)}
              onKeyDown={handleKeyDown}
              onInput={handleInput}
              onPaste={handlePaste}
              placeholder={
                archived
                  ? "Archived thread — read-only"
                  : providerDisabled
                    ? "Provider disabled — re-enable it in Settings to send"
                    : queued.length > 0
                      ? "Compose another message…"
                      : "Send a message..."
              }
              disabled={disabled || providerDisabled || archived}
              rows={1}
              className="flex-1 resize-none rounded-lg border border-input bg-background px-3 py-2 text-sm ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
            />

            {showStop ? (
              <button
                type="button"
                onClick={onInterrupt}
                className="inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-destructive text-destructive-foreground hover:bg-destructive/90"
                title="Interrupt (Esc Esc)"
              >
                <Square className="h-4 w-4" />
              </button>
            ) : (
              <button
                type="button"
                onClick={handleSubmit}
                disabled={sendDisabled}
                className="inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-primary text-primary-foreground hover:bg-primary/90 disabled:pointer-events-none disabled:opacity-50"
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
          {toolbar && <div className="mt-1.5">{toolbar}</div>}
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
