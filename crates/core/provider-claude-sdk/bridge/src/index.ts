#!/usr/bin/env node
/**
 * Claude Agent SDK Bridge for ZenUI
 *
 * Wraps @anthropic-ai/claude-agent-sdk's query() and forwards SDK message
 * stream events to ZenUI as JSON-line stream events.
 */

import {
  query,
  type SDKMessage,
  type SDKUserMessage,
  type Options,
  type PermissionResult,
  type CanUseTool,
  type Query,
  type PermissionMode as SdkPermissionMode,
  type EffortLevel,
} from '@anthropic-ai/claude-agent-sdk';
import { createInterface } from 'readline';
import { randomUUID } from 'crypto';
import { existsSync } from 'fs';
import { delimiter as pathDelimiter, join as joinPath } from 'path';
import { homedir } from 'os';

type ZenUiMessage = {
  type: string;
  [key: string]: unknown;
};

type DecisionString = 'allow' | 'allow_always' | 'deny' | 'deny_always';

// Mirrors `zenui_provider_api::ReasoningEffort`. The first four levels
// are flowstate-native; `xhigh` / `max` are straight pass-throughs to
// the SDK's `EffortLevel` enum (gated per-model via
// `ModelInfo.supportedEffortLevels`).
type ReasoningEffortWire =
  | 'minimal'
  | 'low'
  | 'medium'
  | 'high'
  | 'xhigh'
  | 'max';

interface PendingPermission {
  resolve: (decision: PermissionResult) => void;
  // Echoed back as updatedInput when the user allows. Without this the
  // SDK would replace the tool's args with {} and the tool would crash
  // (e.g. Bash with command=undefined → "Cannot read properties of
  // undefined (reading 'includes')").
  input: Record<string, unknown>;
}

/**
 * Shape of one `AskUserQuestion` question as emitted by the Claude Agent SDK.
 * Mirrors the public contract at
 * https://code.claude.com/docs/en/agent-sdk/user-input#handle-clarifying-questions
 */
interface AskUserSdkQuestion {
  question: string;
  header?: string;
  options: Array<{ label: string; description?: string }>;
  multiSelect?: boolean;
}

interface PendingQuestion {
  resolve: (result: PermissionResult) => void;
  questions: AskUserSdkQuestion[];
}

interface StructuredAnswer {
  questionId: string;
  optionIds: string[];
  answer: string;
}

/**
 * Optional override: try to resolve the user's locally-installed
 * `claude` CLI on PATH and hand it to the Claude Agent SDK via
 * `pathToClaudeCodeExecutable`. The whole thing is opportunistic —
 * if no local `claude` is found we leave the option unset and the
 * SDK transparently falls back to the binary it bundles inside
 * `@anthropic-ai/claude-agent-sdk` (extracted via cli.js / embed.js
 * at runtime), so users who have never installed Claude Code locally
 * still get a working bridge with zero setup.
 *
 * Why prefer the local install when present:
 *   - Picks up the user's existing Claude Code login automatically
 *     (no separate `ANTHROPIC_API_KEY` plumbing required)
 *   - Honors their MCP server configuration, settings, and any
 *     globally-configured tools
 *   - Tracks newer Claude Code releases without re-bundling the SDK
 *
 * Resolution is a pure-Node PATH walk (using `process.env.PATH` and
 * `path.delimiter`, with `PATHEXT` on Windows) plus a curated list
 * of well-known install locations across Linux, macOS, and Windows.
 * No shell, no `which` / `where` subprocess, no extra npm deps.
 *
 * Computed once at module load and cached — the binary location
 * doesn't change between turns within a single bridge process.
 */
/**
 * Cross-platform PATH resolution for a CLI binary.
 *
 * This function's body must stay byte-identical to `resolveBinaryOnPath`
 * in `provider-github-copilot/bridge/src/index.ts`. The two bridges are
 * compiled as independent packages (single-file `dist/index.js` output),
 * so a shared TS module would require bundling; keeping the code
 * identical via convention is the next-best defence against drift.
 */
function resolveBinaryOnPath(
  name: string,
  fallbackPaths: readonly string[],
): string | null {
  const isWindows = process.platform === 'win32';
  const exeExtensions = isWindows
    ? (process.env.PATHEXT ?? '.COM;.EXE;.BAT;.CMD')
        .split(';')
        .filter((e) => e.length > 0)
    : [''];
  // Always include the bare name on every platform, in case the user
  // installed a shim script without an extension on Windows.
  if (!exeExtensions.includes('')) {
    exeExtensions.unshift('');
  }

  const pathEntries = (process.env.PATH ?? '')
    .split(pathDelimiter)
    .filter((entry) => entry.length > 0);

  for (const dir of pathEntries) {
    for (const ext of exeExtensions) {
      const candidate = joinPath(dir, `${name}${ext}`);
      try {
        if (existsSync(candidate)) return candidate;
      } catch {
        // Permission errors on individual entries shouldn't abort the walk.
      }
    }
  }

  for (const candidate of fallbackPaths) {
    try {
      if (existsSync(candidate)) return candidate;
    } catch {
      // skip
    }
  }

  return null;
}

function resolveLocalClaudeBinary(): string | null {
  const home = homedir();
  const fallbackPaths: string[] =
    process.platform === 'win32'
      ? [
          joinPath(home, 'AppData', 'Local', 'Programs', 'claude', 'claude.exe'),
          joinPath(home, 'AppData', 'Roaming', 'npm', 'claude.cmd'),
          'C:\\Program Files\\Claude\\claude.exe',
        ]
      : [
          joinPath(home, '.local', 'bin', 'claude'),
          '/opt/homebrew/bin/claude',
          '/usr/local/bin/claude',
          '/home/linuxbrew/.linuxbrew/bin/claude',
          '/usr/bin/claude',
        ];
  return resolveBinaryOnPath('claude', fallbackPaths);
}

const RESOLVED_LOCAL_CLAUDE_PATH: string | null = resolveLocalClaudeBinary();
if (RESOLVED_LOCAL_CLAUDE_PATH) {
  console.error(
    `[bridge] Using local claude CLI at: ${RESOLVED_LOCAL_CLAUDE_PATH}`,
  );
} else {
  console.error(
    "[bridge] No local claude CLI found; falling back to the SDK's bundled binary",
  );
}

const pendingPermissions = new Map<string, PendingPermission>();
const pendingQuestions = new Map<string, PendingQuestion>();

/// Resolve every in-flight permission and question with a denial /
/// dismissal so the SDK's canUseTool callbacks unwind. Called when the
/// turn aborts — without this, awaiting Promises leak forever.
function drainPendingOnAbort(): void {
  for (const [, p] of pendingPermissions) {
    p.resolve({ behavior: 'deny', message: 'Turn aborted' });
  }
  pendingPermissions.clear();
  for (const [, q] of pendingQuestions) {
    q.resolve({ behavior: 'deny', message: 'Turn aborted' });
  }
  pendingQuestions.clear();
}

function writeJson(payload: Record<string, unknown>): void {
  process.stdout.write(JSON.stringify(payload) + '\n');
}

function writeStream(payload: Record<string, unknown>): void {
  writeJson({ type: 'stream', ...payload });
}

/**
 * Unbounded async iterable that external code can push into at any
 * time. Backbone of streaming-input mode: the SDK's `query()`
 * consumes from this, and `sendPrompt` / `interrupt` push into it
 * across multiple turns on a single persistent Query.
 *
 * Pushed values are delivered in FIFO order. If a consumer is already
 * awaiting `next()` when a value lands, it's handed directly to that
 * waiter with zero buffering; otherwise it's enqueued until the next
 * `next()` call. Calling `close()` drains any outstanding waiters
 * with `{done: true}` and causes all future `next()` calls to do the
 * same, which terminates the SDK Query gracefully.
 *
 * Not thread-safe — single-threaded Node event loop only.
 */
class PushableAsyncIterable<T> implements AsyncIterable<T> {
  private queue: T[] = [];
  private waiters: Array<(result: IteratorResult<T>) => void> = [];
  private closed = false;

  push(value: T): void {
    if (this.closed) return;
    const waiter = this.waiters.shift();
    if (waiter) {
      waiter({ value, done: false });
    } else {
      this.queue.push(value);
    }
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    const waiters = this.waiters.slice();
    this.waiters.length = 0;
    for (const w of waiters) {
      w({ value: undefined as unknown as T, done: true });
    }
  }

  [Symbol.asyncIterator](): AsyncIterator<T> {
    return {
      next: (): Promise<IteratorResult<T>> => {
        if (this.queue.length > 0) {
          return Promise.resolve({ value: this.queue.shift()!, done: false });
        }
        if (this.closed) {
          return Promise.resolve({
            value: undefined as unknown as T,
            done: true,
          });
        }
        return new Promise((resolve) => {
          this.waiters.push(resolve);
        });
      },
      return: (): Promise<IteratorResult<T>> => {
        this.close();
        return Promise.resolve({
          value: undefined as unknown as T,
          done: true,
        });
      },
    };
  }
}

