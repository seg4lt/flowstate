#!/usr/bin/env node
/**
 * GitHub Copilot SDK Bridge for ZenUI
 *
 * This bridge uses the official @github/copilot-sdk to communicate
 * with the GitHub Copilot CLI, forwarding streaming events as JSON lines.
 */

import {
  CopilotClient,
  type PermissionRequest,
  type PermissionRequestResult,
} from '@github/copilot-sdk';
import { createInterface } from 'readline';
import { randomUUID } from 'crypto';
import { existsSync } from 'fs';
import { delimiter as pathDelimiter, join as joinPath } from 'path';
import { homedir } from 'os';

/**
 * Cross-platform PATH resolution for the `copilot` CLI binary.
 * Pure Node, no shell, no extra deps — works on Linux, macOS, Windows.
 *
 * Returns an absolute path to the resolved binary, or null if no
 * matching file exists on PATH or in any of the known fallback
 * install locations.
 */
function resolveCopilotBinary(): string | null {
  const isWindows = process.platform === 'win32';
  const exeExtensions = isWindows
    ? (process.env.PATHEXT ?? '.COM;.EXE;.BAT;.CMD').split(';').map((e) => e.toLowerCase())
    : [''];

  const pathEntries = (process.env.PATH ?? '').split(pathDelimiter).filter(Boolean);
  for (const dir of pathEntries) {
    for (const ext of exeExtensions) {
      const candidate = joinPath(dir, `copilot${ext}`);
      try {
        if (existsSync(candidate)) return candidate;
      } catch {
        // ENOENT/EACCES on individual entries shouldn't abort the walk.
      }
    }
  }

  // Fallback: well-known install locations across the three OSes.
  // Tried only when PATH lookup fails (e.g. host process didn't
  // forward PATH, or the binary lives in a directory the user hasn't
  // added to PATH).
  const home = homedir();
  const fallbackPaths: string[] = isWindows
    ? [
        joinPath(home, 'AppData', 'Local', 'Programs', 'copilot', 'copilot.exe'),
        joinPath(home, 'AppData', 'Roaming', 'npm', 'copilot.cmd'),
        'C:\\Program Files\\GitHub CLI\\copilot.exe',
      ]
    : [
        joinPath(home, '.local', 'bin', 'copilot'),
        '/opt/homebrew/bin/copilot',
        '/usr/local/bin/copilot',
        '/home/linuxbrew/.linuxbrew/bin/copilot',
        '/usr/bin/copilot',
      ];

  for (const candidate of fallbackPaths) {
    try {
      if (existsSync(candidate)) return candidate;
    } catch {
      // skip
    }
  }

  return null;
}

// ZenUI protocol types
interface ZenUiMessage {
  type: string;
  [key: string]: unknown;
}

type ZenUiPermissionMode = 'default' | 'accept_edits' | 'plan' | 'bypass';

// `UserInputRequest` / `UserInputResponse` are defined in @github/copilot-sdk's
// types.d.ts but the package's index.d.ts does not re-export them, so we
// mirror the structural shape locally. Keep in sync with
// node_modules/@github/copilot-sdk/dist/types.d.ts:550-577.
interface UserInputRequest {
  question: string;
  choices?: string[];
  allowFreeform?: boolean;
}
interface UserInputResponse {
  answer: string;
  wasFreeform: boolean;
}

// Pending round-trip resolvers keyed by request_id. Populated when the SDK
// asks the host (via permission/user-input handler), drained when an
// `answer_*` message arrives on stdin.
const pendingUserInputs = new Map<string, (resp: UserInputResponse) => void>();
const pendingPermissions = new Map<
  string,
  (result: PermissionRequestResult) => void
>();

/** Write a stream event JSON line to stdout. */
function writeStream(payload: Record<string, unknown>): void {
  process.stdout.write(JSON.stringify({ type: 'stream', ...payload }) + '\n');
}

/** Best-effort markdown bullet/numbered-list parser for plan content. */
function parsePlanSteps(raw: string): Array<{ title: string; detail?: string }> {
  if (!raw) return [];
  const steps: Array<{ title: string; detail?: string }> = [];
  for (const line of raw.split('\n')) {
    const trimmed = line.trim();
    const match = trimmed.match(/^(?:[-*]|\d+\.)\s+(.*)$/);
    if (match) {
      steps.push({ title: match[1] });
    }
  }
  return steps;
}

