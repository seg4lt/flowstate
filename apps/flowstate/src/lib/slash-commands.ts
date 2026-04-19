import type {
  ClientMessage,
  CommandCatalog,
  CommandKind,
  ProviderKind,
  ServerMessage,
  SessionSummary,
  SkillSource,
} from "@/lib/types";
import { PROVIDER_META } from "@/lib/providers";

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

// ---------------------------------------------------------------------------
// SlashCommandItem — the prop shape passed to ChatInput
// ---------------------------------------------------------------------------

/**
 * One row in the slash-command popup. Unions three sources:
 * - app-core commands (no `kind`, matched by name against COMMANDS)
 * - provider-native commands (kind: "builtin")
 * - user-authored SKILL.md entries (kind: "user_skill", with source)
 * - sub-agents (kind: "agent" — synthesized from CommandCatalog.agents)
 *
 * `userInvocable: false` entries are filtered out by
 * `mergeCommandsWithCatalog` before reaching the popup.
 */
export interface SlashCommandItem {
  name: string;
  description: string;
  /**
   * Undefined for core (app-level) commands. For provider commands
   * carries the wire-level discriminator; for agents it's the sentinel
   * string "agent" which is NOT a `CommandKind` variant — we use the
   * same field so the popup has a single dispatch point for badges.
   */
  kind?: CommandKind["kind"] | "agent";
  /** Only meaningful when kind is "user_skill". */
  source?: SkillSource;
  /** Provider-suggested argument placeholder, rendered muted inline. */
  argHint?: string;
  /**
   * Wire id for provider commands. Used by the reducer's id-equality
   * short-circuit and by React keys. Core commands don't have one.
   */
  id?: string;
}

/** Metadata subset safe to pass as a prop (no execute function). Core
 * commands have no `kind` — the popup renders them without a badge. */
export const COMMAND_META: SlashCommandItem[] = COMMANDS.map(
  ({ name, description }) => ({ name, description }),
);

// ---------------------------------------------------------------------------
// Catalog merge + invocation formatting
// ---------------------------------------------------------------------------

/**
 * Merge the core app commands with a provider catalog into a single
 * list suitable for the popup. Core commands come first and win name
 * collisions against provider commands. Non-user-invocable provider
 * commands (e.g. Copilot's `customize-cloud-agent`) are filtered out.
 * Sub-agents from `catalog.agents` are appended as synthetic entries
 * with `kind: "agent"`.
 */
export function mergeCommandsWithCatalog(
  catalog: CommandCatalog | undefined,
): SlashCommandItem[] {
  const core = COMMAND_META;
  if (!catalog) return core;

  const seen = new Set(core.map((c) => c.name));
  const out: SlashCommandItem[] = [...core];

  for (const cmd of catalog.commands) {
    if (!cmd.userInvocable) continue;
    if (seen.has(cmd.name)) continue;
    seen.add(cmd.name);
    out.push({
      id: cmd.id,
      name: cmd.name,
      description: cmd.description,
      kind: cmd.kind,
      source: cmd.kind === "user_skill" ? cmd.source : undefined,
      argHint: cmd.argHint,
    });
  }

  // Collect agents first, then push them at the end so they cluster
  // visually. Sort alphabetically so the ordering is stable across
  // refreshes — the reducer's id-equality short-circuit depends on a
  // deterministic list.
  const agents: SlashCommandItem[] = [];
  for (const agent of catalog.agents) {
    // Namespace agent names away from slash-commands so a collision
    // (e.g. a skill named "general-purpose") doesn't silently replace
    // the agent. The popup already distinguishes kinds visually.
    const key = `agent:${agent.name}`;
    if (seen.has(key)) continue;
    seen.add(key);
    agents.push({
      id: agent.id,
      name: agent.name,
      description: agent.description,
      kind: "agent",
    });
  }
  agents.sort((a, b) => a.name.localeCompare(b.name));

  return [...out, ...agents];
}

/** True when `name` matches a core app command. Used by chat-input
 * to decide whether to fire the command immediately on selection or
 * pre-fill the composer (for user skills / provider commands that can
 * take args). */
export function isCoreCommand(name: string): boolean {
  return COMMANDS.some((c) => c.name === name);
}

/** Format the string that should land in the composer when the user
 * selects a non-core command. Codex uses `$name` for its skill-like
 * invocations; every other provider uses `/name`. Driven by
 * `PROVIDER_META[provider].slashPrefix`. */
export function formatSkillInvocation(
  name: string,
  provider: ProviderKind | undefined,
): string {
  const prefix = provider ? PROVIDER_META[provider].slashPrefix : "/";
  return `${prefix}${name}`;
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

export type ResolveResult =
  | { kind: "core"; command: SlashCommand; args: string }
  | {
      kind: "skill";
      item: SlashCommandItem;
      invocation: string;
      args: string;
    }
  | { kind: "unknown"; raw: string };

/**
 * Classify `input` against the merged command list. Returns `null`
 * when the input isn't a slash command at all. When `commands` is
 * omitted the function falls back to core-only matching — handy for
 * legacy call sites that haven't been migrated yet.
 */
export function resolveCommand(
  input: string,
  commands?: SlashCommandItem[],
  provider?: ProviderKind,
): ResolveResult | null {
  const trimmed = input.trim();
  if (!trimmed.startsWith("/") && !trimmed.startsWith("$")) return null;

  const prefix = trimmed[0];
  const spaceIdx = trimmed.indexOf(" ");
  const rawName =
    spaceIdx === -1 ? trimmed.slice(1) : trimmed.slice(1, spaceIdx);
  const name = rawName.toLowerCase();
  const args = spaceIdx === -1 ? "" : trimmed.slice(spaceIdx + 1).trim();

  // Core commands always use `/` and are matched by lowercase name.
  if (prefix === "/") {
    const core = COMMANDS.find((c) => c.name === name);
    if (core) return { kind: "core", command: core, args };
  }

  if (commands && commands.length > 0) {
    const item = commands.find(
      (c) => c.name.toLowerCase() === name && !isCoreCommand(c.name),
    );
    if (item) {
      return {
        kind: "skill",
        item,
        invocation: formatSkillInvocation(item.name, provider),
        args,
      };
    }
  }

  return { kind: "unknown", raw: trimmed.split(" ")[0] };
}

/**
 * Return commands whose `/name` starts with the given partial input.
 * Used by the autocomplete popup in ChatInput. When `commands` is
 * provided it's used as the source of truth; otherwise we fall back
 * to the hardcoded core commands.
 */
export function getCompletions(
  partial: string,
  commands?: SlashCommandItem[],
): SlashCommandItem[] {
  const source = commands ?? COMMAND_META;
  const lower = partial.toLowerCase();
  // The popup is triggered by either `/` (slash commands + skills) or
  // `$` (Codex skill invocations). We match against both prefix forms
  // so Codex sessions see their `$skill` entries when the user starts
  // typing `$`.
  return source.filter((c) => {
    const slashMatch = `/${c.name.toLowerCase()}`.startsWith(lower);
    const dollarMatch = `$${c.name.toLowerCase()}`.startsWith(lower);
    return slashMatch || dollarMatch;
  });
}