class ClaudeBridge {
  private cwd: string = process.cwd();
  private model?: string;
  /**
   * The SDK session id to `resume:` from when we next need to (re)open
   * a Query — typically only on bridge cold-start / respawn. Persisted
   * back to Rust as `provider_state.native_thread_id`.
   *
   * Updated from every `result` message the SDK commits. In streaming-
   * input mode the Query is persistent across turns, so we no longer
   * need the two-phase pending/committed split that used to protect
   * against interrupted-turn context loss: the Query handles interrupt
   * internally (committing partial tool_use/tool_result pairs), and
   * every `result` we see is a genuine commit boundary.
   */
  private resumeSessionId?: string;
  /**
   * The persistent SDK Query for this session. Lives for the lifetime
   * of the bridge process (one per session). All user messages —
   * first turn, follow-ups, steered messages after an interrupt —
   * flow through `inputQueue` into this single Query, preserving
   * conversation context across interrupts. Created lazily on first
   * `sendPrompt`; torn down in `endSession`.
   */
  private activeQuery?: Query;
  /**
   * Streaming input feeding `activeQuery`. Every `sendPrompt` pushes
   * one `SDKUserMessage` onto this queue; the SDK consumes it as the
   * next user turn. Closing this queue terminates the Query.
   */
  private inputQueue?: PushableAsyncIterable<SDKUserMessage>;
  /**
   * Background pump draining SDK messages out of `activeQuery`. Runs
   * once per session lifetime. Each `result` it observes resolves
   * `pendingTurn`; the pump then keeps looping for the next turn.
   * On pump-level errors (Query unrecoverable) we reject `pendingTurn`
   * and tear down so the next `sendPrompt` re-opens a fresh Query
   * via `resume: resumeSessionId`.
   */
  private pumpPromise?: Promise<void>;
  /**
   * Resolver for the currently-awaited `sendPrompt`. Set before the
   * user message is pushed onto `inputQueue`; cleared by the pump
   * when the matching `result` arrives (or by `interrupt` on the
   * interrupt path). At most one pending turn at a time — Rust
   * serializes turns per session.
   */
  private pendingTurn?: {
    resolve: (text: string) => void;
    reject: (err: unknown) => void;
  };
  /**
   * True from the moment a user message is pushed onto `inputQueue`
   * until the matching `result` is observed (or an interrupt
   * short-circuits it). Guards against concurrent `sendPrompt` calls
   * on the same bridge.
   */
  private turnInProgress = false;
  /**
   * Cached canUseTool closure for the persistent Query. Created at
   * Query-open time and never re-created — mid-turn permission
   * changes flow through `setPermissionMode` + `livePermissionMode`
   * (read inside this closure), not via recreating the callback.
   */
  private canUseToolCached?: CanUseTool;
  /**
   * The `effort` / compact-instructions config the live `activeQuery`
   * was opened with. Compared against each incoming `sendPrompt`
   * request: if either knob changed, we close-and-reopen the Query
   * with the new options, passing `resume: resumeSessionId` so
   * conversation history is fully preserved. The SDK has no
   * mid-session setter for `effort` / `thinking` / `systemPrompt`,
   * so reopen is the only path to honour a per-turn change.
   * Cleared whenever `activeQuery` is unset.
   */
  private queryConfig?: {
    reasoningEffort?: ReasoningEffortWire;
    // Trimmed; undefined and empty-string are equivalent (no steering).
    compactInstructions?: string;
  };
  /**
   * Current effective permission mode for the in-flight turn. Seeded
   * from the `permissionMode` parameter at the start of each
   * `sendPrompt`, then kept in sync with every subsequent mode
   * change — whether the host pushed an `update_permission_mode` RPC
   * (`setPermissionMode` method below) or the user approved an
   * ExitPlanMode with a new mode (`answerPermission` with a mode
   * override, which rides the SDK's `updatedPermissions: [{setMode}]`
   * mechanism).
   *
   * Read by `canUseTool` on every tool invocation to decide whether
   * bypass mode should short-circuit the prompt. The previous
   * implementation read the closure-captured `permissionMode`
   * parameter, which froze at turn start and therefore failed the
   * bypass check for tools AFTER an in-turn mode change — the
   * user's "click Bypass Permissions in the plan-exit dialog but
   * still get prompted for the next Bash" bug.
   */
  private livePermissionMode?: SdkPermissionMode;
  /**
   * Per-turn catalog mapping `subagent_type` (the string Claude's
   * Task tool uses) → raw provider model id, populated on
   * `system.init` from `q.supportedAgents()`. Populated best-effort:
   * if `supportedAgents()` hasn't resolved by the time a Task
   * tool_use arrives, we emit `subagent_started` without a model
   * and rely on `subagent_model_observed` (fired from the
   * subagent's first assistant message) to fill the UI in. Cleared
   * at the start of each turn so a model override doesn't leak
   * between turns.
   */
  private agentModelByType: Map<string, string> = new Map();
  /**
   * Per-turn set of sub-agent ids we've already emitted a
   * `subagent_model_observed` for. The SDK produces many assistant
   * messages per sub-agent (streaming deltas, tool_use blocks,
   * etc.) and they all carry the same `message.model`; we only
   * want to forward it once.
   */
  private observedSubagentIds: Set<string> = new Set();
  /**
   * Running sum of output tokens across every top-level assistant
   * message in the current turn. Each per-call `message.usage`
   * only reports its own slice of output, so we accumulate — unlike
   * cache_read / cache_creation / input_tokens, which are reported
   * per-call and should be displayed as the LATEST call's values
   * to represent current prompt size (not summed, which would
   * inflate past the context window on long tool loops).
   * Reset at the start of every `sendPrompt`.
   */
  private outputTokensTotal = 0;
  /**
   * The last observed per-call `input_tokens` / cache / model state
   * from a top-level assistant message. Forwarded verbatim in the
   * final `turn_usage` emitted from the `result` handler so the
   * closing event carries the same numerator basis as the mid-turn
   * stream (rather than the aggregated `result.usage`, which sums
   * cache reads across every API call and is what caused the
   * "51M / 1M" display).
   */
  private lastAssistantUsage?: {
    inputTokens: number;
    cacheWriteTokens: number | null;
    cacheReadTokens: number | null;
    model?: string;
  };
  /**
   * Per-turn per-agent token accumulator. Keyed by `parent_tool_use_id`
   * for sub-agents, or the literal string `"main"` for the parent
   * (top-level assistant messages). Every incoming assistant message's
   * `usage` is added into the matching bucket at message time — the
   * final `turn_usage` event surfaces this as `usage.agents[]` so the
   * host's usage dashboard can attribute cost and tokens per agent
   * instead of rolling everything into the SDK-aggregate total.
   *
   * Why accumulate per message rather than trust a single SDK-level
   * split: the SDK's `result.modelUsage` is keyed by MODEL, not by
   * agent. Two Explore subagents running the same model would share
   * one `modelUsage` bucket, which loses the attribution the user
   * actually wants ("how much did I spend on Explore runs?"). The
   * per-message `parent_tool_use_id` is the only signal that cleanly
   * separates agents, and it's already on every assistant message.
   *
   * Reset at the start of every `sendPrompt`.
   */
  private agentUsage: Map<
    string,
    {
      agentId: string | null;
      agentType: string | null;
      model: string | null;
      inputTokens: number;
      outputTokens: number;
      cacheReadTokens: number;
      cacheWriteTokens: number;
    }
  > = new Map();
  /**
   * Map from sub-agent id (the SDK's `Task`/`Agent` tool_use call id,
   * which becomes `parent_tool_use_id` on every nested message) to the
   * catalog `subagent_type` the parent spawned it with. Populated when
   * we forward `subagent_started`; read at message time so each
   * `agentUsage` bucket can carry the human-readable type label
   * ("Explore", "Plan", "general-purpose", …). Cleared per turn.
   */
  private agentTypeById: Map<string, string> = new Map();
  /**
   * Last `contextWindow` observed from the SDK's `result.modelUsage`.
   * Cached across turns so mid-turn `turn_usage` events on the next
   * turn can carry the window without waiting for that turn's own
   * `result`. When unset (first turn of a fresh bridge), the client
   * falls back to the provider-declared window from
   * `ProviderModel.contextWindow`.
   */
  private lastContextWindow: number | null = null;

  createSession(cwd: string, model?: string, resumeSessionId?: string): string {
    this.cwd = cwd;
    this.model = model;
    // Hydrate the SDK resume id from persisted state when zenui restarts or
    // this bridge is a fresh respawn. The SDK's `resume:` option on the next
    // send_prompt picks this up and replays the prior conversation.
    if (resumeSessionId) {
      this.resumeSessionId = resumeSessionId;
    }
    const sessionId = `claude-sdk-${randomUUID()}`;
    return sessionId;
  }

  /** Mid-session model switch. Updates the model used by subsequent
   *  `query()` calls without restarting the bridge process. */
  setModel(model: string): void {
    this.model = model;
  }

  /**
   * Current Claude SDK session id captured from the most recent query, if any.
   * Round-tripped to the Rust adapter so it can be persisted on
   * `session.provider_state.native_thread_id` for cross-restart resume.
   */
  getResumeSessionId(): string | undefined {
    return this.resumeSessionId;
  }

