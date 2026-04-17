import type {
  ClientMessage,
  ServerMessage,
  SessionSummary,
} from "@/lib/types";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type NavigateFn = (opts: {
  to: string;
  params?: Record<string, string>;
}) => void;

export interface SlashCommandContext {
  sessionId: string;
  session: SessionSummary;
  send: (msg: ClientMessage) => Promise<ServerMessage | null>;
  navigate: NavigateFn;
  toast: (opts: { description: string; duration?: number }) => void;
}

export interface SlashCommand {
  /** Command name without the leading slash, e.g. "flowstate-clear" */
  name: string;
  /** One-liner shown in the autocomplete popup */
  description: string;
  /** Run the command. `args` is everything after the command name. */
  execute: (ctx: SlashCommandContext, args: string) => Promise<void> | void;
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

const clearCommand: SlashCommand = {
  name: "flowstate-clear",
  description: "Clear session — archives current thread and opens a fresh one",
  async execute(ctx) {
    const { session, sessionId, send, navigate, toast } = ctx;

    // Start the fresh thread BEFORE archiving the current one. If we
    // archive first, the session_archived broadcast races us back into
    // chat-view, whose event handler auto-navigates to "/" when the
    // active session disappears -- and the second await (start_session)
    // can land after that redirect, so the user ends up on the home
    // page instead of the new thread. Starting first means the new
    // chat-view is already mounted by the time the archive event fires,
    // and that event is ignored by the new view because session_id
    // doesn't match.
    const res = await send({
      type: "start_session",
      provider: session.provider,
      model: session.model,
      project_id: session.projectId,
    });

    if (res?.type !== "session_created") {
      toast({
        description: "Failed to start a new session — current thread left as-is",
        duration: 3000,
      });
      return;
    }

    navigate({
      to: "/chat/$sessionId",
      params: { sessionId: res.session.sessionId },
    });

    // Archive the now-old session in the background. The session_archived
    // broadcast will arrive at the new chat-view's event handler but be
    // ignored because event.session_id is the OLD id, not the new one.
    // Errors here are silent on purpose -- the user already has their
    // fresh thread; archive failure shouldn't surface as a scary toast.
    void send({ type: "archive_session", session_id: sessionId });
    toast({ description: "Session cleared" });
  },
};

const newCommand: SlashCommand = {
  name: "flowstate-new",
  description: "New thread — same provider, model & project",
  async execute(ctx) {
    const { session, send, navigate, toast } = ctx;

    const res = await send({
      type: "start_session",
      provider: session.provider,
      model: session.model,
      project_id: session.projectId,
    });

    if (res?.type !== "session_created") {
      toast({
        description: "Failed to create new thread",
        duration: 3000,
      });
      return;
    }

    navigate({
      to: "/chat/$sessionId",
      params: { sessionId: res.session.sessionId },
    });
    toast({ description: "New thread created" });
  },
};

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

export const COMMANDS: SlashCommand[] = [clearCommand, newCommand];

/** Metadata subset safe to pass as a prop (no execute function). */
export const COMMAND_META: { name: string; description: string }[] =
  COMMANDS.map(({ name, description }) => ({ name, description }));

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/**
 * If `input` looks like a slash command (`/foo …`), return the matched
 * command and any trailing arguments. Returns `null` when the input is
 * not a command at all, or `{ command: undefined, raw }` when the slash
 * prefix is present but the name doesn't match any registered command.
 */
export function resolveCommand(
  input: string,
): { command: SlashCommand; args: string } | { command: undefined; raw: string } | null {
  const trimmed = input.trim();
  if (!trimmed.startsWith("/")) return null;

  const spaceIdx = trimmed.indexOf(" ");
  const name =
    spaceIdx === -1
      ? trimmed.slice(1).toLowerCase()
      : trimmed.slice(1, spaceIdx).toLowerCase();
  const args = spaceIdx === -1 ? "" : trimmed.slice(spaceIdx + 1).trim();

  const command = COMMANDS.find((c) => c.name === name);
  if (command) return { command, args };
  return { command: undefined, raw: trimmed.split(" ")[0] };
}

/**
 * Return commands whose `/name` starts with the given partial input.
 * Used by the autocomplete popup in ChatInput.
 */
export function getCompletions(
  partial: string,
): { name: string; description: string }[] {
  const lower = partial.toLowerCase();
  // Match against "/name" so the user can type "/" and see everything,
  // or "/flow" and narrow down to "/flowstate-clear".
  return COMMAND_META.filter((c) => `/${c.name}`.startsWith(lower));
}