class CopilotBridge {
  private client: CopilotClient | null = null;
  private session: any = null;
  // Closures registered at session creation read this field on every callback,
  // so a per-turn `permission_mode` from sendPrompt can change handler behavior
  // without re-registering anything on the SDK.
  private currentPermissionMode: ZenUiPermissionMode = 'accept_edits';

  /**
   * Permission decision policy. Returns either an immediate
   * PermissionRequestResult, or `null` to indicate "forward this request to
   * ZenUI and wait for the user to decide via answer_permission".
   */
  private decidePermissionLocally(
    req: PermissionRequest,
  ): PermissionRequestResult | null {
    const mode = this.currentPermissionMode;
    if (mode === 'bypass') {
      return { kind: 'approved' };
    }
    if (mode === 'accept_edits' || mode === 'plan') {
      // Auto-approve read/write file ops; route shell/mcp/url/custom-tool
      // through the user.
      if (req.kind === 'read' || req.kind === 'write') {
        return { kind: 'approved' };
      }
      return null;
    }
    // 'default' — ask the user about everything.
    return null;
  }

  async start(): Promise<void> {
    console.error('[bridge] Starting GitHub Copilot SDK Bridge...');

    // Resolve the absolute path to the `copilot` CLI binary.
    //
    // The upstream @github/copilot-sdk validates `cliPath` with
    // `fs.existsSync(cliPath)`, which is a CWD-relative file check —
    // a bare name like "copilot" never resolves through PATH. So we
    // MUST hand the SDK an absolute path.
    //
    // We do the PATH walk in pure Node so this works identically on
    // Linux, macOS, and Windows with no shell, no `which`/`where`
    // subprocess, and no extra npm dep:
    //   - `process.env.PATH` split by `path.delimiter` (':' on POSIX,
    //     ';' on Windows)
    //   - for each PATH entry, try `<entry>/<name><ext>` for every
    //     ext in PATHEXT on Windows (.EXE/.CMD/...) or just '' on POSIX
    //   - first hit that `existsSync` is the resolved binary
    //
    // If that fails (PATH not inherited, copilot installed somewhere
    // unusual), fall back to a short list of well-known install
    // locations across the three OSes.
    const copilotPath = resolveCopilotBinary();

    if (!copilotPath) {
      throw new Error(
        'Copilot CLI not found. Install `@github/copilot` (e.g. via the official ' +
          'GitHub Copilot CLI installer) and ensure the `copilot` binary is on PATH ' +
          'when launching this process.',
      );
    }

    console.error(`[bridge] Resolved copilot CLI at: ${copilotPath}`);

    // Create client with system CLI
    this.client = new CopilotClient({
      useStdio: true,
      cliPath: copilotPath,
      logLevel: 'info',
    });

    console.error('[bridge] Connecting to Copilot CLI...');
    await this.client.start();
    console.error('[bridge] Connected to Copilot CLI');
  }