  /**
   * Send one user message through the session's persistent SDK Query.
   *
   * On first call this opens the Query (with `resume:` if we have a
   * hydrated session id), wires up the streaming-input pump, and pushes
   * the user message onto the input queue. On every subsequent call
   * the same Query and same pump are reused — the message is simply
   * pushed onto the existing queue, preserving full conversation
   * context including any partial tool_use/tool_result pairs from a
   * previously interrupted turn.
   *
   * Returns the assistant's final text for this turn (or
   * `'[interrupted]'` if the user steered before completion).
   *
   * IMPORTANT: `reasoningEffort` and `compactCustomInstructions` take
   * effect at Query-open time only. Per-turn changes are currently
   * ignored by the active Query — switching them requires ending the
   * session. `permissionMode` IS switchable mid-session via the SDK's
   * `setPermissionMode` control.
   */
  async sendPrompt(
    prompt: string,
    permissionMode: SdkPermissionMode,
    reasoningEffort?: ReasoningEffortWire,
    images: Array<{ media_type: string; data_base64: string }> = [],
    compactCustomInstructions?: string,
  ): Promise<string> {
    if (this.turnInProgress) {
      throw new Error('Another turn is already in flight');
    }

    // Per-turn accumulator resets. `lastContextWindow` deliberately
    // persists across turns — the SDK only reports it inside
    // `result.modelUsage`, so holding the previous turn's value lets
    // us populate mid-turn `turn_usage` events on the next turn
    // without waiting for that turn's `result`.
    this.agentModelByType.clear();
    this.observedSubagentIds.clear();
    this.outputTokensTotal = 0;
    this.lastAssistantUsage = undefined;
    this.agentUsage.clear();
    this.agentTypeById.clear();

    // Lazy-open the persistent Query on first sendPrompt, or re-open
    // it if a previous pump error tore it down.
    if (!this.activeQuery) {
      this.openPersistentQuery(
        permissionMode,
        reasoningEffort,
        compactCustomInstructions,
      );
    } else {
      // Query already live. Two per-turn knobs may have changed:
      //
      //   1. `reasoningEffort` or `compactCustomInstructions` — the
      //      SDK has no mid-session setter for these, so a change
      //      forces a close-and-reopen with `resume: resumeSessionId`.
      //      Full conversation history is preserved; only the SDK-
      //      side Query object is recycled.
      //   2. `permissionMode` — has a live SDK setter, apply in
      //      place so we don't pay the reopen cost for the common
      //      case of the user flipping permission mode between turns.
      //
      // If both changed, (1) happens first: the reopen opens the
      // new Query already configured with the requested permission
      // mode, so (2) is skipped.
      const desiredCompact =
        compactCustomInstructions?.trim() || undefined;
      const effortChanged =
        this.queryConfig?.reasoningEffort !== reasoningEffort;
      const compactChanged =
        this.queryConfig?.compactInstructions !== desiredCompact;
      if (effortChanged || compactChanged) {
        await this.reopenQueryForConfigChange(
          permissionMode,
          reasoningEffort,
          compactCustomInstructions,
        );
      } else if (this.livePermissionMode !== permissionMode) {
        // Subsequent turn with a different permission mode. Push the
        // change into the live Query; `setPermissionMode` below also
        // updates `this.livePermissionMode` so canUseTool reads the
        // fresh mode for the upcoming turn.
        try {
          await this.setPermissionMode(permissionMode);
        } catch (err) {
          console.error(
            '[bridge] setPermissionMode before sendPrompt failed (continuing):',
            err,
          );
          this.livePermissionMode = permissionMode;
        }
      }
    }

    // Compose the SDKUserMessage (text + optional images) to push
    // onto the input stream.
    type SdkImageMediaType =
      | 'image/jpeg'
      | 'image/png'
      | 'image/gif'
      | 'image/webp';
    const userMessage: SDKUserMessage = {
      type: 'user' as const,
      message: {
        role: 'user' as const,
        content:
          images.length === 0
            ? prompt
            : [
                { type: 'text' as const, text: prompt },
                ...images.map((img) => ({
                  type: 'image' as const,
                  source: {
                    type: 'base64' as const,
                    media_type: img.media_type as SdkImageMediaType,
                    data: img.data_base64,
                  },
                })),
              ],
      },
      parent_tool_use_id: null,
      session_id: '',
    };

    // Arm the turn-boundary resolver BEFORE pushing so the pump loop
    // can never see a `result` race ahead of us. Only one turn is in
    // flight at a time (guarded above).
    const turnResult = new Promise<string>((resolve, reject) => {
      this.pendingTurn = { resolve, reject };
    });
    this.turnInProgress = true;
    try {
      this.inputQueue!.push(userMessage);
      return await turnResult;
    } finally {
      this.turnInProgress = false;
      this.pendingTurn = undefined;
    }
  }

  /**
   * Open the long-lived SDK Query for this session and start the pump
   * loop that turns SDK messages into stream events + turn-boundary
   * resolutions. Called from `sendPrompt` when `activeQuery` is unset
   * (first turn of the session, or recovery after a pump error).
   */
  private openPersistentQuery(
    initialPermissionMode: SdkPermissionMode,
    reasoningEffort?: ReasoningEffortWire,
    compactCustomInstructions?: string,
  ): void {
    this.livePermissionMode = initialPermissionMode;

    // Persistent canUseTool closure. Reads `this.livePermissionMode`
    // on every invocation so mid-session mode changes (toolbar toggle
    // or ExitPlanMode approval) take effect immediately without
    // reopening the Query.
    const canUseTool: CanUseTool = async (
      toolName: string,
      input: Record<string, unknown>,
    ): Promise<PermissionResult> => {
      // AskUserQuestion is Claude's built-in clarifying-question tool. Route it
      // to the question-dialog UI instead of the permission dialog; the return
      // shape is also different — we must pass back `updatedInput: { questions, answers }`
      // where `answers` is keyed by the question text. See
      // https://code.claude.com/docs/en/agent-sdk/user-input#handle-clarifying-questions
      if (toolName === 'AskUserQuestion') {
        const rawQuestions =
          (input?.questions as AskUserSdkQuestion[] | undefined) ?? [];
        const requestId = randomUUID();
        writeStream({
          event: 'user_question',
          request_id: requestId,
          questions: rawQuestions,
        });
        return new Promise<PermissionResult>((resolve) => {
          pendingQuestions.set(requestId, {
            resolve,
            questions: rawQuestions,
          });
        });
      }

      // Bypass mode: the user explicitly opted out of permission
      // prompting. Resolve every non-question tool call as-is without
      // round-tripping through the host. See the runtime-core safety
      // net which auto-answers any permission_request that slips
      // through for a bypass turn — this short-circuit is the primary
      // fix; the safety net covers other provider adapters.
      if (this.livePermissionMode === 'bypassPermissions') {
        return { behavior: 'allow', updatedInput: input };
      }

      // Auto mode runs its own model-classifier upstream. The SDK
      // only invokes canUseTool for tool calls the classifier isn't
      // confident about — treat them like Default / AcceptEdits and
      // emit a permission_request.
      const requestId = randomUUID();
      writeStream({
        event: 'permission_request',
        request_id: requestId,
        tool_name: toolName,
        input,
        suggested: 'allow',
      });
      return new Promise<PermissionResult>((resolve) => {
        pendingPermissions.set(requestId, { resolve, input });
      });
    };
    this.canUseToolCached = canUseTool;

    // Extended-thinking / effort dial — see the long-form comment in
    // the previous version of sendPrompt for the mapping rationale.
    // In streaming-input mode these are fixed at Query open time; a
    // change requires closing the Query and starting a new session.
    const thinkingConfig = (() => {
      if (reasoningEffort === undefined) return undefined;
      if (reasoningEffort === 'minimal') {
        return { type: 'disabled' as const };
      }
      return { type: 'adaptive' as const };
    })();
    const effortLevel: EffortLevel | undefined =
      reasoningEffort && reasoningEffort !== 'minimal'
        ? reasoningEffort
        : undefined;

    // Plan-mode allowlist: let the model investigate without per-tool
    // prompting. See previous version's comment block for the
    // rationale behind which tools are whitelisted.
    const planModeAllowedTools = [
      'Read',
      'Grep',
      'Glob',
      'WebFetch',
      'WebSearch',
      'TodoWrite',
      'Task',
      'Agent',
    ];

    // Per-session compaction steering. Baked into the Query's
    // systemPrompt at open time — switching mid-session requires a
    // new session.
    const trimmedCompactInstructions = compactCustomInstructions?.trim();
    const compactSystemPrompt: Options['systemPrompt'] | undefined =
      trimmedCompactInstructions
        ? {
            type: 'preset',
            preset: 'claude_code',
            append: `When summarizing prior conversation during compaction, prioritize the following user-supplied guidance: ${trimmedCompactInstructions}`,
          }
        : undefined;

    const options: Options = {
      cwd: this.cwd,
      permissionMode: initialPermissionMode,
      canUseTool,
      includePartialMessages: true,
      promptSuggestions: true,
      hooks: {
        PostCompact: [
          {
            hooks: [
              async (input) => {
                const pc = input as {
                  trigger?: 'manual' | 'auto';
                  compact_summary?: string;
                };
                writeStream({
                  event: 'compact_summary',
                  trigger: pc.trigger ?? 'auto',
                  summary: pc.compact_summary ?? '',
                });
                return {};
              },
            ],
          },
        ],
        SessionStart: [
          {
            hooks: [
              async (input) => {
                const s = input as {
                  source?: 'startup' | 'resume' | 'clear' | 'compact';
                  agent_type?: string;
                  model?: string;
                };
                const parts = [`source=${s.source ?? 'unknown'}`];
                if (s.agent_type) parts.push(`agent=${s.agent_type}`);
                if (s.model) parts.push(`model=${s.model}`);
                writeStream({
                  event: 'info',
                  message: `session_start: ${parts.join(' ')}`,
                });
                return {};
              },
            ],
          },
        ],
        SessionEnd: [
          {
            hooks: [
              async (input) => {
                const s = input as { reason?: string };
                writeStream({
                  event: 'info',
                  message: `session_end: reason=${s.reason ?? 'unknown'}`,
                });
                return {};
              },
            ],
          },
        ],
      },
      ...(initialPermissionMode === 'plan'
        ? { allowedTools: planModeAllowedTools }
        : {}),
      ...(this.model ? { model: this.model } : {}),
      ...(this.resumeSessionId ? { resume: this.resumeSessionId } : {}),
      ...(thinkingConfig ? { thinking: thinkingConfig } : {}),
      ...(effortLevel ? { effort: effortLevel } : {}),
      ...(compactSystemPrompt ? { systemPrompt: compactSystemPrompt } : {}),
      ...(RESOLVED_LOCAL_CLAUDE_PATH
        ? { pathToClaudeCodeExecutable: RESOLVED_LOCAL_CLAUDE_PATH }
        : {}),
    };

    const inputQueue = new PushableAsyncIterable<SDKUserMessage>();
    this.inputQueue = inputQueue;
    const q = query({ prompt: inputQueue, options });
    this.activeQuery = q;
    // Remember what this Query was opened with so the next
    // `sendPrompt` can detect a per-turn config change and trigger a
    // reopen. `trimmedCompactInstructions` is already trimmed/empty-
    // collapsed above, so the stored value compares cleanly.
    this.queryConfig = {
      reasoningEffort,
      compactInstructions: trimmedCompactInstructions || undefined,
    };

    // Background pump: drain SDK messages forever. Every `result`
    // resolves the current turn's `pendingTurn`; the pump then keeps
    // looping for the next turn's messages. A thrown error here tears
    // the Query down; the next sendPrompt re-opens a fresh Query with
    // `resume: resumeSessionId` so context is still preserved.
    this.pumpPromise = (async () => {
      try {
        for await (const message of q) {
          const text = this.handleSdkMessage(message);
          if (text != null) {
            // Turn boundary (result event). Drain any permission /
            // question prompts left dangling by an interrupt — their
            // callers need a terminal answer, and the tool the model
            // had asked about is no longer running.
            drainPendingOnAbort();
            const pending = this.pendingTurn;
            this.pendingTurn = undefined;
            pending?.resolve(text);
          }
        }
        // Iterator ended cleanly (Query closed externally).
        const pending = this.pendingTurn;
        this.pendingTurn = undefined;
        this.activeQuery = undefined;
        this.inputQueue = undefined;
        this.canUseToolCached = undefined;
        this.queryConfig = undefined;
        drainPendingOnAbort();
        pending?.resolve('[interrupted]');
      } catch (err) {
        // Pump-level failure. Tear the Query down and fail the
        // in-flight turn (if any). Next sendPrompt will re-open a
        // fresh Query using the most recent resumeSessionId, so
        // conversation history is preserved across the recovery.
        const pending = this.pendingTurn;
        this.pendingTurn = undefined;
        this.activeQuery = undefined;
        this.inputQueue = undefined;
        this.canUseToolCached = undefined;
        this.queryConfig = undefined;
        drainPendingOnAbort();
        console.error('[bridge] SDK pump error:', err);
        pending?.reject(err);
      }
    })();
  }

