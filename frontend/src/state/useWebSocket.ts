import { useCallback, useEffect, useRef } from "react";
import { actions } from "./appStore";
import type { BootstrapPayload, ClientMessage, ServerMessage } from "../types";

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

    async function boot() {
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
          actions.setConnectionStatus("connected");
        });

        socket.addEventListener("close", () => {
          if (disposed) return;
          actions.setConnectionStatus("disconnected");
        });

        socket.addEventListener("error", () => {
          if (disposed) return;
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
            case "session_created":
              actions.setLastAction(`Session created: ${payload.session.title}`);
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
        actions.setConnectionStatus("disconnected");
      }
    }

    boot();

    return () => {
      disposed = true;
      socketRef.current?.close();
      socketRef.current = null;
    };
  }, []);

  return sendClientMessage;
}
