import type { ClientMessage, ServerMessage } from "./types";

let ws: WebSocket | null = null;
const listeners = new Set<(message: ServerMessage) => void>();
const pendingResponses: Array<(message: ServerMessage) => void> = [];

function isEventOrBroadcast(msg: ServerMessage): boolean {
  return msg.type === "event" || msg.type === "welcome" || msg.type === "snapshot";
}

function handleIncoming(raw: MessageEvent) {
  let message: ServerMessage;
  try {
    message = JSON.parse(raw.data);
  } catch {
    return;
  }

  // Broadcast to all listeners (store, chat-view, etc.)
  for (const listener of listeners) {
    listener(message);
  }

  // Resolve pending request-response if this is a response (not an event/broadcast)
  if (!isEventOrBroadcast(message) && pendingResponses.length > 0) {
    const resolve = pendingResponses.shift()!;
    resolve(message);
  }
}

export function initWebSocket(url: string): Promise<void> {
  return new Promise((resolve, reject) => {
    ws = new WebSocket(url);
    ws.onopen = () => resolve();
    ws.onerror = () => reject(new Error("WebSocket connection failed"));
    ws.onmessage = handleIncoming;
    ws.onclose = () => {
      ws = null;
    };
  });
}

export function sendMessage(
  message: ClientMessage,
): Promise<ServerMessage | null> {
  if (!ws || ws.readyState !== WebSocket.OPEN) {
    return Promise.resolve(null);
  }
  return new Promise((resolve) => {
    pendingResponses.push(resolve);
    ws!.send(JSON.stringify(message));
  });
}

export function onServerMessage(
  callback: (message: ServerMessage) => void,
): () => void {
  listeners.add(callback);
  return () => {
    listeners.delete(callback);
  };
}