  /**
   * Forward a runtime permission-mode change to the in-flight SDK query.
   * No-op if no turn is currently running. The host calls this after a
   * user approves an ExitPlanMode and picks a new mode for the rest of
   * the turn.
   */
  async setPermissionMode(mode: SdkPermissionMode): Promise<void> {
    if (!this.activeQuery) return;
    await this.activeQuery.setPermissionMode(mode);
    // Keep our canUseTool view in sync with the SDK's internal
    // state — without this the bypass short-circuit would still
    // read the stale turn-start mode for the rest of the turn.
    this.livePermissionMode = mode;
  }

  /**
   * Fetch the SDK's per-category context-usage breakdown for the
   * live turn. Returns `null` when no query is active; the Rust
   * caller treats that as "context breakdown not available right
   * now" and the UI falls back to a disabled state. Any real
   * failure (SDK throws) propagates as an exception so the
   * caller can surface a distinct error rather than collapsing
   * it into the no-active-query case.
   */
  async getContextUsage(): Promise<unknown | null> {
    if (!this.activeQuery) return null;
    return await this.activeQuery.getContextUsage();
  }

  /**
   * Gracefully close the persistent Query and the streaming input
   * queue. Used on shutdown / SIGTERM paths. Safe to call when no
   * Query is open (no-op). After close, the next `sendPrompt` will
   * open a fresh Query with `resume: resumeSessionId` — so even a
   * graceful close preserves conversation history.
   */
  async closeSession(): Promise<void> {
    const q = this.activeQuery;
    const queue = this.inputQueue;
    this.activeQuery = undefined;
    this.inputQueue = undefined;
    this.canUseToolCached = undefined;
    this.queryConfig = undefined;
    // Reject any turn pending at close time so the caller doesn't hang.
    const pending = this.pendingTurn;
    this.pendingTurn = undefined;
    pending?.reject(new Error('Session closed'));
    drainPendingOnAbort();
    if (queue) queue.close();
    if (q) {
      try {
        q.close();
      } catch (err) {
        console.error('[bridge] Query.close() during closeSession:', err);
      }
    }
    // Wait for the pump to unwind so we don't race process exit.
    const pump = this.pumpPromise;
    this.pumpPromise = undefined;
    if (pump) {
      try {
        await pump;
      } catch {
        // pump errors are already logged inside the pump.
      }
    }
  }

  /**
   * Close the current persistent Query and reopen a fresh one under
   * new `reasoningEffort` / `compactCustomInstructions`. Conversation
   * history is preserved via `resume: this.resumeSessionId` — from
   * the model's POV nothing happened, only the SDK-side Query object
   * was recycled.
   *
   * Called from `sendPrompt` when the requested config differs from
   * `this.queryConfig`. Serialised inside sendPrompt's single-turn
   * guard (`turnInProgress`), so the pump cannot race the reopen and
   * there is never a `pendingTurn` when we get here.
   */
  private async reopenQueryForConfigChange(
    permissionMode: SdkPermissionMode,
    reasoningEffort: ReasoningEffortWire | undefined,
    compactCustomInstructions: string | undefined,
  ): Promise<void> {
    const pump = this.pumpPromise;
    const q = this.activeQuery;
    const queue = this.inputQueue;
    // Drop all references before closing so the pump's own teardown
    // branch (which also clears these) is a harmless second write.
    this.activeQuery = undefined;
    this.inputQueue = undefined;
    this.canUseToolCached = undefined;
    this.queryConfig = undefined;
    this.pumpPromise = undefined;
    // Defensive: we should never have a pendingTurn here (sendPrompt
    // arms it only AFTER the reopen completes). If one somehow
    // exists, reject it rather than leak the promise.
    if (this.pendingTurn) {
      const pending = this.pendingTurn;
      this.pendingTurn = undefined;
      pending.reject(new Error('Query reopened before turn completed'));
    }
    drainPendingOnAbort();
    if (queue) queue.close();
    if (q) {
      try {
        q.close();
      } catch (err) {
        console.error('[bridge] Query.close() during reopen:', err);
      }
    }
    if (pump) {
      try {
        await pump;
      } catch {
        // pump errors already logged inside the pump.
      }
    }
    // Reopen. `resumeSessionId` is the latest committed id (updated
    // by every `result` event + promoted eagerly on `init` since the
    // streaming-input fix), so the new Query rehydrates the full
    // conversation.
    this.openPersistentQuery(
      permissionMode,
      reasoningEffort,
      compactCustomInstructions,
    );
  }