  async createSession(
    cwd: string,
    model?: string,
    resumeSessionId?: string,
  ): Promise<string> {
    if (!this.client) {
      throw new Error('Client not started');
    }

    const selectedModel = model ?? 'gpt-4o';

    const permissionHandler = async (
      req: PermissionRequest,
      _invocation: { sessionId: string },
    ): Promise<PermissionRequestResult> => {
      const local = this.decidePermissionLocally(req);
      if (local !== null) {
        return local;
      }
      // Forward to ZenUI and wait for the user to answer.
      const requestId = randomUUID();
      writeStream({
        event: 'permission_request',
        request_id: requestId,
        tool_name: req.kind,
        input: req,
        suggested: 'allow',
      });
      return await new Promise<PermissionRequestResult>((resolve) => {
        pendingPermissions.set(requestId, resolve);
      });
    };

    const userInputHandler = async (
      req: UserInputRequest,
      _invocation: { sessionId: string },
    ): Promise<UserInputResponse> => {
      const requestId = randomUUID();
      writeStream({
        event: 'user_question',
        request_id: requestId,
        question: req.question,
        choices: req.choices ?? null,
        allow_freeform: req.allowFreeform ?? true,
      });
      return await new Promise<UserInputResponse>((resolve) => {
        pendingUserInputs.set(requestId, resolve);
      });
    };

    // `streaming: true` is REQUIRED for the SDK to emit incremental
    // `assistant.message_delta` / `assistant.reasoning_delta` events.
    // Without it, the SDK only fires the final `assistant.message` /
    // `assistant.reasoning` events and the UI sees only the complete
    // response in one shot. See
    // https://github.com/github/copilot-sdk README.
    //
    // `workingDirectory` MUST be set explicitly. Without it, the SDK
    // defaults tool operations (bash, file reads, edits, …) to the
    // bridge's own cwd — i.e. the rust-embed extraction dir under
    // `~/Library/Caches/zenui/copilot-bridge-<hash>/`, which is not a
    // project at all. The Rust adapter passes the zenui session cwd via
    // the `cwd` parameter on this call; forward it here.
    const baseConfig = {
      model: selectedModel,
      streaming: true,
      workingDirectory: cwd,
      onPermissionRequest: permissionHandler,
      onUserInputRequest: userInputHandler,
    };

    // Resume path: if the Rust adapter handed us a previously-persisted
    // native_thread_id, try to rehydrate that Copilot-server-side
    // session so the model sees the full prior conversation. If the
    // upstream doesn't recognise the id (expired / deleted / stale
    // after a server-side purge) we log a warning and fall through to
    // a fresh createSession. The Rust side captures whichever
    // sessionId we ultimately return and overwrites
    // provider_state.native_thread_id on the next turn_completed, so a
    // stale id self-heals after one round-trip.
    if (resumeSessionId) {
      console.error(
        `[bridge] Resuming session ${resumeSessionId} in: ${cwd} (model: ${selectedModel})`,
      );
      try {
        this.session = await this.client.resumeSession(
          resumeSessionId,
          baseConfig as Parameters<typeof this.client.resumeSession>[1],
        );
      } catch (err) {
        console.error(
          `[bridge] Resume failed for ${resumeSessionId}, falling back to fresh session: ${
            err instanceof Error ? err.message : String(err)
          }`,
        );
        this.session = undefined;
      }
    }

    if (!this.session) {
      console.error(
        `[bridge] Creating session in: ${cwd} (model: ${selectedModel})`,
      );
      this.session = await this.client.createSession(
        baseConfig as Parameters<typeof this.client.createSession>[0],
      );
    }

    // Plan-mode visibility: when the model decides to exit plan mode, surface
    // the proposed plan to ZenUI as a read-only plan card. NOTE: the SDK
    // documents `session.respondToExitPlanMode()` in the event docstring but
    // no such method is exposed in the public API, so this is observe-only —
    // we cannot currently route an accept/reject decision back to the model.
    this.session.on('exit_plan_mode.requested', (event: any) => {
      const data = event?.data ?? {};
      const raw: string = data.planContent ?? data.summary ?? '';
      writeStream({
        event: 'plan_proposed',
        plan_id: data.requestId ?? randomUUID(),
        title: 'Copilot plan',
        steps: parsePlanSteps(raw),
        raw,
      });
    });

    // Placeholder: when the Copilot SDK exposes an enter_plan_mode
    // event, wire it here so the frontend can sync the mode selector.
    // this.session.on('enter_plan_mode.requested', (event: any) => {
    //   writeStream({
    //     event: 'plan_mode_entered',
    //     call_id: event?.data?.requestId ?? randomUUID(),
    //   });
    // });

    // CopilotSession exposes a non-optional `sessionId: string` per the
    // SDK's type definition. If the SDK ever violates that contract we
    // want a loud error, not a silent fallback to a dead label string.
    const sessionId = this.session.sessionId;
    if (!sessionId) {
      throw new Error(
        'Copilot SDK returned a session without a sessionId — upstream SDK contract broken',
      );
    }
    console.error(`[bridge] Session ready: ${sessionId}`);

    return sessionId;
  }

