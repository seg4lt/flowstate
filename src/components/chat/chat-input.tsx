import * as React from "react";
import { Clock, Send, Square, X } from "lucide-react";
import type { SessionStatus } from "@/lib/types";
import { getCompletions } from "@/lib/slash-commands";
import { SlashCommandPopup } from "./slash-command-popup";

interface ChatInputProps {
  onSend: (input: string) => void;
  onInterrupt: () => void;
  sessionStatus: SessionStatus | undefined;
  disabled: boolean;
  /** When true, the session's provider has been toggled off in
   *  Settings — the composer is locked read-only until the user
   *  re-enables it. Distinct from `disabled` which is about transient
   *  loading states. */
  providerDisabled?: boolean;
  toolbar?: React.ReactNode;
  /** Command metadata for the autocomplete popup. */
  commands?: { name: string; description: string }[];
}

interface QueuedMessage {
  id: string;
  text: string;
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
  toolbar,
}: ChatInputProps) {
  const [value, setValue] = React.useState("");
  const [queued, setQueued] = React.useState<QueuedMessage[]>([]);
  const [popupIndex, setPopupIndex] = React.useState(0);
  const textareaRef = React.useRef<HTMLTextAreaElement>(null);

  const isRunning = sessionStatus === "running";

  // --- Slash command autocomplete ---
  // Show popup when the input starts with "/" and the session isn't busy.
  const inputToken = value.trim().split(/\s/)[0] ?? "";
  const showPopup = inputToken.startsWith("/") && !isRunning && !disabled;
  const matches = showPopup ? getCompletions(inputToken) : [];

  // Reset the highlighted index when the match list changes.
  React.useEffect(() => {
    setPopupIndex(0);
  }, [matches.length, inputToken]);

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
  // overlapping send_turn calls. The explicit interrupt path leaves
  // the queue intact (status flips to "interrupted", not "ready"),
  // so the user can choose whether to drain by sending one more
  // message themselves or to clear chips manually.
  const prevStatusRef = React.useRef(sessionStatus);
  React.useEffect(() => {
    const wasRunning = prevStatusRef.current === "running";
    const nowReady = sessionStatus === "ready";
    prevStatusRef.current = sessionStatus;
    if (!wasRunning || !nowReady) return;
    if (queued.length === 0) return;
    const [first, ...rest] = queued;
    onSend(first.text);
    setQueued(rest);
  }, [sessionStatus, queued, onSend]);

  function enqueue(text: string) {
    setQueued((q) => [...q, { id: newQueueId(), text }]);
  }

  function removeQueued(id: string) {
    setQueued((q) => q.filter((item) => item.id !== id));
  }

  function handleSubmit() {
    if (providerDisabled) return;
    const trimmed = value.trim();
    if (!trimmed) return;
    // While a turn is running OR earlier messages are still queued,
    // append this one to the queue. Clearing the textarea immediately
    // mirrors what the user just did ("send"), and the queued chip
    // above the input shows what's pending. The earlier UX kept the
    // text in the textarea which felt like the message hadn't been
    // accepted at all.
    if (isRunning || queued.length > 0) {
      enqueue(trimmed);
      setValue("");
      resetHeight();
      return;
    }
    onSend(trimmed);
    setValue("");
    resetHeight();
  }

  function handlePopupSelect(name: string) {
    // Fill the command and immediately submit.
    const cmd = `/${name}`;
    setValue("");
    resetHeight();
    onSend(cmd);
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

  const hasContent = value.trim().length > 0;
  // Stop button shows whenever the turn is running and the user isn't
  // mid-compose. Queued chips are intentionally NOT a precondition --
  // interrupting only stops the current turn and leaves the queue
  // intact, so the user can always reach the stop affordance.
  const showStop = isRunning && !hasContent && !providerDisabled;
  const sendDisabled = !hasContent || disabled || providerDisabled;

  return (
    // Queued chips live OUTSIDE the bordered composer so they float above
    // the divider in the chat area, not inside the composer box. When the
    // queue is empty the extra wrapper collapses and the composer renders
    // exactly as it did before.
    <div className="shrink-0">
      {queued.length > 0 && (
        <div className="px-3 pb-1 pt-2">
          <div className="mx-auto max-w-3xl space-y-1">
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
                  <div className="mt-0.5 break-words whitespace-pre-wrap text-foreground/85">
                    {item.text}
                  </div>
                </div>
                <button
                  type="button"
                  onClick={() => removeQueued(item.id)}
                  className="mt-0.5 shrink-0 rounded p-0.5 text-muted-foreground hover:bg-destructive/10 hover:text-destructive"
                  title="Remove from queue"
                >
                  <X className="h-3 w-3" />
                </button>
              </div>
            ))}
          </div>
        </div>
      )}
      <div className="border-t border-border px-3 pb-2 pt-3">
        <div className="mx-auto max-w-3xl">
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
              placeholder={
                providerDisabled
                  ? "Provider disabled — re-enable it in Settings to send"
                  : queued.length > 0
                    ? "Compose another message…"
                    : "Send a message..."
              }
              disabled={disabled || providerDisabled}
              rows={1}
              className="flex-1 resize-none rounded-lg border border-input bg-background px-3 py-2 text-sm ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
            />

            {showStop ? (
              <button
                type="button"
                onClick={onInterrupt}
                className="inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-destructive text-destructive-foreground hover:bg-destructive/90"
                title="Interrupt (Esc)"
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
    </div>
  );
}
