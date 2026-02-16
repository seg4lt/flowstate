import { Channel, invoke } from "@tauri-apps/api/core";
import type { ClientMessage, ServerMessage } from "./types";

export function sendMessage(
  message: ClientMessage,
): Promise<ServerMessage | null> {
  return invoke<ServerMessage | null>("handle_message", { message });
}

export function connectStream(
  onMessage: (message: ServerMessage) => void,
): Promise<void> {
  const channel = new Channel<ServerMessage>();
  channel.onmessage = onMessage;
  return invoke("connect", { onEvent: channel });
}
