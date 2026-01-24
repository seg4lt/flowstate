import { useCallback } from "react";
import { Send, Square } from "lucide-react";
import { Button } from "../ui/button";
import { ProviderModelPicker } from "./ProviderModelPicker";
import { ReasoningEffortPicker } from "./ReasoningEffortPicker";
import { ModeSelector } from "./ModeSelector";
import {
  actions,
  selectProviderStatuses,
  useAppStore,
  type SendClientMessage,
} from "../../state/appStore";
import type { ProviderKind, SessionDetail } from "../../types";

interface Props {
  activeSession: SessionDetail;
  sendClientMessage: SendClientMessage;
}

export function ComposerFooter({ activeSession, sendClientMessage }: Props) {
  const composer = useAppStore((s) => s.composer);
  const providers = useAppStore(selectProviderStatuses);
  const isRunning = activeSession.summary.status === "running";

  const effectiveProvider: ProviderKind = activeSession.summary.provider;
  const effectiveModel = activeSession.summary.model ?? composer.model;

  const handleSend = useCallback(() => {
    const prompt = composer.prompt.trim();
    if (!prompt || isRunning) return;
    const pendingId = crypto.randomUUID();
    actions.optimisticSendTurn(activeSession.summary.sessionId, prompt, pendingId);
    sendClientMessage({
      type: "send_turn",
      session_id: activeSession.summary.sessionId,
      input: prompt,
      permission_mode: composer.permissionMode,
      reasoning_effort: composer.reasoningEffort,
    });
    actions.setPrompt("");
  }, [
    composer.prompt,
    composer.permissionMode,
    composer.reasoningEffort,
    isRunning,
    activeSession.summary.sessionId,
    sendClientMessage,
  ]);

  const handleInterrupt = useCallback(() => {
    sendClientMessage({
      type: "interrupt_turn",
      session_id: activeSession.summary.sessionId,
    });
  }, [activeSession.summary.sessionId, sendClientMessage]);

  return (
    <div className="border-t border-border bg-sidebar px-4 py-3">
      <div className="max-w-3xl mx-auto">
        <div className="relative">
          <textarea
            value={composer.prompt}
            onChange={(e) => actions.setPrompt(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                handleSend();
              }
            }}
            placeholder="Ask anything..."
            className="w-full min-h-[80px] max-h-[260px] bg-background border border-input rounded-lg p-3 pr-12 resize-none focus:outline-none focus:ring-1 focus:ring-ring text-sm"
          />
          {isRunning ? (
            <Button
              size="icon"
              variant="destructive"
              className="absolute bottom-3 right-3 h-8 w-8"
              onClick={handleInterrupt}
              title="Interrupt turn"
            >
              <Square className="h-3.5 w-3.5" />
            </Button>
          ) : (
            <Button
              size="icon"
              className="absolute bottom-3 right-3 h-8 w-8"
              disabled={!composer.prompt.trim()}
              onClick={handleSend}
              title="Send"
            >
              <Send className="h-4 w-4" />
            </Button>
          )}
        </div>
        <div className="flex items-center gap-1 mt-2">
          <ProviderModelPicker
            provider={effectiveProvider}
            model={effectiveModel}
            providers={providers}
            disabled
            onChange={(p, m) => actions.setProviderAndModel(p, m)}
          />
          <span className="text-border">|</span>
          <ReasoningEffortPicker
            value={composer.reasoningEffort}
            onChange={actions.setReasoningEffort}
          />
          <span className="text-border">|</span>
          <ModeSelector
            value={composer.permissionMode}
            onChange={actions.setPermissionMode}
          />
          <div className="flex-1" />
          <span className="text-[11px] text-muted-foreground">
            {isRunning ? "Running..." : "Enter to send · Shift+Enter for newline"}
          </span>
        </div>
      </div>
    </div>
  );
}