  async sendPrompt(
    prompt: string,
    permissionMode: ZenUiPermissionMode,
    reasoningEffort?: 'minimal' | 'low' | 'medium' | 'high',
  ): Promise<string> {
    if (!this.session) {
      throw new Error('No active session');
    }

    // Stash the per-turn mode so the closures registered at session creation
    // (permissionHandler / userInputHandler) read the right policy on this turn.
    this.currentPermissionMode = permissionMode;
    console.error(
      `[bridge] Sending prompt (${prompt.length} chars, mode=${permissionMode}, effort=${reasoningEffort ?? 'unset'})`,
    );

    // Copilot tracks session-level collaboration mode via `session.rpc.mode.set`.
    // Values: "interactive" (normal tool execution) or "plan" (plan-only, model
    // calls exit_plan_mode when done). ZenUI's Plan mode must set "plan"; all
    // other ZenUI modes map to "interactive" so the model actually executes tools.
    // See https://github.com/github/copilot-sdk/blob/main/nodejs/test/e2e/rpc.test.ts
    try {
      const targetMode = permissionMode === 'plan' ? 'plan' : 'interactive';
      await this.session.rpc.mode.set({ mode: targetMode });
      console.error(`[bridge] Session mode set to ${targetMode}`);
    } catch (err) {
      console.error(
        `[bridge] Failed to set session mode: ${err instanceof Error ? err.message : String(err)}`,
      );
    }

    // Subscribe to streaming events. Each returns an unsubscribe fn.
    const unsubs: Array<() => void> = [];
    let deltasSeen = 0;
    // Context-window usage buffered from `session.usage_info` so that
    // when `assistant.usage` fires we can compose a full turn_usage
    // event in one shot. The Copilot SDK reports these in two
    // separate events, so we hold the latest context snapshot here.
    let latestTokenLimit: number | null = null;
    let latestCurrentTokens: number | null = null;

    // Copilot quota ids come from the SDK's `quotaSnapshots` map —
    // known ids include "chat", "completions", "premium_interactions".
    // Pretty-print the ones we know about, title-case the rest as a
    // safe default so arbitrary future ids still read reasonably.
    const copilotQuotaLabel = (id: string): string => {
      const known: Record<string, string> = {
        chat: 'Chat',
        completions: 'Completions',
        premium_interactions: 'Premium interactions',
      };
      if (known[id]) return known[id];
      return id
        .split('_')
        .map((w) => (w.length > 0 ? w[0].toUpperCase() + w.slice(1) : w))
        .join(' ');
    };

    // Text deltas
    unsubs.push(
      this.session.on('assistant.message_delta', (event: any) => {
        const delta: string = event?.data?.deltaContent ?? '';
        if (delta) {
          deltasSeen++;
          writeStream({ event: 'text_delta', delta });
        }
      }),
    );

    // Fallback: some models (e.g. gpt-4o) emit only the final assistant.message
    // without per-token deltas. If no deltas fired, emit the full content as one delta.
    unsubs.push(
      this.session.on('assistant.message', (event: any) => {
        if (deltasSeen === 0) {
          const content: string = event?.data?.content ?? '';
          if (content) {
            writeStream({ event: 'text_delta', delta: content });
          }
        }
        deltasSeen = 0;
      }),
    );

    // Reasoning deltas
    unsubs.push(
      this.session.on('assistant.reasoning_delta', (event: any) => {
        const delta: string = event?.data?.deltaContent ?? '';
        if (delta) {
          writeStream({ event: 'reasoning_delta', delta });
        }
      }),
    );

    // Tool execution start
    unsubs.push(
      this.session.on('tool.execution_start', (event: any) => {
        const d = event?.data ?? {};
        writeStream({
          event: 'tool_started',
          call_id: d.toolCallId ?? '',
          name: d.toolName ?? '',
          args: d.arguments ?? {},
        });
      }),
    );

    // Tool execution complete
    unsubs.push(
      this.session.on('tool.execution_complete', (event: any) => {
        const d = event?.data ?? {};
        const success: boolean = d.success ?? true;
        const output: string =
          d.result?.detailedContent ?? d.result?.content ?? '';
        // error info: look for nested error object
        const errMsg: string | undefined = !success
          ? (d.error?.message ?? d.error?.text ?? 'Tool failed')
          : undefined;
        writeStream({
          event: 'tool_completed',
          call_id: d.toolCallId ?? '',
          output,
          ...(errMsg !== undefined ? { error: errMsg } : {}),
        });
      }),
    );

    // Session error
    unsubs.push(
      this.session.on('session.error', (event: any) => {
        const msg: string = event?.data?.message ?? 'Unknown Copilot error';
        console.error(`[bridge] Session error: ${msg}`);
        writeStream({ event: 'info', message: `Copilot error: ${msg}` });
      }),
    );

    // Context window snapshot. Buffered; flushed when assistant.usage
    // fires with concrete per-turn tokens.
    unsubs.push(
      this.session.on('session.usage_info', (event: any) => {
        const d = event?.data ?? {};
        if (typeof d.tokenLimit === 'number') latestTokenLimit = d.tokenLimit;
        if (typeof d.currentTokens === 'number')
          latestCurrentTokens = d.currentTokens;
      }),
    );

    // Per-API-call token usage + rate-limit snapshots. Composes a
    // turn_usage event combining the per-call tokens with the
    // latest buffered context-window info, and fans out one
    // rate_limit_update per quotaSnapshot entry.
    unsubs.push(
      this.session.on('assistant.usage', (event: any) => {
        const d = event?.data ?? {};
        if (d.inputTokens != null || d.outputTokens != null) {
          // Prefer `currentTokens` from session.usage_info as the
          // authoritative "how full is the window" reading —
          // Copilot's SDK computes that for us, so we surface it
          // via output_tokens being the delta we just added. Keep
          // the contextWindow denominator from tokenLimit so the
          // Flowstate UI renders N / M correctly.
          writeStream({
            event: 'turn_usage',
            usage: {
              inputTokens: latestCurrentTokens ?? d.inputTokens ?? 0,
              outputTokens: d.outputTokens ?? 0,
              cacheReadTokens: d.cacheReadTokens ?? null,
              cacheWriteTokens: d.cacheWriteTokens ?? null,
              contextWindow: latestTokenLimit ?? null,
              totalCostUsd: d.cost ?? null,
              durationMs: d.duration ?? null,
              model: d.model ?? null,
            },
          });
        }

        const snapshots = d.quotaSnapshots as
          | Record<
              string,
              {
                isUnlimitedEntitlement?: boolean;
                entitlementRequests?: number;
                usedRequests?: number;
                remainingPercentage?: number;
                resetDate?: string;
                overage?: number;
                usageAllowedWithExhaustedQuota?: boolean;
              }
            >
          | undefined;
        if (snapshots) {
          for (const [bucketId, q] of Object.entries(snapshots)) {
            if (q.isUnlimitedEntitlement) continue;
            const remaining = q.remainingPercentage ?? 1;
            const utilization = Math.max(0, Math.min(1, 1 - remaining));
            const isUsingOverage = (q.overage ?? 0) > 0;
            const exhausted = utilization >= 1;
            const status = exhausted
              ? q.usageAllowedWithExhaustedQuota
                ? 'allowed_warning'
                : 'rejected'
              : utilization >= 0.8
                ? 'allowed_warning'
                : 'allowed';
            const resetsAt = q.resetDate
              ? Date.parse(q.resetDate)
              : null;
            writeStream({
              event: 'rate_limit_update',
              rate_limit_info: {
                bucket: bucketId,
                label: copilotQuotaLabel(bucketId),
                status,
                utilization,
                resetsAt: Number.isFinite(resetsAt) ? resetsAt : null,
                isUsingOverage,
              },
            });
          }
        }
      }),
    );

    try {
      // sendAndWait blocks until session.idle, streaming events fire via the handlers above.
      // The Copilot SDK does not have a documented `reasoning_effort` field; we forward it
      // alongside the prompt so the SDK can pick it up if a future version supports it, and
      // ignore it silently otherwise.
      const sendPayload: Record<string, unknown> = { prompt };
      if (reasoningEffort !== undefined) {
        sendPayload.reasoning_effort = reasoningEffort;
      }
      const response = await this.session.sendAndWait(sendPayload as { prompt: string }, 120_000);
      const content: string =
        response?.data?.content ?? '[No response from Copilot]';
      console.error('[bridge] Turn complete');
      return content;
    } finally {
      // Always unsubscribe so handlers from this turn don't leak into the next.
      unsubs.forEach((fn) => fn());
    }
  }

