import { useCallback, useEffect, useRef } from "react";
import { actions, appStore } from "./appStore";
import type { BootstrapPayload, ClientMessage, ServerMessage } from "../types";

// Reconnect policy: capped exponential backoff. The daemon can now
// legitimately outlive the shell window (session-survival feature), so a
// WS disconnect is no longer terminal. On reconnect the daemon sends a
// fresh welcome+bootstrap which replaces client state wholesale.
const RECONNECT_BASE_DELAY_MS = 500;
const RECONNECT_MAX_DELAY_MS = 15_000;
const RECONNECT_MAX_ATTEMPTS = 20;

export function useWebSocket() {
  const socketRef = useRef<WebSocket | null>(null);

  const sendClientMessage = useCallback((message: ClientMessage) => {
    const socket = socketRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      console.error("WebSocket not connected");
      return;
    }
    socket.send(JSON.stringify(message));
  }, []);

  useEffect(() => {
    let disposed = false;
    let reconnectAttempt = 0;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

    function scheduleReconnect() {
      if (disposed) return;
      if (reconnectAttempt >= RECONNECT_MAX_ATTEMPTS) {
        console.error("websocket: giving up after max reconnect attempts");
        actions.setConnectionStatus("disconnected");
        return;
      }
      const delay = Math.min(
        RECONNECT_BASE_DELAY_MS * 2 ** reconnectAttempt,
        RECONNECT_MAX_DELAY_MS,
      );
      reconnectAttempt += 1;
      console.info(
        `websocket: reconnect attempt ${reconnectAttempt} in ${delay}ms`,
      );
      actions.setConnectionStatus("reconnecting");
      reconnectTimer = setTimeout(() => {
        reconnectTimer = null;
        void boot();
      }, delay);
    }

    async function boot() {
      if (disposed) return;
      try {
        const response = await fetch("/api/bootstrap");
        if (!response.ok) throw new Error(`Bootstrap failed: ${response.status}`);

        const bootstrap = (await response.json()) as BootstrapPayload;
        if (disposed) return;
        actions.loadBootstrap(bootstrap);

        const socket = new WebSocket(bootstrap.wsUrl);
        socketRef.current = socket;

        socket.addEventListener("open", () => {
          if (disposed) return;
          reconnectAttempt = 0;
          actions.setConnectionStatus("connected");
          // Re-hydrate the active session on (re)connect. Bootstrap ships
          // sessions without turns, so an open chat view needs its turn
          // history loaded via LoadSession. On first connect this is a
          // no-op (nothing selected yet); on reconnect it restores the
          // chat view the user was looking at before the socket dropped.
          const active = appStore.getState().activeSessionId;
          if (active) {
            socket.send(
              JSON.stringify({ type: "load_session", session_id: active } satisfies ClientMessage),
            );
          }
        });

        socket.addEventListener("close", () => {
          if (disposed) return;
          actions.setConnectionStatus("disconnected");
          socketRef.current = null;
          scheduleReconnect();
        });

        socket.addEventListener("error", (err) => {
          if (disposed) return;
          console.error("websocket error", err);
          actions.setConnectionStatus("disconnected");
        });

        socket.addEventListener("message", (event) => {
          if (disposed) return;
          let payload: ServerMessage;
          try {
            payload = JSON.parse(event.data) as ServerMessage;
          } catch (err) {
            console.error("failed to parse server message", err);
            return;
          }
          switch (payload.type) {
            case "welcome":
              actions.loadBootstrap(payload.bootstrap);
              break;
            case "snapshot":
              actions.setSnapshot(payload.snapshot);
              break;
            case "session_loaded":
              actions.mergeSessionDetail(payload.session);
              break;
            case "session_created":
              actions.setLastAction(`Session created: ${payload.session.sessionId}`);
              // Let the subsequent session_started event add it to the list.
              actions.selectSession(payload.session.sessionId);
              break;
            case "ack":
              // Optional: could surface as last action if useful.
              break;
            case "event":
              actions.applyEvent(payload.event);
              break;
            case "error":
              actions.setLastAction(`Error: ${payload.message}`);
              break;
            case "pong":
              break;
          }
        });
      } catch (err) {
        console.error("bootstrap failed:", err);
        if (disposed) return;
        actions.setConnectionStatus("disconnected");
        scheduleReconnect();
      }
    }

    void boot();

    return () => {
      disposed = true;
      if (reconnectTimer !== null) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      socketRef.current?.close();
      socketRef.current = null;
    };
  }, []);

  return sendClientMessage;
}
