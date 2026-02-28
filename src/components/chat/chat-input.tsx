import * as React from "react";
import { Send, Square } from "lucide-react";
import type { SessionStatus } from "@/lib/types";

interface ChatInputProps {
  onSend: (input: string) => void;
  onInterrupt: () => void;
  sessionStatus: SessionStatus | undefined;
  disabled: boolean;
  toolbar?: React.ReactNode;
}

export function ChatInput({
  onSend,
  onInterrupt,
  sessionStatus,
  disabled,
  toolbar,
}: ChatInputProps) {
  const [value, setValue] = React.useState("");
  const [pendingSend, setPendingSend] = React.useState(false);
  const textareaRef = React.useRef<HTMLTextAreaElement>(null);

  const isRunning = sessionStatus === "running";

  function resetHeight() {
    if (textareaRef.current) {
      textareaRef.current.style.height = "auto";
    }
  }

  // Flush the queued send when the current turn ends. If the turn was
  // interrupted (user pressed Stop/Esc), drop the queue — the interrupt
  // is an explicit "stop everything" signal, so firing a follow-up turn
  // right after would be surprising. A natural "ready" transition fires
  // whatever text is currently in the textarea at that moment (user may
  // have kept typing while waiting).
  React.useEffect(() => {
    if (!pendingSend) return;
    if (sessionStatus === "running") return;
    if (sessionStatus === "interrupted") {
      setPendingSend(false);
      return;
    }
    const trimmed = value.trim();
    if (!trimmed) {
      setPendingSend(false);
      return;
    }
    onSend(trimmed);
    setValue("");
    setPendingSend(false);
    resetHeight();
  }, [sessionStatus, pendingSend, value, onSend]);

  function handleSubmit() {
    const trimmed = value.trim();
    if (isRunning) {
      // While a turn is running, Send queues the current textarea
      // contents to fire when the turn ends. Clicking again toggles
      // the queue back off so the user can change their mind.
      if (pendingSend) {
        setPendingSend(false);
        return;
      }
      if (!trimmed) return;
      setPendingSend(true);
      return;
    }
    if (!trimmed) return;
    onSend(trimmed);
    setValue("");
    setPendingSend(false);
    resetHeight();
  }

  function handleKeyDown(e: React.KeyboardEvent) {
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
  // Stop button shows only when the turn is running AND the user has
  // nothing queued — an empty textarea with no pending message means
  // the primary action the user wants is to cancel.
  const showStop = isRunning && !hasContent && !pendingSend;
  const sendDisabled = (!hasContent && !pendingSend) || disabled;

  return (
    <div className="shrink-0 border-t border-border px-3 pb-2 pt-3">
      <div className="mx-auto max-w-3xl">
        <div className="flex items-end gap-2">
          <textarea
            ref={textareaRef}
            value={value}
            onChange={(e) => setValue(e.target.value)}
            onKeyDown={handleKeyDown}
            onInput={handleInput}
            placeholder="Send a message..."
            disabled={disabled}
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
              className={`inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-primary text-primary-foreground hover:bg-primary/90 disabled:pointer-events-none disabled:opacity-50 ${
                pendingSend ? "ring-2 ring-primary/60" : ""
              }`}
              title={
                pendingSend
                  ? "Queued — click to cancel"
                  : isRunning
                    ? "Queue send (fires when turn ends)"
                    : "Send"
              }
            >
              <Send className="h-4 w-4" />
            </button>
          )}
        </div>
        {pendingSend && (
          <div className="mt-1 text-[11px] text-muted-foreground">
            Queued — will send when the current turn ends
          </div>
        )}
        {toolbar && <div className="mt-1.5">{toolbar}</div>}
      </div>
    </div>
  );
}