  async interrupt(): Promise<void> {
    if (!this.session) return;
    console.error('[bridge] Interrupting session...');
    try {
      await this.session.interrupt();
    } catch {
      // interrupt may throw if not in-flight; ignore
    }
  }

  async listModels(): Promise<
    Array<{
      value: string;
      label: string;
      contextWindow?: number;
      maxOutputTokens?: number;
    }>
  > {
    if (!this.client) {
      throw new Error('Client not started');
    }
    const models = await this.client.listModels();
    return models.map((m: any) => {
      // The Copilot SDK exposes the model's ceilings on
      // `capabilities.limits`. Older SDK builds expose them flat on
      // the entry (see node_modules/@github/copilot/sdk/index.d.ts
      // around the OpenRouter provider shape). Accept both so a
      // Copilot CLI upgrade doesn't silently drop the value.
      const limits = (m.capabilities?.limits ?? {}) as Record<string, unknown>;
      const ctx =
        (typeof m.maxContextWindowTokens === 'number'
          ? m.maxContextWindowTokens
          : undefined) ??
        (typeof limits.max_context_window_tokens === 'number'
          ? (limits.max_context_window_tokens as number)
          : undefined);
      const out =
        (typeof m.maxOutputTokens === 'number'
          ? m.maxOutputTokens
          : undefined) ??
        (typeof limits.max_output_tokens === 'number'
          ? (limits.max_output_tokens as number)
          : undefined);
      const entry: {
        value: string;
        label: string;
        contextWindow?: number;
        maxOutputTokens?: number;
      } = {
        value: (m.id ?? m.value ?? '') as string,
        label: (m.name ?? m.displayName ?? m.id ?? '') as string,
      };
      if (typeof ctx === 'number') entry.contextWindow = ctx;
      if (typeof out === 'number') entry.maxOutputTokens = out;
      return entry;
    });
  }