  /**
   * Translate one SDKMessage into stream events. Returns the final assistant text
   * if this message is a `result` (so the caller can capture canonical output).
   */
  private handleSdkMessage(msg: SDKMessage): string | null {
    // Non-delta SDK message dispatch log. Stream_event messages are
    // high-volume (one per token) so we suppress them to keep the log
    // readable; everything else — assistant, user, result, system,
    // tool_progress, tool_use_summary — gets a single line so the
    // "did a user/tool_result message arrive after the permission
    // answer" question is answerable from the log alone.
    if (msg.type !== 'stream_event') {
      const subtype =
        (msg as { subtype?: string }).subtype !== undefined
          ? `.${(msg as { subtype?: string }).subtype}`
          : '';
      console.error(`[bridge] sdk msg type=${msg.type}${subtype}`);
    }
    switch (msg.type) {
      case 'system': {
        const sub = (msg as { subtype?: string }).subtype;
        if (sub === 'init') {
          const init = msg as { session_id?: string; model?: string };
          if (init.session_id) {
            // Under streaming-input mode the Query is persistent: one
            // init fires when the Query opens, and every subsequent
            // turn commits back to the same SDK session. We can safely
            // promote the init session_id to the resume target right
            // away — if the bridge dies before any turn completes, the
            // worst case on next restart is `resume:<never-used-id>`,
            // which is no worse than a cold start (both yield an empty
            // conversation). See the previous two-phase pending/
            // committed split (pendingInitSessionId) — no longer
            // needed now that interrupt doesn't tear the session down.
            this.resumeSessionId = init.session_id;
            writeStream({ event: 'info', message: `Claude session ${init.session_id}` });
          }
          // Surface the model the SDK actually resolved. If the requested
          // model string is rejected or remapped, this is the only place
          // the truth shows up — the bridge sends what the Rust adapter
          // asked for, but the SDK/CLI may silently substitute.
          writeStream({
            event: 'info',
            message: `Claude model: requested=${this.model ?? '<default>'} resolved=${init.model ?? '<unknown>'}`,
          });
          // Populate the subagent model catalog asynchronously off
          // the query handle. Many agent definitions carry an
          // explicit `model` override (a research agent pinned to
          // Haiku, etc.); we pre-read the map so `subagent_started`
          // can ship the planned model without waiting for the
          // subagent's first assistant message. Fire-and-forget —
          // `subagent_model_observed` is the authoritative signal
          // later, so a late/failed resolution here just means the
          // UI gets the value a moment later.
          if (this.activeQuery) {
            const q = this.activeQuery;
            (async () => {
              try {
                const agents = await q.supportedAgents();
                for (const a of agents) {
                  if (a.model) this.agentModelByType.set(a.name, a.model);
                }
              } catch (err) {
                console.error(
                  `[bridge] supportedAgents() failed during init: ${
                    err instanceof Error ? err.message : String(err)
                  }`,
                );
              }
            })();
          }
          // Emit the resolved model as a structured event too, so the
          // Rust adapter can upgrade `session.summary.model` to match
          // what the SDK actually runs. Without this the UI model
          // selector fails to highlight the active entry whenever the
          // stored value is an alias (`sonnet`) but the dropdown list
          // carries pinned ids (`claude-sonnet-4-5-<date>`) returned
          // by `supportedModels()`.
          if (init.model) {
            writeStream({ event: 'model_resolved', model: init.model });
          }
        }
        // The SDK inserts a `compact_boundary` system message at the
        // point in the stream where older turns were compressed into
        // a summary. The accompanying text lives on the PostCompact
        // hook (registered above); this message only carries metrics
        // + the trigger, which we pair up on the app side.
        if (sub === 'compact_boundary') {
          const cb = msg as {
            compact_metadata?: {
              trigger?: 'manual' | 'auto';
              pre_tokens?: number;
              post_tokens?: number;
              duration_ms?: number;
            };
          };
          const m = cb.compact_metadata ?? {};
          writeStream({
            event: 'compact_boundary',
            trigger: m.trigger ?? 'auto',
            pre_tokens: m.pre_tokens ?? null,
            post_tokens: m.post_tokens ?? null,
            duration_ms: m.duration_ms ?? null,
          });
        }
        // The SDK's memory-recall supervisor surfaces relevant
        // memory files into the turn. `select` mode yields full file
        // bodies (renderers lazy-load from `path`); `synthesize`
        // mode yields a Sonnet-authored paragraph in `content`. We
        // forward whatever shape we receive and let the UI decide
        // how to render each mode.
        if (sub === 'memory_recall') {
          const mr = msg as {
            mode?: 'select' | 'synthesize';
            memories?: Array<{
              path?: string;
              scope?: 'personal' | 'team';
              content?: string;
            }>;
          };
          writeStream({
            event: 'memory_recall',
            mode: mr.mode ?? 'select',
            memories: (mr.memories ?? []).map((x) => ({
              path: x.path ?? '',
              scope: x.scope ?? 'personal',
              content: x.content ?? null,
            })),
          });
        }
        // Turn-phase transitions. The SDK emits `status: null |
        // 'compacting' | 'requesting'`; we map to our coarse
        // TurnPhase enum and forward. Unknown strings fall through
        // to `idle` so the UI's label clears rather than freezing
        // on a phase the frontend doesn't recognise.
        if (sub === 'status') {
          const s = msg as {
            status?: 'compacting' | 'requesting' | null;
          };
          const phase = (() => {
            switch (s.status) {
              case 'compacting':
                return 'compacting';
              case 'requesting':
                return 'requesting';
              default:
                return 'idle';
            }
          })();
          writeStream({ event: 'turn_status', phase });
        }
        // Transient API retry. The SDK auto-retries 5xx / rate-
        // limit responses before giving up; the UI hides the
        // retry behind a banner so the user knows the turn is
        // still alive. `error` is SDKAssistantMessageError —
        // best-effort stringify down to its message if present.
        if (sub === 'api_retry') {
          const ar = msg as {
            attempt?: number;
            max_retries?: number;
            retry_delay_ms?: number;
            error_status?: number | null;
            error?: { message?: string } | string;
          };
          const errorMessage = (() => {
            if (typeof ar.error === 'string') return ar.error;
            if (ar.error && typeof ar.error === 'object' && 'message' in ar.error) {
              return ar.error.message ?? '';
            }
            return '';
          })();
          writeStream({
            event: 'api_retry',
            attempt: ar.attempt ?? 1,
            max_retries: ar.max_retries ?? 0,
            retry_delay_ms: ar.retry_delay_ms ?? 0,
            error_status: ar.error_status ?? null,
            error: errorMessage,
          });
        }
        return null;
      }
      case 'prompt_suggestion': {
        // Predicted next user prompt. SDK emits these after a turn
        // when it has high-confidence predictions (and the
        // `promptSuggestions` option is enabled — which we opt
        // into in Options). Forward verbatim; the frontend renders
        // only the latest one per session as ghost text.
        const ps = msg as { suggestion?: string };
        if (typeof ps.suggestion === 'string' && ps.suggestion.length > 0) {
          writeStream({
            event: 'prompt_suggestion',
            suggestion: ps.suggestion,
          });
        }
        return null;
      }
      case 'tool_progress': {
        // Heartbeat for an in-flight tool call. The SDK emits these
        // periodically while a tool is running (Bash watching stdout,
        // Task subagent ticking, etc.) so the UI can distinguish
        // "still working" from "actually stuck" — without a heartbeat
        // we can only fall back to session-wide silence detection.
        // SDKToolProgressMessage shape (sdk.d.ts:3048):
        //   { type, tool_use_id, tool_name, parent_tool_use_id,
        //     elapsed_time_seconds, task_id?, uuid, session_id }
        // We forward a minimal shape: the call_id (the SDK calls it
        // `tool_use_id`) is the join key against ToolCall in the
        // runtime; tool_name + elapsed are display fluff. Stamp
        // `occurred_at` here in the bridge so the freshness clock
        // ticks against wall time, not event-arrival time at the
        // frontend (which can lag under load).
        const tp = msg as {
          tool_use_id?: string;
          tool_name?: string;
          parent_tool_use_id?: string | null;
          elapsed_time_seconds?: number;
        };
        if (typeof tp.tool_use_id === 'string' && tp.tool_use_id.length > 0) {
          writeStream({
            event: 'tool_progress',
            call_id: tp.tool_use_id,
            tool_name: tp.tool_name ?? '',
            parent_call_id: tp.parent_tool_use_id ?? null,
            elapsed_time_seconds: tp.elapsed_time_seconds ?? 0,
            occurred_at: new Date().toISOString(),
          });
        }
        return null;
      }
      case 'rate_limit_event': {
        // Claude subscription rate-limit snapshot. Maps
        // Anthropic's bucket names to human-readable labels inside
        // the bridge so the shared provider-api RateLimitInfo stays
        // provider-agnostic. Drops events without a bucket type —
        // those carry no actionable info.
        const rl = msg as {
          rate_limit_info: {
            status: 'allowed' | 'allowed_warning' | 'rejected';
            rateLimitType?:
              | 'five_hour'
              | 'seven_day'
              | 'seven_day_opus'
              | 'seven_day_sonnet'
              | 'overage';
            utilization?: number;
            resetsAt?: number;
            isUsingOverage?: boolean;
          };
        };
        const info = rl.rate_limit_info;
        if (!info.rateLimitType || info.utilization == null) return null;
        const labels: Record<string, string> = {
          five_hour: '5-hour limit',
          seven_day: 'Weekly · all models',
          seven_day_opus: 'Weekly · Opus',
          seven_day_sonnet: 'Weekly · Sonnet',
          overage: 'Overage',
        };
        writeStream({
          event: 'rate_limit_update',
          rate_limit_info: {
            bucket: info.rateLimitType,
            label: labels[info.rateLimitType] ?? info.rateLimitType,
            status: info.status,
            utilization: info.utilization,
            resetsAt: info.resetsAt ?? null,
            isUsingOverage: info.isUsingOverage ?? false,
          },
        });
        return null;
      }
      case 'stream_event': {
        // Incremental token streaming. With `includePartialMessages: true`, the SDK
        // emits Anthropic raw stream events. We forward `content_block_delta` chunks
        // as small text/reasoning deltas so the UI updates token-by-token.
        const sm = msg as unknown as {
          event: Record<string, unknown>;
          parent_tool_use_id?: string | null;
        };
        const ev = sm.event ?? {};
        const evType = ev.type as string | undefined;
        const parentId = sm.parent_tool_use_id ?? null;
        if (evType === 'content_block_delta') {
          const delta = ev.delta as Record<string, unknown> | undefined;
          const dType = delta?.type as string | undefined;
          if (dType === 'text_delta') {
            const text = (delta?.text as string) ?? '';
            if (text.length > 0) {
              if (parentId) {
                writeStream({
                  event: 'subagent_event',
                  agent_id: parentId,
                  nested_event: { role: 'assistant', text },
                });
              } else {
                writeStream({ event: 'text_delta', delta: text });
              }
            }
          } else if (dType === 'thinking_delta') {
            const thinking = (delta?.thinking as string) ?? '';
            if (thinking.length > 0) {
              writeStream({ event: 'reasoning_delta', delta: thinking });
            }
          }
        }
        return null;
      }
      case 'assistant': {
        const m = msg as unknown as {
          message: {
            content: unknown;
            model?: string;
            usage?: {
              input_tokens?: number;
              output_tokens?: number;
              cache_creation_input_tokens?: number | null;
              cache_read_input_tokens?: number | null;
            };
          };
          parent_tool_use_id?: string | null;
        };
        const rawContent = m.message?.content;
        if (!Array.isArray(rawContent)) {
          console.error(
            `[bridge] assistant message content is not an array: type=${typeof rawContent}`,
          );
          return null;
        }
        // Assistant messages from sub-agents (spawned via the Task/Agent
        // tool) carry a non-null parent_tool_use_id pointing back at the
        // spawner's call_id. Propagate it onto every tool_started we
        // forward from this message so the frontend can group tool calls
        // by the agent that actually ran them.
        const parentToolUseId = m.parent_tool_use_id ?? undefined;
        // Per-agent token accumulation — runs for BOTH main-agent and
        // subagent messages. We key the bucket by parent_tool_use_id
        // (or the literal "main" for top-level messages) and sum the
        // per-call `message.usage` slice into it. The final `turn_usage`
        // emitted from the `result` handler surfaces these buckets as
        // `usage.agents[]`, which the host's usage store persists so
        // the dashboard can break down cost/tokens per agent rather
        // than only showing the SDK-aggregate total.
        //
        // Summing per-message is the right semantics here (even for
        // cache_read, which is problematic for the LIVE context-window
        // indicator): we want billable totals per agent, and the SDK
        // bills every API call's cache read separately. That matches
        // the scope of `r.usage.*` at turn end, so the sum across
        // `agents[]` reconciles with the top-level aggregate and the
        // dashboard can split `total_cost_usd` proportionally without
        // drift.
        const agentMessageUsage = m.message?.usage;
        if (agentMessageUsage && typeof agentMessageUsage === 'object') {
          const bucketKey = parentToolUseId ?? 'main';
          const existing = this.agentUsage.get(bucketKey) ?? {
            agentId: parentToolUseId ?? null,
            agentType: parentToolUseId
              ? this.agentTypeById.get(parentToolUseId) ?? null
              : null,
            model: null,
            inputTokens: 0,
            outputTokens: 0,
            cacheReadTokens: 0,
            cacheWriteTokens: 0,
          };
          existing.inputTokens += agentMessageUsage.input_tokens ?? 0;
          existing.outputTokens += agentMessageUsage.output_tokens ?? 0;
          existing.cacheReadTokens +=
            agentMessageUsage.cache_read_input_tokens ?? 0;
          existing.cacheWriteTokens +=
            agentMessageUsage.cache_creation_input_tokens ?? 0;
          // First non-empty model wins; later messages in the same
          // agent carry the same value. For subagents this is the
          // SDK's runtime-resolved model (authoritative). For main it
          // matches `lastAssistantUsage.model` below.
          const msgModel = m.message?.model;
          if (!existing.model && msgModel) {
            existing.model = msgModel;
          }
          // Backfill agent_type when `subagent_started` races after
          // this message (rare but possible — the order isn't strictly
          // guaranteed by the SDK).
          if (!existing.agentType && parentToolUseId) {
            existing.agentType =
              this.agentTypeById.get(parentToolUseId) ?? null;
          }
          this.agentUsage.set(bucketKey, existing);
        }
        // Per-call token usage for the MAIN-AGENT context-window
        // indicator only. Only top-level assistant messages count
        // toward the main turn's numerator — subagent calls have
        // their own context window (tracked separately by the SDK
        // via modelUsage) and lumping them in inflates the display
        // on Task-heavy turns.
        //
        // Fields below use per-call semantics: input/cache/creation
        // are from THIS API call (the "current prompt size"),
        // output_tokens accumulates across the turn because each
        // call only reports its own slice. The SDK's `result.usage`
        // sums all four across every call — using its cache_read
        // would re-count the same cached prompt once per iteration
        // of the tool loop, which is how the "51M / 1M" bug used
        // to reach 50×.
        //
        // These values do NOT land in the usage dashboard. The
        // FINAL `turn_usage` event emitted from the `result` handler
        // below overrides these with SDK-aggregate totals (including
        // subagents) before `TurnRecord.usage` is persisted, so the
        // dashboard's tokens and cost share a scope. See the comment
        // block at the `result` handler for details.
        const msgUsage = m.message?.usage;
        if (!parentToolUseId && msgUsage && typeof msgUsage === 'object') {
          const inputTokens = msgUsage.input_tokens ?? 0;
          const outputDelta = msgUsage.output_tokens ?? 0;
          const cacheWrite = msgUsage.cache_creation_input_tokens ?? null;
          const cacheRead = msgUsage.cache_read_input_tokens ?? null;
          const observedMainModel = m.message?.model;
          this.outputTokensTotal += outputDelta;
          this.lastAssistantUsage = {
            inputTokens,
            cacheWriteTokens: cacheWrite,
            cacheReadTokens: cacheRead,
            model: observedMainModel,
          };
          // Snapshot of what's actually filling the model's context
          // RIGHT NOW: this API call's prompt + the running output.
          // Distinct from the SDK-aggregate sum we'll emit at result
          // time (which counts the cached system prompt once per
          // tool-loop iteration). Carrying both lets the dashboard
          // show billable totals while the live indicator shows
          // current context fill. See TokenUsage.live_context_tokens.
          const liveContextTokens =
            inputTokens +
            this.outputTokensTotal +
            (cacheRead ?? 0) +
            (cacheWrite ?? 0);
          writeStream({
            event: 'turn_usage',
            usage: {
              inputTokens,
              outputTokens: this.outputTokensTotal,
              cacheWriteTokens: cacheWrite,
              cacheReadTokens: cacheRead,
              // contextWindow only surfaces in `result.modelUsage`;
              // reuse the last seen value on subsequent turns.
              // First-turn mid-stream emits carry null and the
              // client falls back to the provider-declared window.
              contextWindow: this.lastContextWindow,
              liveContextTokens,
              // Cost and duration are only authoritative at turn end.
              totalCostUsd: null,
              durationMs: null,
              model: observedMainModel ?? null,
            },
          });
        }
        // Subagent model observation: the very first assistant
        // message from a subagent carries the resolved model in
        // `message.model`. That's the authoritative signal (beats
        // the planned catalog value from supportedAgents() because
        // the SDK may override or fail-over at runtime). Dedupe
        // per-subagent so we only fire once per agent_id — later
        // assistant messages carry the same model and would
        // otherwise spam the event stream.
        const observedModel = m.message?.model;
        if (
          parentToolUseId &&
          observedModel &&
          !this.observedSubagentIds.has(parentToolUseId)
        ) {
          this.observedSubagentIds.add(parentToolUseId);
          writeStream({
            event: 'subagent_model_observed',
            agent_id: parentToolUseId,
            model: observedModel,
          });
        }
        const blockTypes = rawContent
          .map((b) => (b as { type?: string }).type ?? '?')
          .join(',');
        console.error(
          `[bridge] assistant blocks=[${blockTypes}] parent=${parentToolUseId ?? '-'}`,
        );
        // Text and thinking blocks were already streamed via `stream_event`, so
        // skip them here to avoid duplicating the full message body. We still
        // process `tool_use` blocks because those only arrive complete.
        for (const block of rawContent as Array<Record<string, unknown>>) {
          const t = block.type as string;
          if (t === 'tool_use') {
            const callId = block.id as string | undefined;
            const name = block.name as string | undefined;
            // Anthropic guarantees both fields on tool_use blocks, but
            // skipping defensively keeps a corrupted SDK message from
            // propagating empty IDs that break tool result correlation
            // downstream. Log so the malformed block is visible in debug.
            if (!callId || !name) {
              console.error(
                `[bridge] skipping malformed tool_use block (id=${callId} name=${name})`,
              );
              continue;
            }
            const input = (block.input as Record<string, unknown>) ?? {};
            writeStream({
              event: 'tool_started',
              call_id: callId,
              name,
              args: input,
              ...(parentToolUseId ? { parent_call_id: parentToolUseId } : {}),
            });

            // Structured file-change extraction for Write/Edit/Delete tools.
            if (name === 'Write') {
              writeStream({
                event: 'file_change',
                call_id: callId,
                path: (input.file_path as string) ?? '',
                operation: 'write',
                after: (input.content as string) ?? '',
              });
            } else if (name === 'Edit') {
              writeStream({
                event: 'file_change',
                call_id: callId,
                path: (input.file_path as string) ?? '',
                operation: 'edit',
                before: (input.old_string as string) ?? '',
                after: (input.new_string as string) ?? '',
              });
            }

            // Subagent dispatch
            if (name === 'Task' || name === 'Agent') {
              const agentType =
                (input.subagent_type as string) ?? 'general-purpose';
              // Planned model: the static agent catalog value when
              // the SDK exposed one. Falls back to the main-agent
              // model on the Rust side for display. The live
              // resolved value will overwrite via
              // `subagent_model_observed` from the subagent's first
              // assistant message.
              const plannedModel = this.agentModelByType.get(agentType);
              // Remember the agent's catalog type so the per-agent
              // usage accumulator (above, in the assistant handler)
              // can stamp `agent_type` on this subagent's bucket when
              // its first assistant message lands. The SDK guarantees
              // `subagent_started` forwards before any of that
              // subagent's assistant messages, so the lookup hits.
              this.agentTypeById.set(callId, agentType);
              writeStream({
                event: 'subagent_started',
                parent_call_id: callId,
                agent_id: callId,
                agent_type: agentType,
                prompt: (input.prompt as string) ?? '',
                ...(plannedModel ? { model: plannedModel } : {}),
              });
            }

            // Plan mode tools: ExitPlanMode emits the plan,
            // EnterPlanMode signals the model wants plan mode.
            if (name === 'ExitPlanMode') {
              const raw = (input.plan as string) ?? '';
              writeStream({
                event: 'plan_proposed',
                plan_id: callId,
                title: 'Proposed plan',
                steps: parsePlanSteps(raw),
                raw,
              });
            }
            if (name === 'EnterPlanMode') {
              writeStream({
                event: 'plan_mode_entered',
                call_id: callId,
              });
            }
          }
        }
        return null;
      }
      case 'user': {
        const m = msg as unknown as {
          message: { content: unknown };
          parent_tool_use_id?: string | null;
        };
        const rawContent = m.message?.content;
        if (!Array.isArray(rawContent)) {
          console.error(
            `[bridge] user message content is not an array: type=${typeof rawContent}`,
          );
          return null;
        }
        const blockSummary = rawContent
          .map((b) => {
            const bb = b as { type?: string; tool_use_id?: string };
            return bb.type === 'tool_result'
              ? `tool_result(${bb.tool_use_id ?? '?'})`
              : (bb.type ?? '?');
          })
          .join(',');
        console.error(`[bridge] user blocks=[${blockSummary}]`);
        for (const block of rawContent as Array<Record<string, unknown>>) {
          if (block.type === 'tool_result') {
            const cid = (block.tool_use_id as string) ?? '';
            const raw = block.content as unknown;
            const output =
              typeof raw === 'string' ? raw : JSON.stringify(raw);
            const isError = (block.is_error as boolean) === true;
            writeStream({
              event: 'tool_completed',
              call_id: cid,
              output,
              ...(isError ? { error: 'tool returned an error' } : {}),
            });

            // If this user message is nested under a parent tool, mark subagent completion.
            if (m.parent_tool_use_id) {
              writeStream({
                event: 'subagent_completed',
                agent_id: m.parent_tool_use_id,
                output,
                ...(isError ? { error: 'tool error' } : {}),
              });
            }
          }
        }
        return null;
      }
      case 'result': {
        const r = msg as {
          subtype: string;
          result?: string;
          session_id?: string;
          usage?: {
            input_tokens: number;
            output_tokens: number;
            cache_creation_input_tokens?: number | null;
            cache_read_input_tokens?: number | null;
          };
          modelUsage?: Record<
            string,
            { contextWindow?: number; costUSD?: number }
          >;
          total_cost_usd?: number;
          duration_ms?: number;
        };
        // Each turn's result carries the current committed session id.
        // Overwrite resumeSessionId unconditionally — even if the turn
        // was interrupted, the SDK's session store reflects whatever
        // tool_use/tool_result pairs completed before the interrupt.
        // Resuming from this id on a future bridge restart replays
        // exactly that state.
        if (r.session_id) {
          this.resumeSessionId = r.session_id;
        }
        // Forward token usage before returning the output text so the
        // runtime-core drain loop sees a TurnUsage event before the
        // turn finalises. Picks the first key in modelUsage as the
        // source of truth for contextWindow — a single Flowstate turn
        // only runs on one model at a time.
        //
        // Scope: this is the FINAL `turn_usage` of the turn and the
        // one that lands in `TurnRecord.usage` (last-writer-wins in
        // runtime-core), so it also feeds the usage dashboard
        // (`usage.sqlite` / per-turn cost + token analytics).
        //
        // Critically, token fields here use `r.usage.*` — the SDK's
        // aggregate across the whole turn, including every subagent
        // Task call. This matches the scope of `r.total_cost_usd`
        // (the SDK sums subagent cost into the result) so the
        // dashboard's cost ÷ tokens ratio is self-consistent. An
        // older version of this code sourced tokens from
        // `lastAssistantUsage` (parent-only, per-call) while cost
        // came from `r.total_cost_usd` (parent + subagents), which
        // under-counted tokens on Task-heavy turns and made
        // $/token analytics look spuriously expensive.
        //
        // The parent-only per-call values are still the right signal
        // for the live context-window indicator — that's emitted
        // separately by the per-assistant-message handler above (it
        // explicitly skips subagent messages via `parentToolUseId`
        // and avoids the `r.usage.cache_read_*` aggregation that
        // caused the "51M / 1M" inflation bug on long tool loops).
        // We only fall back to those fields here if the SDK didn't
        // supply an aggregate (exotic result-only path).
        const modelKey = r.modelUsage
          ? Object.keys(r.modelUsage)[0]
          : undefined;
        const mu = modelKey ? r.modelUsage![modelKey] : undefined;
        const resolvedContextWindow = mu?.contextWindow ?? this.lastContextWindow;
        if (mu?.contextWindow != null) {
          this.lastContextWindow = mu.contextWindow;
        }
        if (this.lastAssistantUsage || r.usage) {
          const last = this.lastAssistantUsage;
          // Preserve the per-call snapshot for the live context
          // indicator. The aggregate fields below are correct for
          // the dashboard (they match `r.total_cost_usd`'s scope),
          // but they overcount cached prompt reads — once per tool
          // loop iteration — so they're the wrong numerator for
          // "context window fill". We carry the parent's last per-
          // call values plus the running output total. Subagents
          // run in their own context windows and don't affect the
          // parent's, so excluding them here is correct.
          const liveContextTokens = last
            ? last.inputTokens +
              this.outputTokensTotal +
              (last.cacheReadTokens ?? 0) +
              (last.cacheWriteTokens ?? 0)
            : null;
          // Per-agent token breakdown: ordered so the main (parent)
          // bucket lands first, then each subagent in dispatch order
          // (insertion order on `agentUsage` — Map preserves it). The
          // host store reads this list to persist one row per agent
          // for the usage dashboard; an empty list means the turn was
          // all parent-agent with no Task dispatches, which the host
          // still renders as a single "main" row.
          const mainBucket = this.agentUsage.get('main');
          const subagentBuckets: typeof mainBucket[] = [];
          for (const [key, bucket] of this.agentUsage) {
            if (key === 'main') continue;
            subagentBuckets.push(bucket);
          }
          const agents = [
            ...(mainBucket ? [mainBucket] : []),
            ...subagentBuckets,
          ].map((b) => ({
            agentId: b!.agentId,
            agentType: b!.agentType,
            model: b!.model,
            inputTokens: b!.inputTokens,
            outputTokens: b!.outputTokens,
            cacheReadTokens: b!.cacheReadTokens,
            cacheWriteTokens: b!.cacheWriteTokens,
          }));
          writeStream({
            event: 'turn_usage',
            usage: {
              inputTokens: r.usage?.input_tokens ?? last?.inputTokens ?? 0,
              outputTokens: r.usage?.output_tokens ?? this.outputTokensTotal,
              cacheWriteTokens:
                r.usage?.cache_creation_input_tokens ?? last?.cacheWriteTokens ?? null,
              cacheReadTokens:
                r.usage?.cache_read_input_tokens ?? last?.cacheReadTokens ?? null,
              contextWindow: resolvedContextWindow,
              liveContextTokens,
              totalCostUsd: r.total_cost_usd ?? null,
              durationMs: r.duration_ms ?? null,
              // Parent model. On Task-heavy turns subagents may have
              // run different models; the `agents` field carries the
              // per-agent model so per-subagent cost attribution stays
              // correct even when this single top-level label doesn't
              // capture every agent's model.
              model: last?.model ?? modelKey ?? null,
              // Per-agent breakdown (main + every Task dispatch).
              // Sums over `agents` equal the turn-level token fields
              // above, so the host store can allocate
              // `totalCostUsd` proportionally across agents without
              // drifting from the SDK-aggregate cost.
              ...(agents.length > 0 ? { agents } : {}),
            },
          });
        }
        if (r.subtype === 'success') {
          return r.result ?? '';
        }
        return null;
      }
      default:
        return null;
    }
  }

