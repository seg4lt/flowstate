import * as React from "react";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { useApp } from "@/stores/app-store";
import type { PermissionDecision, TurnRecord } from "@/lib/types";
import { connectStream, sendMessage } from "@/lib/api";
import { MessageList } from "./message-list";
import { ChatInput } from "./chat-input";
import { PermissionDialog } from "./permission-dialog";

interface PermissionRequest {
  requestId: string;
  toolName: string;
  input: unknown;
  suggested: string;
}

export function ChatView({ sessionId }: { sessionId: string }) {
  const { state, dispatch } = useApp();
  const [turns, setTurns] = React.useState<TurnRecord[]>([]);
  const [loading, setLoading] = React.useState(true);
  const [pendingPermission, setPendingPermission] =
    React.useState<PermissionRequest | null>(null);

  const session = state.sessions.get(sessionId);
  const streaming = state.streamingTurns.get(sessionId);

  // Set active session
  React.useEffect(() => {
    dispatch({ type: "set_active_session", sessionId });
    return () => {
      dispatch({ type: "set_active_session", sessionId: null });
    };
  }, [sessionId, dispatch]);

  // Load session detail
  React.useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setTurns([]);
    setPendingPermission(null);

    sendMessage({ type: "load_session", session_id: sessionId }).then((res) => {
      if (cancelled) return;
      if (res && res.type === "session_loaded") {
        setTurns(res.session.turns);
      }
      setLoading(false);
    });

    return () => {
      cancelled = true;
    };
  }, [sessionId]);

  // Listen for session-specific events via a dedicated stream
  React.useEffect(() => {
    let active = true;

    connectStream((message) => {
      if (!active) return;
      if (message.type !== "event") return;
      const event = message.event;

      if (!("session_id" in event) || event.session_id !== sessionId) return;

      switch (event.type) {
        case "turn_completed":
          setTurns((prev) => {
            const exists = prev.some((t) => t.turnId === event.turn.turnId);
            if (exists) {
              return prev.map((t) =>
                t.turnId === event.turn.turnId ? event.turn : t,
              );
            }
            return [...prev, event.turn];
          });
          break;

        case "permission_requested":
          setPendingPermission({
            requestId: event.request_id,
            toolName: event.tool_name,
            input: event.input,
            suggested: event.suggested,
          });
          break;
      }
    });

    return () => {
      active = false;
    };
  }, [sessionId]);

  async function handleSend(input: string) {
    await sendMessage({
      type: "send_turn",
      session_id: sessionId,
      input,
      permission_mode: "accept_edits",
    });
  }

  async function handleInterrupt() {
    await sendMessage({ type: "interrupt_turn", session_id: sessionId });
  }

  async function handlePermissionDecision(decision: PermissionDecision) {
    if (!pendingPermission) return;
    await sendMessage({
      type: "answer_permission",
      session_id: sessionId,
      request_id: pendingPermission.requestId,
      decision,
    });
    setPendingPermission(null);
  }

  const isRunning = session?.status === "running";
  const title = session?.title || "New thread";

  return (
    <div className="flex h-full min-h-svh flex-col">
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-border px-2 text-sm">
        <SidebarTrigger />
        <span className="truncate font-medium">{title}</span>
        {isRunning && (
          <span className="ml-auto text-xs text-muted-foreground">
            Running...
          </span>
        )}
      </header>

      <MessageList
        turns={turns}
        streaming={streaming ?? null}
        loading={loading}
      />

      <ChatInput
        onSend={handleSend}
        onInterrupt={handleInterrupt}
        isRunning={isRunning}
        disabled={loading}
      />

      {pendingPermission && (
        <PermissionDialog
          toolName={pendingPermission.toolName}
          input={pendingPermission.input}
          suggested={pendingPermission.suggested}
          onDecision={handlePermissionDecision}
        />
      )}
    </div>
  );
}