  /**
   * Enumerate the Copilot session's skills, sub-agents, and MCP
   * servers. Unlike listModels (client-scoped), these live on the
   * session object — so this RPC requires the bridge to already have
   * a session created via `create_session`. Returns the raw SDK
   * shapes; the Rust side maps `userInvocable`, etc. onto our wire
   * types.
   */
  async listCapabilities(): Promise<{
    skills: Array<{
      name: string;
      description: string;
      source: string;
      userInvocable: boolean;
      enabled: boolean;
    }>;
    agents: Array<{
      name: string;
      displayName: string;
      description: string;
    }>;
    mcpServers: Array<{
      name: string;
      status: string;
      source?: string;
      error?: string;
    }>;
  }> {
    if (!this.session) {
      throw new Error('Session not created');
    }
    const [skillsResult, agentResult, mcpResult] = await Promise.all([
      this.session.rpc.skills.list(),
      this.session.rpc.agent.list(),
      this.session.rpc.mcp.list(),
    ]);
    return {
      skills: (skillsResult.skills ?? []).map((s: any) => ({
        name: s.name,
        description: s.description ?? '',
        source: s.source ?? '',
        userInvocable: s.userInvocable === true,
        enabled: s.enabled !== false,
      })),
      agents: (agentResult.agents ?? []).map((a: any) => ({
        name: a.name,
        displayName: a.displayName ?? a.name,
        description: a.description ?? '',
      })),
      mcpServers: (mcpResult.servers ?? []).map((m: any) => ({
        name: m.name,
        status: String(m.status ?? 'unknown'),
        source: m.source,
        error: m.error,
      })),
    };
  }

  async stop(): Promise<void> {
    if (this.session) {
      try {
        await this.session.disconnect();
      } catch {
        // ignore
      }
      this.session = null;
    }
    if (this.client) {
      try {
        await this.client.stop();
      } catch {
        // ignore
      }
      this.client = null;
    }
  }
}