  answerPermission(
    requestId: string,
    decision: DecisionString,
    permissionMode?: SdkPermissionMode,
  ): void {
    const p = pendingPermissions.get(requestId);
    // stderr is teed into the Rust daemon log via the adapter's stderr
    // reader, so this single line is the authoritative "did the bridge
    // receive the answer for this request_id" signal when diagnosing a
    // stuck-pending tool card.
    console.error(
      `[bridge] answer_permission request_id=${requestId} decision=${decision} mode=${permissionMode ?? '-'} found=${!!p}`,
    );
    if (!p) return;
    pendingPermissions.delete(requestId);
    const allow = decision === 'allow' || decision === 'allow_always';
    if (allow) {
      // Echo the original input — passing {} would replace the tool's
      // args with an empty object and crash inside the tool handler.
      // If a mode override was supplied (used by the host's plan-exit
      // approve flow), include it in updatedPermissions so the SDK
      // applies the mode change as part of accepting the tool call.
      // Without this, switching mode after an ExitPlanMode approval
      // doesn't make the model continue executing in the new mode
      // within the same turn — the SDK's plan-mode constraints win.
      const result: PermissionResult = {
        behavior: 'allow',
        updatedInput: p.input,
      };
      if (permissionMode) {
        (result as PermissionResult & {
          updatedPermissions?: Array<{
            type: 'setMode';
            mode: SdkPermissionMode;
            destination: 'session';
          }>;
        }).updatedPermissions = [
          { type: 'setMode', mode: permissionMode, destination: 'session' },
        ];
        // Mirror the mode change into our canUseTool view. The SDK
        // applies `updatedPermissions.setMode` internally at the
        // moment this promise resolves, but it has no getter we can
        // poll — without shadowing it here the next tool invocation
        // would see the stale turn-start mode and fail the bypass
        // short-circuit. This is the exact bug the user reported:
        // ExitPlanMode → Bypass Permissions, then the next Bash
        // still prompts.
        this.livePermissionMode = permissionMode;
      }
      p.resolve(result);
    } else {
      p.resolve({ behavior: 'deny', message: 'User denied' });
    }
  }

  answerQuestion(requestId: string, answers: StructuredAnswer[]): void {
    const pending = pendingQuestions.get(requestId);
    if (!pending) return;
    pendingQuestions.delete(requestId);

    // Claude expects `updatedInput: { questions, answers: { "<question text>": "<value>" } }`.
    // Rust emits `questionId` as `q{i}` (shared format with the Claude-CLI adapter)
    // but older builds emitted a bare integer; accept either shape so we don't
    // break during a staggered rollout.
    const answerMap: Record<string, string> = {};
    for (const a of answers) {
      const raw = a.questionId.startsWith('q') ? a.questionId.slice(1) : a.questionId;
      const idx = Number(raw);
      const q = pending.questions[idx];
      if (!q) continue;
      answerMap[q.question] = a.answer;
    }

    pending.resolve({
      behavior: 'allow',
      updatedInput: {
        questions: pending.questions,
        answers: answerMap,
      },
    });
  }

  cancelQuestion(requestId: string): void {
    const pending = pendingQuestions.get(requestId);
    if (!pending) return;
    pendingQuestions.delete(requestId);
    // Resolve the pending canUseTool promise with deny so the SDK reports the
    // tool call as user-denied. The model sees the message string and can
    // proceed without the clarifying answer.
    pending.resolve({
      behavior: 'deny',
      message: 'User cancelled the clarifying question',
    });
  }

  /**
   * Stop the currently-running assistant turn without closing the
   * session. The SDK's `Query.interrupt()` unwinds the in-flight turn
   * cooperatively — tool_use/tool_result pairs that already completed
   * stay committed to the session, a `result` event is emitted (which
   * the pump treats as the turn boundary and resolves `pendingTurn`),
   * and the Query stays alive waiting for the next user message.
   *
   * This is THE fix for the steering context-loss bug: the old
   * implementation called `abortController.abort()`, which tore the
   * whole Query down and discarded the uncommitted SDK session —
   * meaning the next turn resumed from before the interrupted turn's
   * tools ran. With `Query.interrupt()`, everything the assistant did
   * up to the interrupt point remains visible to the model on the
   * next turn.
   */
  interrupt(): void {
    // Always drain pending permission / question prompts so their
    // resolvers unwind even if the SDK takes a while to emit the
    // result event. The UI dismisses the dialog immediately either
    // way.
    drainPendingOnAbort();
    if (!this.activeQuery || !this.turnInProgress) {
      return;
    }
    // Fire-and-forget — we don't await because `interrupt()` resolves
    // asynchronously after the SDK flushes state, and the pump loop
    // is already awaiting the next message. Any error surfaces via
    // the pump's catch.
    this.activeQuery.interrupt().catch((err) => {
      console.error('[bridge] Query.interrupt() failed:', err);
    });
  }