// Main entry point
async function main(): Promise<void> {
  const bridge = new CopilotBridge();

  const rl = createInterface({
    input: process.stdin,
    output: process.stdout,
    terminal: false,
  });

  // Track the in-flight send_prompt as a background promise so the readline
  // loop can keep processing stdin (specifically `interrupt` and `answer_*`
  // messages) while a turn is running. Without this, awaiting `sendPrompt`
  // inline blocks the loop and the interrupt message never gets read until
  // after the turn completes — which defeats the whole point.
  let promptInFlight: Promise<void> | null = null;

  try {
    await bridge.start();

    // Send ready signal
    process.stdout.write(JSON.stringify({ type: 'ready' }) + '\n');
    console.error('[bridge] Ready for commands');

    // Process incoming messages
    for await (const line of rl) {
      try {
        const msg = JSON.parse(line) as ZenUiMessage;
        console.error(`[bridge] Received: ${msg.type}`);

        switch (msg.type) {
          case 'create_session': {
            const cwd = (msg.cwd as string) ?? process.cwd();
            const model = msg.model as string | undefined;
            const resumeSessionId = msg.resume_session_id as string | undefined;
            const sessionId = await bridge.createSession(cwd, model, resumeSessionId);
            process.stdout.write(
              JSON.stringify({ type: 'session_created', session_id: sessionId }) + '\n',
            );
            break;
          }

          case 'send_prompt': {
            if (promptInFlight) {
              process.stdout.write(
                JSON.stringify({
                  type: 'error',
                  error: 'Another turn is already in flight',
                }) + '\n',
              );
              break;
            }
            const prompt = msg.prompt as string;
            const permissionMode =
              ((msg.permission_mode as ZenUiPermissionMode) ?? 'accept_edits');
            const effort = msg.reasoning_effort as
              | 'minimal'
              | 'low'
              | 'medium'
              | 'high'
              | undefined;
            promptInFlight = (async () => {
              try {
                const output = await bridge.sendPrompt(prompt, permissionMode, effort);
                process.stdout.write(
                  JSON.stringify({ type: 'response', output }) + '\n',
                );
              } catch (err) {
                process.stdout.write(
                  JSON.stringify({
                    type: 'error',
                    error: err instanceof Error ? err.message : String(err),
                  }) + '\n',
                );
              } finally {
                promptInFlight = null;
              }
            })();
            break;
          }

          case 'answer_user_input': {
            const reqId = msg.request_id as string;
            const resolver = pendingUserInputs.get(reqId);
            if (resolver) {
              pendingUserInputs.delete(reqId);
              resolver({
                answer: (msg.answer as string) ?? '',
                wasFreeform: (msg.was_freeform as boolean) ?? false,
              });
            }
            break;
          }

          case 'cancel_user_input': {
            // SDK's UserInputResponse has no cancel variant, so feed the model a
            // `[cancelled]` sentinel string and mark it as freeform. This unblocks
            // the ask_user tool call; the model reads the sentinel and typically
            // proceeds without the answer.
            const reqId = msg.request_id as string;
            const resolver = pendingUserInputs.get(reqId);
            if (resolver) {
              pendingUserInputs.delete(reqId);
              resolver({ answer: '[cancelled]', wasFreeform: true });
            }
            break;
          }

          case 'answer_permission': {
            const reqId = msg.request_id as string;
            const decision = msg.decision as string;
            const resolver = pendingPermissions.get(reqId);
            if (resolver) {
              pendingPermissions.delete(reqId);
              const approved =
                decision === 'allow' || decision === 'allow_always';
              resolver(
                approved
                  ? { kind: 'approved' }
                  : { kind: 'denied-interactively-by-user' },
              );
            }
            break;
          }

          case 'interrupt': {
            await bridge.interrupt();
            process.stdout.write(
              JSON.stringify({ type: 'interrupted' }) + '\n',
            );
            break;
          }

          case 'list_models': {
            try {
              const models = await bridge.listModels();
              process.stdout.write(
                JSON.stringify({ type: 'models', models }) + '\n',
              );
            } catch (err) {
              process.stdout.write(
                JSON.stringify({
                  type: 'error',
                  error: `list_models failed: ${err instanceof Error ? err.message : String(err)}`,
                }) + '\n',
              );
            }
            break;
          }

          case 'list_capabilities': {
            try {
              const caps = await bridge.listCapabilities();
              process.stdout.write(
                JSON.stringify({
                  type: 'capabilities',
                  skills: caps.skills,
                  agents: caps.agents,
                  mcp_servers: caps.mcpServers,
                }) + '\n',
              );
            } catch (err) {
              process.stdout.write(
                JSON.stringify({
                  type: 'error',
                  error: `list_capabilities failed: ${err instanceof Error ? err.message : String(err)}`,
                }) + '\n',
              );
            }
            break;
          }

          case 'shutdown': {
            console.error('[bridge] Shutdown requested');
            if (promptInFlight) {
              try {
                await promptInFlight;
              } catch {
                // already surfaced via `type: 'error'` inside the inflight task
              }
            }
            await bridge.stop();
            process.exit(0);
          }

          default:
            console.error(`[bridge] Unknown message type: ${msg.type}`);
            process.stdout.write(
              JSON.stringify({
                type: 'error',
                error: `Unknown type: ${msg.type}`,
              }) + '\n',
            );
        }
      } catch (err) {
        console.error('[bridge] Error processing message:', err);
        process.stdout.write(
          JSON.stringify({
            type: 'error',
            error: err instanceof Error ? err.message : String(err),
          }) + '\n',
        );
      }
    }
  } finally {
    await bridge.stop();
  }
}

main().catch((err) => {
  console.error('[bridge] Fatal error:', err);
  process.exit(1);
});