  /**
   * List the models the Claude Agent SDK reports as supported. Cheapest path:
   * call query() with a noop prompt, abort immediately, then read
   * supportedModels() — internally that just returns the cached init response.
   *
   * Forwards every capability flag the SDK's `ModelInfo` exposes
   * (`supportsEffort`, `supportedEffortLevels`, `supportsAdaptiveThinking`,
   * `supportsAutoMode`) so the UI can gate per-model affordances without
   * hardcoding model names on the frontend.
   */
  async listModels(): Promise<
    Array<{
      value: string;
      label: string;
      supportsEffort: boolean;
      supportedEffortLevels: string[];
      supportsAdaptiveThinking: boolean;
      supportsAutoMode: boolean;
    }>
  > {
    const abortController = new AbortController();
    const q = query({
      prompt: 'noop',
      options: {
        cwd: this.cwd,
        abortController,
        // Same opportunistic local-claude override as the main turn
        // query: use the user's install when present, leave the
        // option unset otherwise.
        ...(RESOLVED_LOCAL_CLAUDE_PATH
          ? { pathToClaudeCodeExecutable: RESOLVED_LOCAL_CLAUDE_PATH }
          : {}),
      },
    });
    try {
      const models = await q.supportedModels();
      return models.map((m) => ({
        value: m.value,
        label: m.displayName ?? m.value,
        supportsEffort: m.supportsEffort ?? false,
        supportedEffortLevels: m.supportedEffortLevels ?? [],
        supportsAdaptiveThinking: m.supportsAdaptiveThinking ?? false,
        supportsAutoMode: m.supportsAutoMode ?? false,
      }));
    } finally {
      try {
        abortController.abort();
      } catch {
        // ignore
      }
    }
  }

  /**
   * Enumerate the slash commands, sub-agents, and MCP servers the
   * Claude Agent SDK reports for a given cwd. Same ephemeral-query
   * trick as `listModels`: spawn with a noop prompt so init fires,
   * read the cached `supportedCommands` / `supportedAgents` /
   * `mcpServerStatus`, then abort. The aborted query doesn't round-trip
   * to the Claude API, so this is cheap enough to call on popup open.
   *
   * `cwd` is explicit (not `this.cwd`) so the caller can probe any
   * session's working directory without first calling `create_session`.
   */
  async listCapabilities(
    cwd: string,
    model?: string,
  ): Promise<{
    commands: Array<{ name: string; description: string; argumentHint?: string }>;
    agents: Array<{ name: string; description: string; model?: string }>;
    mcpServers: Array<{ name: string; status: string; scope?: string; error?: string }>;
  }> {
    const abortController = new AbortController();
    const q = query({
      prompt: 'noop',
      options: {
        cwd,
        abortController,
        ...(model ? { model } : {}),
        ...(RESOLVED_LOCAL_CLAUDE_PATH
          ? { pathToClaudeCodeExecutable: RESOLVED_LOCAL_CLAUDE_PATH }
          : {}),
      },
    });
    try {
      const [commands, agents, mcpServers] = await Promise.all([
        q.supportedCommands(),
        q.supportedAgents(),
        q.mcpServerStatus(),
      ]);
      return {
        commands: commands.map((c) => ({
          name: c.name,
          description: c.description,
          argumentHint: c.argumentHint,
        })),
        agents: agents.map((a) => ({
          name: a.name,
          description: a.description,
          model: a.model,
        })),
        mcpServers: mcpServers.map((m) => ({
          name: m.name,
          status: m.status,
          scope: m.scope,
          error: m.error,
        })),
      };
    } finally {
      try {
        abortController.abort();
      } catch {
        // ignore
      }
    }
  }
}

function parsePlanSteps(raw: string): Array<{ title: string; detail?: string }> {
  if (!raw) return [];
  const lines = raw.split('\n');
  const steps: Array<{ title: string; detail?: string }> = [];
  for (const line of lines) {
    const trimmed = line.trim();
    const match = trimmed.match(/^(?:[-*]|\d+\.)\s+(.*)$/);
    if (match) {
      steps.push({ title: match[1] });
    }
  }
  return steps;
}

async function main(): Promise<void> {
  const bridge = new ClaudeBridge();

  const rl = createInterface({
    input: process.stdin,
    output: process.stdout,
    terminal: false,
  });

  // Send ready signal once the SDK module is loaded.
  writeJson({ type: 'ready' });
  console.error('[claude-bridge] Ready for commands');

  // Track the current send_prompt promise so we can keep accepting `answer_permission`
  // and other messages while it's in flight.
  let promptInFlight: Promise<void> | null = null;

  rl.on('line', (line: string) => {
    let msg: ZenUiMessage;
    try {
      msg = JSON.parse(line) as ZenUiMessage;
    } catch (err) {
      writeJson({
        type: 'error',
        error: `Invalid JSON: ${(err as Error).message}`,
      });
      return;
    }

    switch (msg.type) {
      case 'create_session': {
        const cwd = (msg.cwd as string) ?? process.cwd();
        const model = msg.model as string | undefined;
        const resumeSessionId = msg.resume_session_id as string | undefined;
        try {
          const sessionId = bridge.createSession(cwd, model, resumeSessionId);
          writeJson({ type: 'session_created', session_id: sessionId });
        } catch (err) {
          writeJson({
            type: 'error',
            error: (err as Error).message,
          });
        }
        break;
      }

      case 'send_prompt': {
        const prompt = msg.prompt as string;
        const mode =
          (msg.permission_mode as SdkPermissionMode | undefined) ??
          'acceptEdits';
        const effort = msg.reasoning_effort as
          | ReasoningEffortWire
          | undefined;
        const images = (msg.images as
          | Array<{ media_type: string; data_base64: string }>
          | undefined) ?? [];
        // Optional per-session compaction steering text. Wraps into
        // `Options.systemPrompt = { type: 'preset', preset:
        // 'claude_code', append: ... }` inside sendPrompt. Absent /
        // empty here means "use the default Claude Code preset, no
        // append".
        const compactCustomInstructions = msg.compact_custom_instructions as
          | string
          | undefined;
        promptInFlight = (async () => {
          try {
            const output = await bridge.sendPrompt(
              prompt,
              mode,
              effort,
              images,
              compactCustomInstructions,
            );
            writeJson({
              type: 'response',
              output,
              session_id: bridge.getResumeSessionId() ?? null,
            });
          } catch (err) {
            writeJson({
              type: 'error',
              error: (err as Error).message,
            });
          } finally {
            promptInFlight = null;
          }
        })();
        break;
      }

      case 'answer_permission': {
        const mode = msg.permission_mode as SdkPermissionMode | undefined;
        bridge.answerPermission(
          msg.request_id as string,
          msg.decision as DecisionString,
          mode,
        );
        break;
      }

      case 'answer_question': {
        bridge.answerQuestion(
          msg.request_id as string,
          (msg.answers as StructuredAnswer[]) ?? [],
        );
        break;
      }

      case 'cancel_question': {
        bridge.cancelQuestion(msg.request_id as string);
        break;
      }

      case 'interrupt': {
        bridge.interrupt();
        writeJson({ type: 'interrupted' });
        break;
      }

      case 'set_permission_mode': {
        // Mid-turn permission switch. The Rust runtime sends this when
        // the user picks "Approve & Auto-edit" (etc.) on an ExitPlanMode
        // approval. Map our wire mode names to the SDK's enum.
        const mode = msg.permission_mode as SdkPermissionMode;
        (async () => {
          try {
            await bridge.setPermissionMode(mode);
            writeJson({ type: 'permission_mode_set', mode });
          } catch (err) {
            writeJson({
              type: 'error',
              error: `set_permission_mode failed: ${(err as Error).message}`,
            });
          }
        })();
        break;
      }

      case 'set_model': {
        // Mid-session model switch. Updates the model used by subsequent
        // query() calls. Synchronous — no async work needed.
        const model = msg.model as string;
        bridge.setModel(model);
        writeJson({ type: 'model_set', model });
        break;
      }

      case 'list_models': {
        (async () => {
          try {
            const models = await bridge.listModels();
            writeJson({ type: 'models', models });
          } catch (err) {
            writeJson({
              type: 'error',
              error: `list_models failed: ${(err as Error).message}`,
            });
          }
        })();
        break;
      }

      case 'get_context_usage': {
        // Mid-turn RPC: call `query.getContextUsage()` on the live
        // Query. The method is only available while a turn is in
        // flight; outside that window we reply with an error so
        // the Rust caller can surface "feature unavailable right
        // now" instead of hanging on a never-resolving oneshot.
        // The Rust adapter already gate-checks via
        // `session_stdin()` (no bridge = Ok(None)) but the bridge
        // can still receive the request during a narrow window
        // between turns.
        const requestId = msg.request_id as string;
        (async () => {
          try {
            const raw = await bridge.getContextUsage();
            if (raw == null) {
              writeJson({
                type: 'rpc_response',
                request_id: requestId,
                kind: 'context_usage',
                error: 'no active query',
              });
            } else {
              writeJson({
                type: 'rpc_response',
                request_id: requestId,
                kind: 'context_usage',
                payload: raw,
              });
            }
          } catch (err) {
            writeJson({
              type: 'rpc_response',
              request_id: requestId,
              kind: 'context_usage',
              error: `get_context_usage failed: ${(err as Error).message}`,
            });
          }
        })();
        break;
      }

      case 'list_capabilities': {
        // Enumerate slash commands / sub-agents / MCP servers for a
        // given cwd. Called from the Rust adapter's
        // session_command_catalog override. Independent of any active
        // session — we spawn an ephemeral query, read init, abort.
        const cwd = (msg.cwd as string) ?? process.cwd();
        const model = msg.model as string | undefined;
        (async () => {
          try {
            const caps = await bridge.listCapabilities(cwd, model);
            writeJson({
              type: 'capabilities',
              commands: caps.commands,
              agents: caps.agents,
              mcp_servers: caps.mcpServers,
            });
          } catch (err) {
            writeJson({
              type: 'error',
              error: `list_capabilities failed: ${(err as Error).message}`,
            });
          }
        })();
        break;
      }

      case 'shutdown': {
        console.error('[claude-bridge] Shutdown requested');
        process.exit(0);
        break;
      }

      default:
        writeJson({
          type: 'error',
          error: `Unknown message type: ${msg.type}`,
        });
    }
  });

  rl.on('close', async () => {
    if (promptInFlight) {
      try {
        await promptInFlight;
      } catch {
        // ignore
      }
    }
    process.exit(0);
  });
}

main().catch((err) => {
  console.error('[claude-bridge] Fatal error:', err);
  process.exit(1);
});
